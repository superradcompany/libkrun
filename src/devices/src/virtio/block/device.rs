// Copyright 2020 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0
//
// Portions Copyright 2017 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the THIRD-PARTY file.

use std::cmp;
use std::convert::From;
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
#[cfg(target_os = "linux")]
use std::os::linux::fs::MetadataExt;
#[cfg(target_os = "macos")]
use std::os::macos::fs::MetadataExt;
#[cfg(target_os = "linux")]
use std::os::unix::io::AsRawFd;
use std::path::PathBuf;
use std::result;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use imago::{
    file::File as ImagoFile, qcow2::Qcow2, raw::Raw, vmdk::Vmdk, DynStorage, FormatDriverBuilder,
    PermissiveImplicitOpenGate, Storage, StorageOpenOptions, SyncFormatAccess,
};
use log::{error, warn};
use utils::eventfd::{EventFd, EFD_NONBLOCK};
use utils::metrics::BlockMetricsWriter;
use virtio_bindings::{
    virtio_blk::*, virtio_config::VIRTIO_F_VERSION_1, virtio_ring::VIRTIO_RING_F_EVENT_IDX,
};
#[cfg(windows)]
use vm_memory::VolatileSlice;
use vm_memory::{ByteValued, GuestMemoryMmap};

#[cfg(windows)]
use super::windows::{
    PendingWindowsRawFileOperation, WindowsRawFile, WindowsRawFileBuffer, WindowsRawFileCompletion,
};
use super::worker::BlockWorker;
#[cfg(target_os = "linux")]
use super::writeback::{BufferedWritebackConfig, BufferedWritebackController};
use super::{
    super::{ActivateResult, DeviceQueue, DeviceState, QueueConfig, VirtioDevice, TYPE_BLOCK},
    Error, NUM_QUEUES, QUEUE_CONFIG, SECTOR_SHIFT, SECTOR_SIZE,
};

use crate::virtio::{
    block::{ImageType, SyncMode},
    ActivateError, InterruptTransport,
};

/// Configuration options for disk caching.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum CacheType {
    /// Flushing mechanic will be advertised to the guest driver, but
    /// the operation will be a noop.
    #[default]
    Unsafe,
    /// Flushing mechanic will be advertised to the guest driver and
    /// flush requests coming from the guest will be performed using
    /// `fsync`.
    Writeback,
}

impl CacheType {
    /// Picks the appropriate cache type based on disk image or device path.
    /// Special files like `/dev/rdisk*` on macOS do not support flush/sync.
    pub fn auto(_path: &str) -> CacheType {
        #[cfg(target_os = "macos")]
        if _path.starts_with("/dev/rdisk") {
            return CacheType::Unsafe;
        }
        CacheType::Writeback
    }
}

/// Helper object for setting up all `Block` fields derived from its backing file.
pub(crate) struct DiskProperties {
    cache_type: CacheType,
    pub(crate) file: Arc<Mutex<SyncFormatAccess<Box<dyn DynStorage>>>>,
    #[cfg(target_os = "linux")]
    writeback_controller: Option<Mutex<BufferedWritebackController>>,
    #[cfg(windows)]
    windows_raw_file: Option<Arc<WindowsRawFile>>,
    #[cfg(windows)]
    pub(crate) windows_formatted_io_runtime: tokio::runtime::Runtime,
    nsectors: u64,
    image_id: Vec<u8>,
}

impl DiskProperties {
    pub fn new(
        disk_image: Arc<Mutex<SyncFormatAccess<Box<dyn DynStorage>>>>,
        disk_image_id: Vec<u8>,
        cache_type: CacheType,
    ) -> io::Result<Self> {
        let disk_size = disk_image.lock().unwrap().size();

        // We only support disk size, which uses the first two words of the configuration space.
        // If the image is not a multiple of the sector size, the tail bits are not exposed.
        if !disk_size.is_multiple_of(SECTOR_SIZE) {
            warn!(
                "Disk size {disk_size} is not a multiple of sector size {SECTOR_SIZE}; \
                 the remainder will not be visible to the guest."
            );
        }

        Ok(Self {
            cache_type,
            nsectors: disk_size >> SECTOR_SHIFT,
            image_id: disk_image_id,
            file: disk_image,
            #[cfg(target_os = "linux")]
            writeback_controller: None,
            #[cfg(windows)]
            windows_raw_file: None,
            #[cfg(windows)]
            windows_formatted_io_runtime: tokio::runtime::Builder::new_current_thread().build()?,
        })
    }

    pub fn nsectors(&self) -> u64 {
        self.nsectors
    }

    pub fn image_id(&self) -> &[u8] {
        &self.image_id
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn build_device_id(disk_file: &File) -> result::Result<String, Error> {
        let blk_metadata = disk_file.metadata().map_err(Error::GetFileMetadata)?;
        // This is how kvmtool does it.
        let device_id = format!(
            "{}{}{}",
            blk_metadata.st_dev(),
            blk_metadata.st_rdev(),
            blk_metadata.st_ino()
        );
        Ok(device_id)
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    fn build_device_id(_disk_file: &File) -> result::Result<String, Error> {
        Err(Error::GetFileMetadata(io::Error::new(
            io::ErrorKind::Unsupported,
            "platform does not expose Unix device metadata",
        )))
    }

    fn build_disk_image_id(disk_file: &File) -> Vec<u8> {
        let mut default_id = vec![0; VIRTIO_BLK_ID_BYTES as usize];
        match Self::build_device_id(disk_file) {
            Err(_) => {
                warn!("Could not generate device id. We'll use a default.");
            }
            Ok(m) => {
                // The kernel only knows to read a maximum of VIRTIO_BLK_ID_BYTES.
                // This will also zero out any leftover bytes.
                let disk_id = m.as_bytes();
                let bytes_to_copy = cmp::min(disk_id.len(), VIRTIO_BLK_ID_BYTES as usize);
                default_id[..bytes_to_copy].clone_from_slice(&disk_id[..bytes_to_copy])
            }
        }
        default_id
    }

    pub fn cache_type(&self) -> CacheType {
        self.cache_type
    }

    pub(crate) fn flush_to_disk(&self) -> io::Result<()> {
        #[cfg(windows)]
        if let Some(raw_file) = &self.windows_raw_file {
            raw_file.flush()?;
        }

        let diskfile = self.file.lock().unwrap();
        diskfile.flush()?;
        diskfile.sync()?;

        #[cfg(target_os = "linux")]
        if let Some(controller) = &self.writeback_controller {
            controller.lock().unwrap().complete_flush();
        }

        Ok(())
    }

    pub(crate) fn record_buffered_write(&self, offset: u64, length: u64) {
        #[cfg(target_os = "linux")]
        if let Some(controller) = &self.writeback_controller {
            controller.lock().unwrap().record_write(offset, length);
        }

        #[cfg(not(target_os = "linux"))]
        let _ = (offset, length);
    }

    #[cfg(target_os = "linux")]
    fn set_writeback_config(&mut self, config: Option<&BufferedWritebackConfig>) {
        self.writeback_controller = config
            .map(BufferedWritebackConfig::controller)
            .map(Mutex::new);
    }

    pub(crate) fn discard_to_any(&self, offset: u64, length: u64) -> io::Result<()> {
        #[cfg(windows)]
        if let Some(raw_file) = &self.windows_raw_file {
            return raw_file.discard_to_any(offset, length);
        }

        let mut diskfile = self.file.lock().unwrap();
        diskfile.discard_to_any(offset, length)
    }

    pub(crate) fn discard_to_zero(&self, offset: u64, length: u64) -> io::Result<()> {
        #[cfg(windows)]
        if let Some(raw_file) = &self.windows_raw_file {
            return raw_file.discard_to_zero(offset, length);
        }

        let mut diskfile = self.file.lock().unwrap();
        diskfile.discard_to_zero(offset, length)
    }

    pub(crate) fn write_zeroes(&self, offset: u64, length: u64) -> io::Result<()> {
        #[cfg(windows)]
        if let Some(raw_file) = &self.windows_raw_file {
            return raw_file.write_zeroes(offset, length);
        }

        let diskfile = self.file.lock().unwrap();
        diskfile.write_zeroes(offset, length)
    }

    #[cfg(windows)]
    fn set_windows_raw_file(&mut self, file: Option<Arc<WindowsRawFile>>) {
        self.windows_raw_file = file;
    }

    #[cfg(windows)]
    pub(crate) fn has_windows_raw_file(&self) -> bool {
        self.windows_raw_file.is_some()
    }

    #[cfg(windows)]
    pub(crate) fn windows_raw_can_submit_direct_buffer(
        &self,
        buffer: WindowsRawFileBuffer,
        offset: u64,
    ) -> bool {
        self.windows_raw_file
            .as_ref()
            .is_some_and(|file| file.can_submit_direct_buffer(buffer, offset))
    }

    #[cfg(windows)]
    pub(crate) fn submit_windows_raw_read_buffer(
        &self,
        buffer: WindowsRawFileBuffer,
        offset: u64,
    ) -> Option<io::Result<PendingWindowsRawFileOperation>> {
        self.windows_raw_file
            .as_ref()
            .map(|file| file.submit_read_buffer(buffer, offset))
    }

    #[cfg(windows)]
    pub(crate) fn submit_windows_raw_write_buffer(
        &self,
        buffer: WindowsRawFileBuffer,
        offset: u64,
    ) -> Option<io::Result<PendingWindowsRawFileOperation>> {
        self.windows_raw_file
            .as_ref()
            .map(|file| file.submit_write_buffer(buffer, offset))
    }

    #[cfg(windows)]
    pub(crate) fn submit_windows_raw_read_bounce(
        &self,
        offset: u64,
        len: usize,
    ) -> Option<io::Result<PendingWindowsRawFileOperation>> {
        self.windows_raw_file
            .as_ref()
            .map(|file| file.submit_read_bounce(offset, len))
    }

    #[cfg(windows)]
    pub(crate) fn submit_windows_raw_write_bounce(
        &self,
        offset: u64,
        buffer: Vec<u8>,
    ) -> Option<io::Result<PendingWindowsRawFileOperation>> {
        self.windows_raw_file
            .as_ref()
            .map(|file| file.submit_write_bounce(offset, buffer))
    }

    #[cfg(windows)]
    pub(crate) fn wait_windows_raw_completion(
        &self,
    ) -> Option<io::Result<WindowsRawFileCompletion>> {
        self.windows_raw_file
            .as_ref()
            .map(|file| file.wait_for_completion())
    }

    #[cfg(windows)]
    pub(crate) fn windows_raw_read_vectored_at_volatile(
        &self,
        bufs: &[VolatileSlice],
        offset: u64,
    ) -> Option<io::Result<usize>> {
        self.windows_raw_file
            .as_ref()
            .map(|file| file.read_vectored_at_volatile(bufs, offset))
    }

    #[cfg(windows)]
    pub(crate) fn windows_raw_write_vectored_at_volatile(
        &self,
        bufs: &[VolatileSlice],
        offset: u64,
    ) -> Option<io::Result<usize>> {
        self.windows_raw_file
            .as_ref()
            .map(|file| file.write_vectored_at_volatile(bufs, offset))
    }
}

impl Drop for DiskProperties {
    fn drop(&mut self) {
        match self.cache_type {
            CacheType::Writeback => {
                if self.flush_to_disk().is_err() {
                    error!("Failed to flush block data on drop.");
                }
            }
            CacheType::Unsafe => {
                // This is a noop.
            }
        };
    }
}

#[derive(Copy, Clone, Debug, Default)]
#[repr(C, packed)]
struct VirtioBlkGeometry {
    cylinders: u16,
    heads: u8,
    sectors: u8,
}

#[derive(Copy, Clone, Debug, Default)]
#[repr(C, packed)]
struct VirtioBlkTopology {
    physical_block_exp: u8,
    alignment_offset: u8,
    min_io_size: u16,
    opt_io_size: u32,
}

#[derive(Copy, Clone, Debug, Default)]
#[repr(C, packed)]
struct VirtioBlkConfig {
    capacity: u64,
    size_max: u32,
    seg_max: u32,
    geometry: VirtioBlkGeometry,
    blk_size: u32,
    topology: VirtioBlkTopology,
    writeback: u8,
    unused0: u8,
    num_queues: u16,
    max_discard_sectors: u32,
    max_discard_seg: u32,
    discard_sector_alignment: u32,
    max_write_zeroes_sectors: u32,
    max_write_zeroes_seg: u32,
    write_zeroes_may_unmap: u8,
}

// Safe because it only has data and has no implicit padding.
unsafe impl ByteValued for VirtioBlkConfig {}

/// Virtio device for exposing block level read/write operations on a host file.
pub struct Block {
    // Host file and properties.
    disk: Option<DiskProperties>,
    cache_type: CacheType,
    disk_image: Arc<Mutex<SyncFormatAccess<Box<dyn DynStorage>>>>,
    disk_image_id: Vec<u8>,
    #[cfg(target_os = "linux")]
    writeback_config: Option<BufferedWritebackConfig>,
    #[cfg(windows)]
    windows_raw_file: Option<Arc<WindowsRawFile>>,
    metrics: BlockMetricsWriter,
    worker_thread: Option<JoinHandle<()>>,
    worker_stopfd: EventFd,

    // Virtio fields.
    pub(crate) avail_features: u64,
    pub(crate) acked_features: u64,
    config: VirtioBlkConfig,

    // Transport related fields.
    pub(crate) device_state: DeviceState,

    // Implementation specific fields.
    pub(crate) id: String,
    pub(crate) partuuid: Option<String>,
}

impl Block {
    /// Create a new virtio block device that operates on the given file.
    ///
    /// The given file must be seekable and sizable.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: String,
        partuuid: Option<String>,
        cache_type: CacheType,
        disk_image_path: String,
        disk_image_format: ImageType,
        is_disk_read_only: bool,
        direct_io: bool,
        sync_mode: SyncMode,
        metrics: BlockMetricsWriter,
    ) -> io::Result<Block> {
        if matches!(disk_image_format, ImageType::Vmdk) && !is_disk_read_only {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "VMDK write support is not available; configure the disk as read-only",
            ));
        }

        let disk_image = OpenOptions::new()
            .read(true)
            .write(!is_disk_read_only)
            .open(PathBuf::from(&disk_image_path))?;

        #[cfg(windows)]
        let windows_raw_file = if matches!(&disk_image_format, ImageType::Raw) {
            Some(Arc::new(WindowsRawFile::open(
                &disk_image_path,
                is_disk_read_only,
                direct_io,
            )?))
        } else {
            None
        };

        // Use the caller-supplied `id` as the virtio-blk disk image id so
        // it surfaces in the guest at `/sys/block/<dev>/serial` and (when
        // udev is available) `/dev/disk/by-id/virtio-<id>`. Falls back to
        // the rdev+inode-derived default if `id` is empty.
        let disk_image_id = if id.is_empty() {
            DiskProperties::build_disk_image_id(&disk_image)
        } else {
            let mut padded = vec![0u8; VIRTIO_BLK_ID_BYTES as usize];
            let bytes = id.as_bytes();
            let n = cmp::min(bytes.len(), padded.len());
            padded[..n].copy_from_slice(&bytes[..n]);
            padded
        };

        #[cfg(target_os = "linux")]
        let writeback_config = if matches!(disk_image_format, ImageType::Raw)
            && !is_disk_read_only
            && !direct_io
            && cache_type == CacheType::Writeback
            && sync_mode != SyncMode::None
        {
            BufferedWritebackConfig::from_environment(Arc::new(disk_image.try_clone()?))
        } else {
            None
        };

        // Keep Imago on its established open path. When advisory writeback is active, procfs
        // provides a race-free name for the already-verified inode without giving Imago the
        // controller's file description or changing its storage implementation.
        #[cfg(target_os = "linux")]
        let imago_path = writeback_config
            .as_ref()
            .map(|_| format!("/proc/self/fd/{}", disk_image.as_raw_fd()))
            .unwrap_or_else(|| disk_image_path.clone());
        #[cfg(not(target_os = "linux"))]
        let imago_path = disk_image_path.clone();

        let file_opts = StorageOpenOptions::new()
            .write(!is_disk_read_only)
            .filename(&imago_path)
            .direct(direct_io);

        #[cfg(target_os = "macos")]
        let file_opts = file_opts.relaxed_sync(sync_mode == SyncMode::Relaxed);

        let file = ImagoFile::open_sync(file_opts)?;
        let discard_alignment = file.discard_align();

        let disk_image = match disk_image_format {
            ImageType::Qcow2 => {
                let mut qcow2 =
                    Qcow2::<Box<dyn DynStorage>, Arc<imago::FormatAccess<_>>>::open_image_sync(
                        Box::new(file),
                        !is_disk_read_only,
                    )?;
                qcow2.open_implicit_dependencies_sync()?;
                SyncFormatAccess::new(qcow2)?
            }
            ImageType::Raw => {
                let raw = Raw::<Box<dyn DynStorage>>::open_image_sync(
                    Box::new(file),
                    !is_disk_read_only,
                )?;
                SyncFormatAccess::new(raw)?
            }
            ImageType::Vmdk => {
                let vmdk = Vmdk::<Box<dyn DynStorage>, Arc<imago::FormatAccess<_>>>::builder(
                    Box::new(file),
                )
                .open_sync(PermissiveImplicitOpenGate::default())?;
                SyncFormatAccess::new(vmdk)?
            }
        };

        let disk_image = Arc::new(Mutex::new(disk_image));

        let disk_properties = {
            let disk_properties =
                DiskProperties::new(disk_image.clone(), disk_image_id.clone(), cache_type)?;
            #[cfg(target_os = "linux")]
            let disk_properties = {
                let mut disk_properties = disk_properties;
                disk_properties.set_writeback_config(writeback_config.as_ref());
                disk_properties
            };
            #[cfg(windows)]
            {
                let mut disk_properties = disk_properties;
                disk_properties.set_windows_raw_file(windows_raw_file.clone());
                disk_properties
            }
            #[cfg(not(windows))]
            {
                disk_properties
            }
        };

        let mut avail_features = (1u64 << VIRTIO_F_VERSION_1)
            | (1u64 << VIRTIO_BLK_F_SEG_MAX)
            | (1u64 << VIRTIO_BLK_F_DISCARD)
            | (1u64 << VIRTIO_BLK_F_WRITE_ZEROES)
            | (1u64 << VIRTIO_RING_F_EVENT_IDX);

        if sync_mode != SyncMode::None {
            avail_features |= 1u64 << VIRTIO_BLK_F_FLUSH;
        }

        if is_disk_read_only {
            avail_features |= 1u64 << VIRTIO_BLK_F_RO;
        };

        let config = VirtioBlkConfig {
            capacity: disk_properties.nsectors(),
            size_max: 0,
            // QUEUE_SIZE - 2
            seg_max: 254,
            max_discard_sectors: u32::MAX,
            max_discard_seg: 1,
            discard_sector_alignment: discard_alignment as u32 / 512,
            max_write_zeroes_sectors: u32::MAX,
            max_write_zeroes_seg: 1,
            write_zeroes_may_unmap: 1,
            ..Default::default()
        };

        Ok(Block {
            id,
            partuuid,
            config,
            disk: Some(disk_properties),
            cache_type,
            disk_image,
            disk_image_id,
            #[cfg(target_os = "linux")]
            writeback_config,
            #[cfg(windows)]
            windows_raw_file,
            metrics,
            avail_features,
            acked_features: 0u64,
            device_state: DeviceState::Inactive,
            worker_thread: None,
            worker_stopfd: EventFd::new(EFD_NONBLOCK)?,
        })
    }

    /// Provides the ID of this block device.
    pub fn id(&self) -> &String {
        &self.id
    }

    /// Provides the PARTUUID of this block device.
    pub fn partuuid(&self) -> Option<&String> {
        self.partuuid.as_ref()
    }

    /// Specifies if this block device is read only.
    pub fn is_read_only(&self) -> bool {
        self.avail_features & (1u64 << VIRTIO_BLK_F_RO) != 0
    }
}

impl VirtioDevice for Block {
    fn device_type(&self) -> u32 {
        TYPE_BLOCK
    }

    fn device_name(&self) -> &str {
        "block"
    }

    fn queue_config(&self) -> &[QueueConfig] {
        &QUEUE_CONFIG
    }

    fn avail_features(&self) -> u64 {
        self.avail_features
    }

    fn acked_features(&self) -> u64 {
        self.acked_features
    }

    fn set_acked_features(&mut self, acked_features: u64) {
        self.acked_features = acked_features;
    }

    fn read_config(&self, offset: u64, mut data: &mut [u8]) {
        let config_slice = self.config.as_slice();
        let config_len = config_slice.len() as u64;
        if offset >= config_len {
            error!("Failed to read config space");
            return;
        }
        if let Some(end) = offset.checked_add(data.len() as u64) {
            // This write can't fail, offset and end are checked against config_len.
            data.write_all(&config_slice[offset as usize..cmp::min(end, config_len) as usize])
                .unwrap();
        }
    }

    fn write_config(&mut self, _offset: u64, _data: &[u8]) {
        error!("Guest attempted to write config");
    }

    fn is_activated(&self) -> bool {
        self.device_state.is_activated()
    }

    fn activate(
        &mut self,
        mem: GuestMemoryMmap,
        interrupt: InterruptTransport,
        queues: Vec<DeviceQueue>,
    ) -> ActivateResult {
        if self.worker_thread.is_some() {
            panic!("virtio_blk: worker thread already exists");
        }

        let [blk_q]: [_; NUM_QUEUES] = queues.try_into().map_err(|_| {
            error!("Cannot perform activate. Expected {} queue(s)", NUM_QUEUES);
            ActivateError::BadActivate
        })?;

        let disk = match self.disk.take() {
            Some(d) => d,
            None => {
                let disk = DiskProperties::new(
                    Arc::clone(&self.disk_image),
                    self.disk_image_id.clone(),
                    self.cache_type,
                )
                .map_err(|_| ActivateError::BadActivate)?;
                #[cfg(target_os = "linux")]
                let disk = {
                    let mut disk = disk;
                    disk.set_writeback_config(self.writeback_config.as_ref());
                    disk
                };
                #[cfg(windows)]
                {
                    let mut disk = disk;
                    disk.set_windows_raw_file(self.windows_raw_file.clone());
                    disk
                }
                #[cfg(not(windows))]
                {
                    disk
                }
            }
        };

        let worker = BlockWorker::new(
            blk_q,
            interrupt.clone(),
            mem.clone(),
            disk,
            self.worker_stopfd.try_clone().unwrap(),
            self.metrics.clone(),
        );
        self.worker_thread = Some(worker.run());

        self.device_state = DeviceState::Activated(mem, interrupt);
        Ok(())
    }

    fn reset(&mut self) -> bool {
        if let Some(worker) = self.worker_thread.take() {
            let _ = self.worker_stopfd.write(1);
            if let Err(e) = worker.join() {
                error!("error waiting for worker thread: {e:?}");
            }
        }
        self.device_state = DeviceState::Inactive;
        true
    }
}

//--------------------------------------------------------------------------------------------------
// Tests
//--------------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use utils::metrics::MetricsWriter;

    use super::*;

    #[test]
    fn writable_vmdk_is_rejected() {
        let result = Block::new(
            "vmdk".to_string(),
            None,
            CacheType::Unsafe,
            "missing.vmdk".to_string(),
            ImageType::Vmdk,
            false,
            false,
            SyncMode::None,
            MetricsWriter::default().register_block_device("vmdk".to_string()),
        );

        let error = match result {
            Ok(_) => panic!("writable VMDK should be rejected"),
            Err(error) => error,
        };

        assert_eq!(error.kind(), io::ErrorKind::Unsupported);
        assert!(error.to_string().contains("VMDK write support"));
    }
}
