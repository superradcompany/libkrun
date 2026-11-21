// Copyright 2020 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0
//
// Portions Copyright 2017 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the THIRD-PARTY file.

#[cfg(target_os = "linux")]
use std::cell::RefCell;
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
use std::sync::atomic::{AtomicBool, Ordering};
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
use super::writeback::{
    BufferedWritebackConfig, BufferedWritebackController, WritebackOutcome, WritebackReservation,
    MINIMUM_WRITEBACK_BUDGET_BYTES,
};
use super::{
    super::{
        ActivateResult, DeviceQueue, DeviceState, QueueConfig, VirtioDevice, VirtioStateError,
        TYPE_BLOCK,
    },
    BlockBackendSpec, Error, PreparedBlockBackend, WritebackLimit, BLOCK_STATE_VERSION, NUM_QUEUES,
    QUEUE_CONFIG, SECTOR_SHIFT, SECTOR_SIZE,
};

use crate::virtio::{
    block::{ImageType, SyncMode},
    ActivateError, InterruptTransport,
};

#[cfg(target_os = "linux")]
const EXPLICIT_ZERO_BUFFER_BYTES: usize = 1024 * 1024;

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
    pub(crate) unavailable: bool,
    cache_type: CacheType,
    read_only: bool,
    pub(crate) file: Arc<Mutex<SyncFormatAccess<Box<dyn DynStorage>>>>,
    #[cfg(target_os = "linux")]
    // One block worker owns each DiskProperties instance. RefCell preserves the `&self` volatile
    // I/O trait while avoiding mutex atomics on every guest write; this must be revisited before
    // increasing NUM_QUEUES or sharing one DiskProperties between workers.
    writeback_controller: Option<RefCell<BufferedWritebackController>>,
    #[cfg(target_os = "linux")]
    // Keep the shared configuration handle so teardown sync results survive the per-activation
    // controller and can fail a later activation closed.
    writeback_config: Option<BufferedWritebackConfig>,
    #[cfg(target_os = "linux")]
    // Bounded mode must account real dirty data, so WRITE_ZEROES and the unaligned edges of an
    // unmapping zero write reuse this buffer instead of Imago's zeroing, which can fall back to
    // unaccounted buffered zero-data writes.
    explicit_zero_buffer: Option<Box<[u8]>>,
    #[cfg(windows)]
    windows_raw_file: Option<Arc<WindowsRawFile>>,
    #[cfg(windows)]
    pub(crate) windows_formatted_io_runtime: tokio::runtime::Runtime,
    nsectors: u64,
    image_id: Vec<u8>,
}

/// An exact mutation chunk paired with an optional Linux writeback reservation.
pub(crate) struct BufferedMutationPlan {
    length: u64,
    #[cfg(target_os = "linux")]
    reservation: Option<WritebackReservation>,
}

impl BufferedMutationPlan {
    pub(crate) fn len(&self) -> u64 {
        self.length
    }
}

impl DiskProperties {
    pub fn new(
        disk_image: Arc<Mutex<SyncFormatAccess<Box<dyn DynStorage>>>>,
        disk_image_id: Vec<u8>,
        cache_type: CacheType,
        read_only: bool,
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
            unavailable: false,
            read_only,
            nsectors: disk_size >> SECTOR_SHIFT,
            image_id: disk_image_id,
            file: disk_image,
            #[cfg(target_os = "linux")]
            writeback_controller: None,
            #[cfg(target_os = "linux")]
            writeback_config: None,
            #[cfg(target_os = "linux")]
            explicit_zero_buffer: None,
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
        // A read-only backend cannot contain guest or device mutations. Treat its durability
        // fence as already satisfied instead of asking platforms such as Windows to flush a
        // handle that intentionally lacks write access.
        if self.read_only {
            return Ok(());
        }

        #[cfg(target_os = "linux")]
        let quiesce_error = self
            .writeback_controller
            .as_ref()
            .and_then(|controller| controller.borrow_mut().quiesce().err());

        #[cfg(windows)]
        if let Some(raw_file) = &self.windows_raw_file {
            raw_file.flush()?;
        }

        // The full Imago flush remains the guest-visible durability fence, so run it even after
        // the background controller fails. A successful later sync cannot erase a writeback error
        // already reported through the file description; that controller remains failed closed.
        let sync_result = {
            let diskfile = self.file.lock().unwrap();
            diskfile.flush().and_then(|_| diskfile.sync())
        };

        #[cfg(target_os = "linux")]
        match &sync_result {
            Ok(()) => {
                let config_healthy = self
                    .writeback_config
                    .as_ref()
                    .is_none_or(BufferedWritebackConfig::record_full_sync_success);
                let controller_healthy = self
                    .writeback_controller
                    .as_ref()
                    .is_none_or(|controller| controller.borrow_mut().reset_after_flush());

                if let Some(error) = quiesce_error {
                    warn!(
                        "Buffered writeback remains permanently failed after the full disk sync: \
                         {error}"
                    );
                    return Err(error);
                }
                if !config_healthy || !controller_healthy {
                    return Err(io::Error::other(
                        "buffered writeback remains permanently failed after the full disk sync",
                    ));
                }
            }
            Err(error) => {
                if let Some(config) = &self.writeback_config {
                    config.record_full_sync_failure(error);
                }
            }
        }

        sync_result
    }

    pub(crate) fn plan_buffered_mutation(
        &self,
        offset: u64,
        requested: u64,
    ) -> io::Result<BufferedMutationPlan> {
        #[cfg(not(target_os = "linux"))]
        let _ = offset;

        #[cfg(target_os = "linux")]
        {
            if requested != 0 {
                if let Some(controller) = &self.writeback_controller {
                    let reservation = controller.borrow_mut().plan_write(offset, requested)?;
                    return Ok(BufferedMutationPlan {
                        length: reservation.len(),
                        reservation: Some(reservation),
                    });
                }
            }
        }

        Ok(BufferedMutationPlan {
            length: requested,
            #[cfg(target_os = "linux")]
            reservation: None,
        })
    }

    pub(crate) fn finish_buffered_mutation(
        &self,
        plan: BufferedMutationPlan,
        operation_result: io::Result<()>,
    ) -> io::Result<()> {
        #[cfg(target_os = "linux")]
        let accounting_result = match plan.reservation {
            Some(reservation) => {
                let outcome = if operation_result.is_ok() {
                    WritebackOutcome::Written(plan.length)
                } else {
                    // Imago may have completed a prefix before returning an error. Charge the
                    // entire reservation conservatively so repeated failing writes cannot bypass
                    // the hard budget.
                    WritebackOutcome::Failed
                };
                self.writeback_controller
                    .as_ref()
                    .expect("reservation requires an active writeback controller")
                    .borrow_mut()
                    .finish_write(reservation, outcome)
            }
            None => Ok(()),
        };

        #[cfg(not(target_os = "linux"))]
        let accounting_result: io::Result<()> = {
            let _ = plan;
            Ok(())
        };

        match operation_result {
            Ok(()) => accounting_result,
            Err(operation_error) => {
                if let Err(accounting_error) = accounting_result {
                    warn!(
                        "Buffered mutation failed and writeback accounting also failed: {accounting_error}"
                    );
                }
                Err(operation_error)
            }
        }
    }

    pub(crate) fn has_writeback_limit(&self) -> bool {
        #[cfg(target_os = "linux")]
        {
            self.writeback_config.is_some()
        }

        #[cfg(not(target_os = "linux"))]
        {
            false
        }
    }

    /// Rejects a complete bounded mutation before any prefix can reach the backing image.
    ///
    /// Legacy disks retain Imago's existing range behavior. Bounded mode is stricter because
    /// truncating a request at the visible edge would violate its all-or-error range contract and
    /// let controller accounting describe a different mutation from the one the guest submitted.
    pub(crate) fn validate_mutation_range(&self, offset: u64, length: u64) -> io::Result<()> {
        if self.has_writeback_limit() {
            Self::validate_range_against_capacity(self.nsectors, offset, length)?;
        }
        Ok(())
    }

    fn validate_range_against_capacity(nsectors: u64, offset: u64, length: u64) -> io::Result<()> {
        let visible_size = nsectors.checked_mul(SECTOR_SIZE).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "visible block capacity overflow",
            )
        })?;
        let end = offset.checked_add(length).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "block mutation range overflow")
        })?;
        if end > visible_size {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "block mutation extends beyond visible capacity",
            ));
        }
        Ok(())
    }

    #[cfg(target_os = "linux")]
    fn set_writeback_config(&mut self, config: Option<&BufferedWritebackConfig>) -> io::Result<()> {
        // Retain the shared health handle before any fallible setup. If controller recovery or
        // buffer allocation fails, DiskProperties::drop can still report its final sync result.
        self.writeback_config = config.cloned();
        let controller = config
            .map(BufferedWritebackConfig::controller)
            .transpose()?
            .map(RefCell::new);
        let explicit_zero_buffer = if config.is_some() {
            let mut buffer = Vec::new();
            buffer
                .try_reserve_exact(EXPLICIT_ZERO_BUFFER_BYTES)
                .map_err(io::Error::other)?;
            buffer.resize(EXPLICIT_ZERO_BUFFER_BYTES, 0);
            Some(buffer.into_boxed_slice())
        } else {
            None
        };

        self.explicit_zero_buffer = explicit_zero_buffer;
        self.writeback_controller = controller;
        Ok(())
    }

    pub(crate) fn discard_to_any(&self, offset: u64, length: u64) -> io::Result<()> {
        self.validate_mutation_range(offset, length)?;

        #[cfg(target_os = "linux")]
        if let Some(controller) = &self.writeback_controller {
            return self.bounded_discard(controller, offset, length, false);
        }

        #[cfg(windows)]
        if let Some(raw_file) = &self.windows_raw_file {
            return raw_file.discard_to_any(offset, length);
        }

        let mut diskfile = self.file.lock().unwrap();
        diskfile.discard_to_any(offset, length)
    }

    pub(crate) fn discard_to_zero(&self, offset: u64, length: u64) -> io::Result<()> {
        self.validate_mutation_range(offset, length)?;

        #[cfg(target_os = "linux")]
        if let Some(controller) = &self.writeback_controller {
            return self.bounded_discard(controller, offset, length, true);
        }

        #[cfg(windows)]
        if let Some(raw_file) = &self.windows_raw_file {
            return raw_file.discard_to_zero(offset, length);
        }

        self.run_buffered_mutation(offset, length, None, |chunk_offset, chunk_length| {
            let mut diskfile = self.file.lock().unwrap();
            diskfile.discard_to_zero(chunk_offset, chunk_length)
        })
    }

    pub(crate) fn write_zeroes(&self, offset: u64, length: u64) -> io::Result<()> {
        self.validate_mutation_range(offset, length)?;

        #[cfg(target_os = "linux")]
        if let Some(zero_buffer) = &self.explicit_zero_buffer {
            return self.run_buffered_mutation(
                offset,
                length,
                Some(zero_buffer.len() as u64),
                |chunk_offset, chunk_length| {
                    let chunk_length = usize::try_from(chunk_length)
                        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
                    let diskfile = self.file.lock().unwrap();
                    diskfile.write(&zero_buffer[..chunk_length], chunk_offset)
                },
            );
        }

        #[cfg(windows)]
        if let Some(raw_file) = &self.windows_raw_file {
            return raw_file.write_zeroes(offset, length);
        }

        self.run_buffered_mutation(offset, length, None, |chunk_offset, chunk_length| {
            let diskfile = self.file.lock().unwrap();
            diskfile.write_zeroes(chunk_offset, chunk_length)
        })
    }

    /// Discards a range under bounded writeback without dirtying unreserved pages.
    ///
    /// Only the interior aligned to the controller's discard alignment is punched, so the kernel
    /// never zeroes a partial block in the page cache. The unaligned edges are left alone for a
    /// plain discard, whose contents are unspecified, and are zeroed through the accounted
    /// explicit-zero writer when the range must read back as zeroes. A host filesystem that
    /// cannot punch turns a plain discard into a no-op and a zeroing discard into accounted zero
    /// writes.
    #[cfg(target_os = "linux")]
    fn bounded_discard(
        &self,
        controller: &RefCell<BufferedWritebackController>,
        offset: u64,
        length: u64,
        zeroes: bool,
    ) -> io::Result<()> {
        let end = offset.checked_add(length).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "block mutation range overflow")
        })?;
        let (aligned_start, aligned_end) = controller.borrow().punchable_range(offset, end);

        if zeroes && offset < aligned_start {
            self.write_zeroes(offset, aligned_start - offset)?;
        }
        if aligned_start < aligned_end {
            // The punch bypasses Imago without taking its lock: this worker is the only thread
            // issuing I/O to the image, and a punch may wait on writeback submission.
            let punched = controller
                .borrow_mut()
                .punch_hole(aligned_start, aligned_end - aligned_start);
            match punched {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::Unsupported => {
                    if zeroes {
                        self.write_zeroes(aligned_start, aligned_end - aligned_start)?;
                    }
                }
                Err(error) => return Err(error),
            }
        }
        if zeroes && aligned_end < end {
            self.write_zeroes(aligned_end, end - aligned_end)?;
        }
        Ok(())
    }

    fn run_buffered_mutation<F>(
        &self,
        offset: u64,
        length: u64,
        maximum_chunk: Option<u64>,
        mut operation: F,
    ) -> io::Result<()>
    where
        F: FnMut(u64, u64) -> io::Result<()>,
    {
        self.validate_mutation_range(offset, length)?;

        let mut chunk_offset = offset;
        let mut remaining = length;

        while remaining != 0 {
            let requested = maximum_chunk.map_or(remaining, |maximum| remaining.min(maximum));
            let plan = self.plan_buffered_mutation(chunk_offset, requested)?;
            let chunk_length = plan.len();
            let operation_result = operation(chunk_offset, chunk_length);
            self.finish_buffered_mutation(plan, operation_result)?;

            remaining -= chunk_length;
            if remaining != 0 {
                chunk_offset = chunk_offset.checked_add(chunk_length).ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidInput, "block mutation range overflow")
                })?;
            }
        }

        Ok(())
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
    unavailable: bool,
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
    worker_thread: Option<JoinHandle<BlockWorker>>,
    worker_stopfd: EventFd,
    // The event wakes a sleeping worker; this flag also closes dequeue admission while the worker
    // is inside an existing request or waiting for an IOCP completion.
    worker_stopping: Arc<AtomicBool>,
    quiesced_queue: Option<DeviceQueue>,

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
    /// Recreate the guest-visible contract while denying all storage I/O.
    /// No source path is accepted or opened. Identity queries remain available.
    pub fn new_unavailable(state: &BlockState, metrics: BlockMetricsWriter) -> io::Result<Self> {
        let size = state
            .capacity_sectors
            .checked_mul(SECTOR_SIZE)
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "block capacity overflow")
            })?;
        let prepared = PreparedBlockBackend::unavailable(size, state.read_only)?;
        let mut block = Self::from_prepared_backend(
            state.id.clone(),
            state.partuuid.clone(),
            state.cache_type,
            state.disk_image_id.clone(),
            prepared,
            metrics,
        )?;
        block
            .restore_state(state)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error.to_string()))?;
        block.unavailable = true;
        block
            .disk
            .as_mut()
            .expect("new block owns its disk")
            .unavailable = true;
        Ok(block)
    }

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
        Self::new_with_writeback_limit(
            id,
            partuuid,
            cache_type,
            disk_image_path,
            disk_image_format,
            is_disk_read_only,
            direct_io,
            sync_mode,
            None,
            metrics,
        )
    }

    /// Creates one virtio-block device over a caller-resolved raw/qcow2 dependency chain.
    ///
    /// Layers are composed base-to-head and header-provided dependency paths are ignored. This is
    /// the preferred constructor when artifacts were resolved by a snapshot or checkpoint store.
    pub fn new_with_backend(
        id: String,
        partuuid: Option<String>,
        cache_type: CacheType,
        backend: BlockBackendSpec,
        metrics: BlockMetricsWriter,
    ) -> io::Result<Block> {
        let head_path = backend
            .layers
            .last()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "missing block head"))?
            .path
            .clone();
        let head_file = OpenOptions::new().read(true).open(&head_path)?;
        let disk_image_id = if id.is_empty() {
            DiskProperties::build_disk_image_id(&head_file)
        } else {
            Self::padded_disk_image_id(&id)
        };
        let prepared = PreparedBlockBackend::open(&backend)?;
        Self::from_prepared_backend(id, partuuid, cache_type, disk_image_id, prepared, metrics)
    }

    /// Create a virtio block device with an optional hard buffered-writeback budget.
    ///
    /// Bounded writeback is supported on Linux for writable raw disks using writeback caching,
    /// buffered I/O and an active sync mode. The configured value is the per-device hard budget;
    /// libkrun derives smaller background batches and releases their credits only after completed
    /// range writeback. This does not replace the full sync that completes a guest flush. A zero
    /// value disables the policy.
    #[allow(clippy::too_many_arguments)]
    pub fn new_with_writeback_limit(
        id: String,
        partuuid: Option<String>,
        cache_type: CacheType,
        disk_image_path: String,
        disk_image_format: ImageType,
        is_disk_read_only: bool,
        direct_io: bool,
        sync_mode: SyncMode,
        writeback_limit_bytes: Option<u64>,
        metrics: BlockMetricsWriter,
    ) -> io::Result<Block> {
        Self::new_with_writeback_limit_handle(
            id,
            partuuid,
            cache_type,
            disk_image_path,
            disk_image_format,
            is_disk_read_only,
            direct_io,
            sync_mode,
            writeback_limit_bytes.map(WritebackLimit::new),
            metrics,
        )
    }

    /// Create a virtio block device with an optional live buffered-writeback budget.
    #[allow(clippy::too_many_arguments)]
    pub fn new_with_writeback_limit_handle(
        id: String,
        partuuid: Option<String>,
        cache_type: CacheType,
        disk_image_path: String,
        disk_image_format: ImageType,
        is_disk_read_only: bool,
        direct_io: bool,
        sync_mode: SyncMode,
        writeback_limit: Option<WritebackLimit>,
        metrics: BlockMetricsWriter,
    ) -> io::Result<Block> {
        // Keep zero equivalent to the builder's disabled state for callers of this lower-level API.
        let writeback_limit = writeback_limit.filter(|limit| limit.maximum_bytes() != 0);

        if matches!(disk_image_format, ImageType::Vmdk) && !is_disk_read_only {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "VMDK write support is not available; configure the disk as read-only",
            ));
        }

        if let Some(_hard_budget_bytes) =
            writeback_limit.as_ref().map(WritebackLimit::maximum_bytes)
        {
            #[cfg(target_os = "linux")]
            if _hard_budget_bytes < MINIMUM_WRITEBACK_BUDGET_BYTES {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "writeback hard budget must be at least {MINIMUM_WRITEBACK_BUDGET_BYTES} bytes"
                    ),
                ));
            }

            #[cfg(not(target_os = "linux"))]
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "bounded writeback is only supported on Linux hosts",
            ));

            #[cfg(target_os = "linux")]
            if !matches!(disk_image_format, ImageType::Raw)
                || is_disk_read_only
                || direct_io
                || cache_type != CacheType::Writeback
                || sync_mode == SyncMode::None
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "bounded writeback requires a writable raw disk with writeback caching, buffered I/O and an active sync mode",
                ));
            }
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
        let writeback_config = match writeback_limit {
            Some(limit) => Some(BufferedWritebackConfig::new(
                Arc::new(disk_image.try_clone()?),
                limit,
            )?),
            None => None,
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

        // Do not attach `RWF_DONTCACHE` to bounded writes. The controller already reserves every
        // dirty page before mutation and returns that credit only after verified writeback, while
        // the per-write hint would start eager writeback and collapse the finite cache window that
        // the hard budget is intended to bound.

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
            let disk_properties = DiskProperties::new(
                disk_image.clone(),
                disk_image_id.clone(),
                cache_type,
                is_disk_read_only,
            )?;
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

        // Bounded writeback punches only whole pages and filesystem blocks, so advertise its
        // coarser alignment and let the guest issue discards the controller can retire whole.
        #[cfg(target_os = "linux")]
        let discard_alignment = writeback_config.as_ref().map_or(
            discard_alignment as u64,
            BufferedWritebackConfig::discard_alignment,
        );
        #[cfg(not(target_os = "linux"))]
        let discard_alignment = discard_alignment as u64;

        let mut avail_features = (1u64 << VIRTIO_F_VERSION_1)
            | (1u64 << VIRTIO_BLK_F_SEG_MAX)
            | (1u64 << VIRTIO_BLK_F_WRITE_ZEROES)
            | (1u64 << VIRTIO_BLK_F_DISCARD)
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
            discard_sector_alignment: (discard_alignment / SECTOR_SIZE) as u32,
            max_write_zeroes_sectors: u32::MAX,
            max_write_zeroes_seg: 1,
            write_zeroes_may_unmap: 1,
            ..Default::default()
        };

        Ok(Block {
            unavailable: false,
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
            worker_stopping: Arc::new(AtomicBool::new(false)),
            quiesced_queue: None,
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

    /// Replaces the backing chain while this device is inactive at a terminal queue boundary.
    ///
    /// The replacement must preserve guest-visible capacity and read-only policy. The virtio
    /// transport, queues, feature negotiation, and serial remain unchanged.
    pub fn replace_backend(
        &mut self,
        backend: PreparedBlockBackend,
    ) -> Result<(), VirtioStateError> {
        if self.worker_thread.is_some() || self.device_state.is_activated() {
            return Err(VirtioStateError::InvalidLifecycle(
                "block backend replacement requires a quiesced device",
            ));
        }
        #[cfg(target_os = "linux")]
        let leaving_bounded_writeback = self.writeback_config.is_some() && backend.direct_io;
        #[cfg(target_os = "linux")]
        if self.writeback_config.is_some() && !leaving_bounded_writeback {
            return Err(VirtioStateError::Incompatible(
                "bounded writeback backend replacement requires direct I/O".to_string(),
            ));
        }
        let configured_capacity = self.config.capacity;
        if backend.capacity_sectors != configured_capacity {
            return Err(VirtioStateError::Incompatible(format!(
                "replacement capacity {} differs from guest-visible capacity {}",
                backend.capacity_sectors, configured_capacity
            )));
        }
        if backend.read_only != self.is_read_only() {
            return Err(VirtioStateError::Incompatible(
                "replacement read-only policy differs from the active device".to_string(),
            ));
        }
        let flush_advertised = self.avail_features & (1u64 << VIRTIO_BLK_F_FLUSH) != 0;
        if (backend.sync_mode != SyncMode::None) != flush_advertised {
            return Err(VirtioStateError::Incompatible(
                "replacement synchronization policy would change advertised flush support"
                    .to_string(),
            ));
        }
        let replacement_discard_alignment = backend.discard_alignment as u32 / SECTOR_SIZE as u32;
        let configured_discard_alignment = self.config.discard_sector_alignment;
        #[cfg(target_os = "linux")]
        let discard_alignment_is_compatible = leaving_bounded_writeback
            || replacement_discard_alignment == configured_discard_alignment;
        #[cfg(not(target_os = "linux"))]
        let discard_alignment_is_compatible =
            replacement_discard_alignment == configured_discard_alignment;
        if !discard_alignment_is_compatible {
            return Err(VirtioStateError::Incompatible(format!(
                "replacement discard alignment {replacement_discard_alignment} differs from guest-visible alignment {configured_discard_alignment}",
            )));
        }

        #[allow(unused_mut)]
        let mut disk = DiskProperties::new(
            Arc::clone(&backend.disk_image),
            self.disk_image_id.clone(),
            self.cache_type,
            backend.read_only,
        )?;
        #[cfg(windows)]
        disk.set_windows_raw_file(backend.windows_raw_file.clone());
        #[cfg(target_os = "linux")]
        if leaving_bounded_writeback {
            // The new direct-I/O backend bypasses the host page cache and no longer needs
            // raw-offset accounting. Imago now serves discards, aligning them itself.
            self.writeback_config = None;
        }
        self.disk_image = backend.disk_image;
        #[cfg(windows)]
        {
            self.windows_raw_file = backend.windows_raw_file;
        }
        self.disk = Some(disk);
        Ok(())
    }

    /// Grows the active image at a drained queue boundary, preserving its backing chain.
    /// The caller must publish its recovery intent before calling: an I/O failure may occur
    /// after image metadata has grown, so rollback by truncation is never safe.
    pub fn grow_capacity(&mut self, size_bytes: u64) -> Result<(), VirtioStateError> {
        if self.worker_thread.is_some() || self.device_state.is_activated() {
            return Err(VirtioStateError::InvalidLifecycle(
                "block growth requires quiescence",
            ));
        }
        if self.is_read_only()
            || !size_bytes.is_multiple_of(SECTOR_SIZE)
            || size_bytes / SECTOR_SIZE < self.config.capacity
        {
            return Err(VirtioStateError::Incompatible(
                "block growth requires a writable device and an aligned nondecreasing size".into(),
            ));
        }
        {
            let image = self.disk_image.lock().unwrap();
            image.resize_grow(size_bytes, imago::format::PreallocateMode::None)?;
            image.flush()?;
            image.sync()?;
        }
        // Keep the drained DiskProperties (including writeback state and Windows handles).
        // Only the bounds change; rebuilding it would discard policy/accounting state.
        if let Some(disk) = self.disk.as_mut() {
            disk.nsectors = size_bytes / SECTOR_SIZE;
        }
        self.config.capacity = size_bytes / SECTOR_SIZE;
        Ok(())
    }

    fn padded_disk_image_id(id: &str) -> Vec<u8> {
        let mut padded = vec![0u8; VIRTIO_BLK_ID_BYTES as usize];
        let bytes = id.as_bytes();
        let n = cmp::min(bytes.len(), padded.len());
        padded[..n].copy_from_slice(&bytes[..n]);
        padded
    }

    fn from_prepared_backend(
        id: String,
        partuuid: Option<String>,
        cache_type: CacheType,
        disk_image_id: Vec<u8>,
        backend: PreparedBlockBackend,
        metrics: BlockMetricsWriter,
    ) -> io::Result<Block> {
        let disk_image = backend.disk_image;
        #[allow(unused_mut)]
        let mut disk = DiskProperties::new(
            Arc::clone(&disk_image),
            disk_image_id.clone(),
            cache_type,
            backend.read_only,
        )?;
        #[cfg(windows)]
        disk.set_windows_raw_file(backend.windows_raw_file.clone());

        let mut avail_features = (1u64 << VIRTIO_F_VERSION_1)
            | (1u64 << VIRTIO_BLK_F_SEG_MAX)
            | (1u64 << VIRTIO_BLK_F_WRITE_ZEROES)
            | (1u64 << VIRTIO_BLK_F_DISCARD)
            | (1u64 << VIRTIO_RING_F_EVENT_IDX);
        if backend.sync_mode != SyncMode::None {
            avail_features |= 1u64 << VIRTIO_BLK_F_FLUSH;
        }
        if backend.read_only {
            avail_features |= 1u64 << VIRTIO_BLK_F_RO;
        }

        let config = VirtioBlkConfig {
            capacity: backend.capacity_sectors,
            size_max: 0,
            seg_max: 254,
            max_discard_sectors: u32::MAX,
            max_discard_seg: 1,
            discard_sector_alignment: backend.discard_alignment as u32 / 512,
            max_write_zeroes_sectors: u32::MAX,
            max_write_zeroes_seg: 1,
            write_zeroes_may_unmap: 1,
            ..Default::default()
        };

        Ok(Block {
            unavailable: false,
            id,
            partuuid,
            disk: Some(disk),
            cache_type,
            disk_image,
            disk_image_id,
            #[cfg(target_os = "linux")]
            writeback_config: None,
            #[cfg(windows)]
            windows_raw_file: backend.windows_raw_file,
            metrics,
            worker_thread: None,
            worker_stopfd: EventFd::new(EFD_NONBLOCK)?,
            worker_stopping: Arc::new(AtomicBool::new(false)),
            quiesced_queue: None,
            avail_features,
            acked_features: 0,
            config,
            device_state: DeviceState::Inactive,
        })
    }

    fn stop_worker(&mut self) -> io::Result<()> {
        if self.worker_thread.is_some() {
            self.worker_stopping.store(true, Ordering::Release);
            self.worker_stopfd.write(1)?;
            let worker = self
                .worker_thread
                .take()
                .expect("worker presence was checked before signalling");
            let worker = worker
                .join()
                .map_err(|error| io::Error::other(format!("block worker panicked: {error:?}")))?;
            let (queue, disk, shutdown_error) = worker.into_stopped_parts();
            self.quiesced_queue = Some(queue);
            self.disk = Some(disk);
            if let Some(error) = shutdown_error {
                return Err(error);
            }
        } else if let Some(disk) = &self.disk {
            // A previous stop may have reached the queue boundary but failed its durability
            // fence. Let a later attempt retry the flush without reactivating the worker.
            disk.flush_to_disk()?;
        }
        Ok(())
    }

    /// Captures device-specific block state after the transport has quiesced the worker.
    pub fn capture_state(&self) -> Result<BlockState, VirtioStateError> {
        if self.worker_thread.is_some() {
            return Err(VirtioStateError::InvalidLifecycle(
                "block worker must be quiesced before state capture",
            ));
        }

        Ok(BlockState {
            version: BLOCK_STATE_VERSION,
            id: self.id.clone(),
            partuuid: self.partuuid.clone(),
            capacity_sectors: self.config.capacity,
            disk_image_id: self.disk_image_id.clone(),
            avail_features: self.avail_features,
            cache_type: self.cache_type,
            read_only: self.is_read_only(),
        })
    }

    /// Validates saved block identity and policy without mutating the device.
    ///
    /// A destination may implement more features than the source offered, but it must implement
    /// every saved feature before the source contract can be restored.
    pub fn validate_state(&self, state: &BlockState) -> Result<(), VirtioStateError> {
        if state.version != BLOCK_STATE_VERSION {
            return Err(VirtioStateError::Incompatible(format!(
                "unsupported block state version {}",
                state.version
            )));
        }
        if self.worker_thread.is_some() {
            return Err(VirtioStateError::InvalidLifecycle(
                "block worker must be inactive before state restore",
            ));
        }
        if self.id != state.id
            || self.partuuid != state.partuuid
            || self.config.capacity != state.capacity_sectors
            || self.disk_image_id != state.disk_image_id
            || self.cache_type != state.cache_type
            || self.is_read_only() != state.read_only
        {
            return Err(VirtioStateError::Incompatible(
                "saved block identity, capacity, or cache policy differs from the destination"
                    .to_string(),
            ));
        }
        if state.avail_features & !self.avail_features != 0 {
            return Err(VirtioStateError::Incompatible(
                "saved block features are unavailable on the destination".to_string(),
            ));
        }
        Ok(())
    }

    /// Restores the exact guest-visible feature contract after compatibility validation.
    pub fn restore_state(&mut self, state: &BlockState) -> Result<(), VirtioStateError> {
        self.validate_state(state)?;
        // Destination-only features must remain hidden from the restored guest. The driver may
        // reread feature pages after resume, and a reset must not silently widen its device ABI.
        self.avail_features = state.avail_features;
        Ok(())
    }
}

/// Device-specific state that must accompany a virtio-block transport snapshot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BlockState {
    /// State contract version.
    pub version: u16,
    /// Stable virtio device identifier.
    pub id: String,
    /// Optional partition UUID exposed by the VMM configuration.
    pub partuuid: Option<String>,
    /// Guest-visible capacity in 512-byte sectors.
    pub capacity_sectors: u64,
    /// Guest-visible virtio block serial bytes.
    pub disk_image_id: Vec<u8>,
    /// Features offered by the source device.
    pub avail_features: u64,
    /// Guest-visible cache policy.
    pub cache_type: CacheType,
    /// Whether writes are prohibited.
    pub read_only: bool,
}

impl Drop for Block {
    fn drop(&mut self) {
        if let Err(error) = self.stop_worker() {
            error!("error stopping block worker during drop: {error}");
        }
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
        self.worker_stopping.store(false, Ordering::Release);
        self.quiesced_queue = None;

        let [blk_q]: [_; NUM_QUEUES] = queues.try_into().map_err(|_| {
            error!("Cannot perform activate. Expected {} queue(s)", NUM_QUEUES);
            ActivateError::BadActivate
        })?;

        let disk = match self.disk.take() {
            Some(d) => d,
            None => {
                let mut disk = DiskProperties::new(
                    Arc::clone(&self.disk_image),
                    self.disk_image_id.clone(),
                    self.cache_type,
                    self.is_read_only(),
                )
                .map_err(|_| ActivateError::BadActivate)?;
                disk.unavailable = self.unavailable;
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

        #[cfg(target_os = "linux")]
        let disk = {
            let mut disk = disk;
            disk.set_writeback_config(self.writeback_config.as_ref())
                .map_err(|error| {
                    error!("Cannot start bounded block writeback: {error}");
                    ActivateError::BadActivate
                })?;
            disk
        };

        let worker = BlockWorker::new(
            blk_q,
            interrupt.clone(),
            mem.clone(),
            disk,
            self.worker_stopfd.try_clone().unwrap(),
            Arc::clone(&self.worker_stopping),
            self.metrics.clone(),
        );
        self.worker_thread = Some(worker.run());

        self.device_state = DeviceState::Activated(mem, interrupt);
        Ok(())
    }

    fn reset(&mut self) -> bool {
        if let Err(error) = self.stop_worker() {
            error!("error stopping block worker during reset: {error}");
            return false;
        }
        self.quiesced_queue = None;
        self.device_state = DeviceState::Inactive;
        true
    }

    fn supports_quiesce(&self) -> bool {
        true
    }

    fn quiesce(&mut self) -> Result<Vec<DeviceQueue>, VirtioStateError> {
        if !self.device_state.is_activated() && self.quiesced_queue.is_none() {
            return Err(VirtioStateError::InvalidLifecycle(
                "block device is not activated",
            ));
        }
        self.stop_worker()?;
        let queue = self
            .quiesced_queue
            .take()
            .ok_or(VirtioStateError::InvalidLifecycle(
                "block worker did not return its queue",
            ))?;
        if let Err(error) = queue.queue.capture_state() {
            self.quiesced_queue = Some(queue);
            return Err(error.into());
        }
        self.device_state = DeviceState::Inactive;
        Ok(vec![queue])
    }
}

//--------------------------------------------------------------------------------------------------
// Tests
//--------------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use utils::metrics::MetricsWriter;
    #[cfg(target_os = "linux")]
    use utils::tempfile::TempFile;

    #[cfg(target_os = "linux")]
    use crate::virtio::block::BlockLayerSpec;

    use super::*;

    #[test]
    fn read_only_disk_durability_fence_is_already_satisfied() {
        let image = temp_image_path("read-only-flush");
        let file = File::create(&image).unwrap();
        file.set_len(4 * 1024 * 1024).unwrap();
        drop(file);
        let block = Block::new(
            "read-only".to_string(),
            None,
            CacheType::Writeback,
            image.to_string_lossy().into_owned(),
            ImageType::Raw,
            true,
            false,
            SyncMode::Full,
            MetricsWriter::default().register_block_device("read-only".to_string()),
        )
        .unwrap();

        block.disk.as_ref().unwrap().flush_to_disk().unwrap();
        drop(block);
        std::fs::remove_file(image).unwrap();
    }

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

    #[test]
    fn drained_raw_growth_preserves_data_and_updates_request_bounds() {
        let image = temp_image_path("capacity-growth");
        let mut file = File::create(&image).unwrap();
        file.write_all(&[0x5a; 512]).unwrap();
        file.set_len(1024 * 1024).unwrap();
        drop(file);
        let mut block = Block::new(
            "grow".into(),
            None,
            CacheType::Writeback,
            image.to_string_lossy().into_owned(),
            ImageType::Raw,
            false,
            false,
            SyncMode::Full,
            MetricsWriter::default().register_block_device("grow".into()),
        )
        .unwrap();
        assert!(block.grow_capacity(1000).is_err());
        assert!(block.grow_capacity(512).is_err());
        block.grow_capacity(2 * 1024 * 1024).unwrap();
        block.grow_capacity(2 * 1024 * 1024).unwrap();
        let capacity = block.config.capacity;
        assert_eq!(capacity, 4096);
        assert_eq!(block.disk.as_ref().unwrap().nsectors, 4096);
        let mut data = [0u8; 512];
        let disk = block.disk_image.lock().unwrap();
        disk.read(&mut data[..], 0).unwrap();
        assert_eq!(data, [0x5a; 512]);
        disk.read(&mut data[..], 2 * 1024 * 1024 - 512).unwrap();
        assert_eq!(data, [0; 512]);
        drop(disk);
        drop(block);
        let mut readonly = Block::new(
            "readonly".into(),
            None,
            CacheType::Writeback,
            image.to_string_lossy().into_owned(),
            ImageType::Raw,
            true,
            false,
            SyncMode::Full,
            MetricsWriter::default().register_block_device("readonly".into()),
        )
        .unwrap();
        assert!(readonly.grow_capacity(3 * 1024 * 1024).is_err());
        assert_eq!(std::fs::metadata(&image).unwrap().len(), 2 * 1024 * 1024);
    }

    #[test]
    fn zero_writeback_limit_disables_the_policy() {
        let result = Block::new_with_writeback_limit(
            "raw".to_string(),
            None,
            CacheType::Unsafe,
            "missing.raw".to_string(),
            ImageType::Raw,
            false,
            true,
            SyncMode::None,
            Some(0),
            MetricsWriter::default().register_block_device("raw".to_string()),
        );

        let error = match result {
            Ok(_) => panic!("missing raw disk should fail"),
            Err(error) => error,
        };
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn bounded_mutation_range_requires_full_visible_capacity() {
        let nsectors = 8;
        assert!(DiskProperties::validate_range_against_capacity(nsectors, 512, 3584).is_ok());

        let crossing_end =
            DiskProperties::validate_range_against_capacity(nsectors, 512, 4096).unwrap_err();
        assert_eq!(crossing_end.kind(), io::ErrorKind::InvalidInput);
        assert!(crossing_end.to_string().contains("visible capacity"));

        let past_end =
            DiskProperties::validate_range_against_capacity(nsectors, 4096, 512).unwrap_err();
        assert_eq!(past_end.kind(), io::ErrorKind::InvalidInput);

        let overflow =
            DiskProperties::validate_range_against_capacity(nsectors, u64::MAX, 1).unwrap_err();
        assert_eq!(overflow.kind(), io::ErrorKind::InvalidInput);
        assert!(overflow.to_string().contains("range overflow"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn bounded_writeback_advertises_aligned_discard_and_unmap() {
        let backing = TempFile::new().unwrap();
        backing.as_file().set_len(4 * 1024 * 1024).unwrap();
        let block = Block::new_with_writeback_limit(
            "bounded".to_string(),
            None,
            CacheType::Writeback,
            backing.as_path().to_string_lossy().into_owned(),
            ImageType::Raw,
            false,
            false,
            SyncMode::Full,
            Some(MINIMUM_WRITEBACK_BUDGET_BYTES),
            MetricsWriter::default().register_block_device("bounded".to_string()),
        )
        .unwrap();

        assert_ne!(block.avail_features & (1u64 << VIRTIO_BLK_F_DISCARD), 0);
        assert_ne!(
            block.avail_features & (1u64 << VIRTIO_BLK_F_WRITE_ZEROES),
            0
        );
        let max_discard_sectors = block.config.max_discard_sectors;
        let max_discard_seg = block.config.max_discard_seg;
        let discard_sector_alignment = block.config.discard_sector_alignment;
        let write_zeroes_may_unmap = block.config.write_zeroes_may_unmap;
        let alignment = block.writeback_config.as_ref().unwrap().discard_alignment();
        // SAFETY: sysconf has no pointer arguments.
        let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as u64;
        assert!(alignment >= page_size && alignment.is_power_of_two());
        assert_eq!(max_discard_sectors, u32::MAX);
        assert_eq!(max_discard_seg, 1);
        assert_eq!(u64::from(discard_sector_alignment), alignment / SECTOR_SIZE);
        assert_eq!(write_zeroes_may_unmap, 1);

        let legacy = Block::new(
            "legacy".to_string(),
            None,
            CacheType::Writeback,
            backing.as_path().to_string_lossy().into_owned(),
            ImageType::Raw,
            false,
            false,
            SyncMode::Full,
            MetricsWriter::default().register_block_device("legacy".to_string()),
        )
        .unwrap();
        let legacy_may_unmap = legacy.config.write_zeroes_may_unmap;
        assert_ne!(legacy.avail_features & (1u64 << VIRTIO_BLK_F_DISCARD), 0);
        assert_eq!(legacy_may_unmap, 1);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn bounded_writeback_rejects_too_small_budget() {
        let result = Block::new_with_writeback_limit(
            "raw".to_string(),
            None,
            CacheType::Writeback,
            "missing.raw".to_string(),
            ImageType::Raw,
            false,
            false,
            SyncMode::Full,
            Some(MINIMUM_WRITEBACK_BUDGET_BYTES - 1),
            MetricsWriter::default().register_block_device("raw".to_string()),
        );

        let error = match result {
            Ok(_) => panic!("too-small writeback budget should fail"),
            Err(error) => error,
        };
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(error.to_string().contains("at least"));
    }

    #[test]
    fn unavailable_worker_rejects_all_storage_io_but_preserves_identity() {
        use super::super::worker::{RequestError, RequestHeader};
        use crate::legacy::DummyIrqChip;
        use crate::virtio::descriptor_utils::{
            create_descriptor_chain, DescriptorType, Reader, Writer,
        };
        use vm_memory::{Bytes, GuestAddress};

        for cache_type in [CacheType::Writeback, CacheType::Unsafe] {
            let state = BlockState {
                version: BLOCK_STATE_VERSION,
                id: "missing".into(),
                partuuid: None,
                capacity_sectors: 8 * 1024 * 1024,
                disk_image_id: b"missing".to_vec(),
                avail_features: 1u64 << VIRTIO_F_VERSION_1,
                cache_type,
                read_only: false,
            };
            let mut block = Block::new_unavailable(
                &state,
                MetricsWriter::default().register_block_device("missing".into()),
            )
            .unwrap();
            assert_eq!(block.capture_state().unwrap(), state);
            block.stop_worker().unwrap();
            let disk = block.disk.take().unwrap();
            assert!(disk.unavailable);
            let mem = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x10000)]).unwrap();
            let mut worker = BlockWorker::new(
                DeviceQueue {
                    queue: crate::virtio::Queue::new(256),
                    event: Arc::new(EventFd::new(0).unwrap()),
                },
                InterruptTransport::new(DummyIrqChip::new().into(), "missing".into()).unwrap(),
                mem.clone(),
                disk,
                EventFd::new(0).unwrap(),
                Arc::new(AtomicBool::new(false)),
                MetricsWriter::default().register_block_device("missing".into()),
            );
            for kind in [
                VIRTIO_BLK_T_IN,
                VIRTIO_BLK_T_OUT,
                VIRTIO_BLK_T_FLUSH,
                VIRTIO_BLK_T_DISCARD,
                VIRTIO_BLK_T_WRITE_ZEROES,
                VIRTIO_BLK_T_GET_ID,
            ] {
                let chain = create_descriptor_chain(
                    &mem,
                    GuestAddress(0),
                    GuestAddress(0x1000),
                    vec![(DescriptorType::Writable, 512)],
                    0,
                )
                .unwrap();
                mem.write_obj(kind, GuestAddress(0x2000)).unwrap();
                let header: RequestHeader = mem.read_obj(GuestAddress(0x2000)).unwrap();
                mem.write_slice(&[0x5a; 512], GuestAddress(0x1000)).unwrap();
                let mut writer = Writer::new(&mem, chain.clone()).unwrap();
                let result = worker.process_request(
                    header,
                    &mut Reader::new(&mem, chain.clone()).unwrap(),
                    &mut writer,
                );
                if kind == VIRTIO_BLK_T_GET_ID {
                    assert!(result.is_ok());
                } else {
                    assert!(
                        matches!(result, Err(RequestError::Unavailable)),
                        "request {kind}: {result:?}"
                    );
                    writer.write_obj(VIRTIO_BLK_S_IOERR as u8).unwrap();
                    let mut payload = [0; 511];
                    mem.read_slice(&mut payload, GuestAddress(0x1000)).unwrap();
                    assert_eq!(payload, [0x5a; 511]);
                    assert_eq!(
                        mem.read_obj::<u8>(GuestAddress(0x11ff)).unwrap(),
                        VIRTIO_BLK_S_IOERR as u8
                    );
                }
            }
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn bounded_worker_punches_only_when_unmap_is_allowed() {
        use std::os::unix::fs::{FileExt, MetadataExt};

        use vm_memory::{Bytes, GuestAddress};

        use super::super::worker::{DiscardWriteData, RequestError, RequestHeader};
        use crate::legacy::DummyIrqChip;
        use crate::virtio::descriptor_utils::{
            create_descriptor_chain, DescriptorType, Reader, Writer,
        };

        // Cross several explicit-zero chunks under a tiny pressure target. A plain zero write
        // must keep its allocation, an unmapping one must deallocate, and both must leave the
        // neighboring sentinels intact.
        let backing = TempFile::new().unwrap();
        let length = (4 * 1024 * 1024).max(16 * test_discard_alignment(&backing) as usize);
        // Size the file before filling it so no speculative preallocation skews block counts.
        backing.as_file().set_len(length as u64).unwrap();
        backing
            .as_file()
            .write_all_at(&vec![0x5a; length], 0)
            .unwrap();
        backing.as_file().sync_all().unwrap();
        let allocated = backing.as_file().metadata().unwrap().blocks();
        let limit = WritebackLimit::new(MINIMUM_WRITEBACK_BUDGET_BYTES);
        limit.set_target_bytes(4096).unwrap();
        let mut block = Block::new_with_writeback_limit_handle(
            "zero-regression".into(),
            None,
            CacheType::Writeback,
            backing.as_path().to_string_lossy().into_owned(),
            ImageType::Raw,
            false,
            false,
            SyncMode::Full,
            Some(limit),
            MetricsWriter::default().register_block_device("zero-regression".into()),
        )
        .unwrap();
        let alignment = block.writeback_config.as_ref().unwrap().discard_alignment() as usize;
        let mut disk = block.disk.take().unwrap();
        disk.set_writeback_config(block.writeback_config.as_ref())
            .unwrap();
        assert!(disk.has_writeback_limit());
        let mem = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x10000)]).unwrap();
        let mut worker = BlockWorker::new(
            DeviceQueue {
                queue: crate::virtio::Queue::new(256),
                event: Arc::new(EventFd::new(0).unwrap()),
            },
            InterruptTransport::new(DummyIrqChip::new().into(), "zero-regression".into()).unwrap(),
            mem.clone(),
            disk,
            EventFd::new(0).unwrap(),
            Arc::new(AtomicBool::new(false)),
            MetricsWriter::default().register_block_device("zero-regression".into()),
        );
        let mut request = |kind, sector, num_sectors, flags| {
            let chain = create_descriptor_chain(
                &mem,
                GuestAddress(0),
                GuestAddress(0x1000),
                vec![
                    (DescriptorType::Readable, 16),
                    (DescriptorType::Writable, 1),
                ],
                0,
            )
            .unwrap();
            // Encode wire bytes rather than sharing the handler's struct construction.
            let mut payload = Vec::new();
            payload.extend_from_slice(&u64::to_le_bytes(sector));
            payload.extend_from_slice(&u32::to_le_bytes(num_sectors));
            payload.extend_from_slice(&u32::to_le_bytes(flags));
            mem.write_slice(&payload, GuestAddress(0x1000)).unwrap();
            let header_mem = GuestAddress(0x2000);
            mem.write_obj(kind, header_mem).unwrap();
            let header: RequestHeader = mem.read_obj(header_mem).unwrap();
            worker.process_request(
                header,
                &mut Reader::new(&mem, chain.clone()).unwrap(),
                &mut Writer::new(&mem, chain).unwrap(),
            )
        };
        assert_eq!(std::mem::size_of::<DiscardWriteData>(), 16);
        let sentinel = alignment;
        let zero_sectors = ((length - 2 * sentinel) / 512) as u32;
        let zero_sector = (sentinel / 512) as u64;

        assert_eq!(
            request(VIRTIO_BLK_T_WRITE_ZEROES, zero_sector, zero_sectors, 0).unwrap(),
            0
        );
        assert!(
            backing.as_file().metadata().unwrap().blocks() >= allocated,
            "plain zero writes must not punch holes"
        );

        assert_eq!(
            request(
                VIRTIO_BLK_T_WRITE_ZEROES,
                zero_sector,
                zero_sectors,
                VIRTIO_BLK_WRITE_ZEROES_FLAG_UNMAP
            )
            .unwrap(),
            0
        );
        let after_unmap = backing.as_file().metadata().unwrap().blocks();
        assert!(
            after_unmap < allocated,
            "unmapping zero writes must deallocate the aligned range"
        );

        assert!(matches!(
            request(
                VIRTIO_BLK_T_WRITE_ZEROES,
                (length / 512 - 1) as u64,
                2,
                VIRTIO_BLK_WRITE_ZEROES_FLAG_UNMAP
            ),
            Err(RequestError::InvalidMutationRange(_))
        ));
        assert!(matches!(
            request(VIRTIO_BLK_T_DISCARD, (length / 512 - 1) as u64, 2, 0),
            Err(RequestError::InvalidMutationRange(_))
        ));

        // A discard of the leading sentinel deallocates it as well.
        assert_eq!(
            request(VIRTIO_BLK_T_DISCARD, 0, (sentinel / 512) as u32, 0).unwrap(),
            0
        );
        assert!(backing.as_file().metadata().unwrap().blocks() < after_unmap);
        request(VIRTIO_BLK_T_FLUSH, 0, 0, 0).unwrap();
        drop(worker);
        let bytes = std::fs::read(backing.as_path()).unwrap();
        assert_eq!(bytes.len(), length);
        assert!(bytes[..length - sentinel].iter().all(|b| *b == 0));
        assert!(bytes[length - sentinel..].iter().all(|b| *b == 0x5a));
    }

    #[cfg(target_os = "linux")]
    fn test_discard_alignment(backing: &TempFile) -> u64 {
        BufferedWritebackConfig::new(
            Arc::new(backing.as_file().try_clone().unwrap()),
            WritebackLimit::new(MINIMUM_WRITEBACK_BUDGET_BYTES),
        )
        .unwrap()
        .discard_alignment()
    }

    /// Opens a bounded disk over `units` discard-alignment units of synced 0x5a data.
    #[cfg(target_os = "linux")]
    fn active_bounded_disk(backing: &TempFile, units: u64) -> (Block, DiskProperties, u64) {
        use std::os::unix::fs::FileExt;

        let alignment = test_discard_alignment(backing);
        let length = units * alignment;
        // Size the file before filling it so no speculative preallocation skews block counts.
        backing.as_file().set_len(length).unwrap();
        backing
            .as_file()
            .write_all_at(&vec![0x5a; length as usize], 0)
            .unwrap();
        backing.as_file().sync_all().unwrap();
        let mut block = Block::new_with_writeback_limit(
            "bounded-discard".into(),
            None,
            CacheType::Writeback,
            backing.as_path().to_string_lossy().into_owned(),
            ImageType::Raw,
            false,
            false,
            SyncMode::Full,
            Some(MINIMUM_WRITEBACK_BUDGET_BYTES),
            MetricsWriter::default().register_block_device("bounded-discard".into()),
        )
        .unwrap();
        let mut disk = block.disk.take().unwrap();
        disk.set_writeback_config(block.writeback_config.as_ref())
            .unwrap();
        assert_eq!(
            block.writeback_config.as_ref().unwrap().discard_alignment(),
            alignment
        );
        (block, disk, alignment)
    }

    #[cfg(target_os = "linux")]
    fn tracked_bytes(disk: &DiskProperties) -> u64 {
        disk.writeback_controller
            .as_ref()
            .unwrap()
            .borrow()
            .tracked_bytes()
    }

    #[cfg(target_os = "linux")]
    fn write_accounted(disk: &DiskProperties, offset: u64, data: &[u8]) {
        disk.run_buffered_mutation(
            offset,
            data.len() as u64,
            None,
            |chunk_offset, chunk_length| {
                let start = (chunk_offset - offset) as usize;
                let diskfile = disk.file.lock().unwrap();
                diskfile.write(&data[start..start + chunk_length as usize], chunk_offset)
            },
        )
        .unwrap();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn bounded_discard_retires_credit_for_dirty_pages() {
        let backing = TempFile::new().unwrap();
        let (_block, disk, alignment) = active_bounded_disk(&backing, 8);

        write_accounted(&disk, 0, &vec![0xa5; 4 * alignment as usize]);
        assert_eq!(tracked_bytes(&disk), 4 * alignment);

        // An unaligned unmapping zero write punches the middle half, so the kernel drops those
        // pages and their credit returns now. Its explicit edge writes land on pages that are
        // already charged.
        disk.discard_to_zero(alignment / 2, 3 * alignment).unwrap();
        assert_eq!(tracked_bytes(&disk), 2 * alignment);

        // A covering discard retires the rest.
        disk.discard_to_any(0, 4 * alignment).unwrap();
        assert_eq!(tracked_bytes(&disk), 0);

        write_accounted(&disk, 0, &vec![0xa5; alignment as usize]);
        assert_eq!(tracked_bytes(&disk), alignment);
        disk.flush_to_disk().unwrap();
        assert_eq!(tracked_bytes(&disk), 0);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn bounded_discard_punches_only_the_aligned_interior() {
        use std::os::unix::fs::{FileExt, MetadataExt};

        let backing = TempFile::new().unwrap();
        let (_block, disk, alignment) = active_bounded_disk(&backing, 32);
        let a = alignment as usize;
        let allocated = backing.as_file().metadata().unwrap().blocks();
        let read = |offset: usize, len: usize| {
            let mut bytes = vec![0; len];
            backing
                .as_file()
                .read_exact_at(&mut bytes, offset as u64)
                .unwrap();
            bytes
        };

        // A discard smaller than one alignment unit cannot be punched without dirtying a partial
        // block, so it is a no-op. Its contents are unspecified, so that is allowed.
        disk.discard_to_any(512, alignment - 1024).unwrap();
        assert!(read(0, a).iter().all(|b| *b == 0x5a));
        assert_eq!(backing.as_file().metadata().unwrap().blocks(), allocated);
        assert_eq!(tracked_bytes(&disk), 0);

        // An unaligned discard punches only the aligned interior and leaves the edges.
        disk.discard_to_any(alignment + 512, 3 * alignment - 1024)
            .unwrap();
        assert!(read(a, 512).iter().all(|b| *b == 0x5a));
        assert!(read(2 * a, a).iter().all(|b| *b == 0));
        assert!(read(3 * a, a).iter().all(|b| *b == 0x5a));
        let after_discard = backing.as_file().metadata().unwrap().blocks();
        assert!(after_discard < allocated);
        assert_eq!(tracked_bytes(&disk), 0);

        // An unaligned unmapping zero write zeroes the edges through accounted writes.
        let start = 8 * a + 512;
        let len = 3 * a - 1024;
        disk.discard_to_zero(start as u64, len as u64).unwrap();
        assert!(read(8 * a, 512).iter().all(|b| *b == 0x5a));
        assert!(read(start, len).iter().all(|b| *b == 0));
        assert!(read(start + len, 512).iter().all(|b| *b == 0x5a));
        let after_zero = backing.as_file().metadata().unwrap().blocks();
        assert!(after_zero < after_discard);
        assert_eq!(tracked_bytes(&disk), 2 * alignment);

        // A zeroing request inside one alignment unit becomes an accounted zero write.
        disk.discard_to_zero(16 * alignment + 512, 1024).unwrap();
        assert!(read(16 * a + 512, 1024).iter().all(|b| *b == 0));
        assert!(read(16 * a, 512).iter().all(|b| *b == 0x5a));
        assert!(backing.as_file().metadata().unwrap().blocks() >= after_zero);
        assert_eq!(tracked_bytes(&disk), 3 * alignment);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn bounded_discard_without_punch_support_never_writes_unaccounted_data() {
        use std::os::unix::fs::{FileExt, MetadataExt};

        let backing = TempFile::new().unwrap();
        let (_block, disk, alignment) = active_bounded_disk(&backing, 8);
        disk.writeback_controller
            .as_ref()
            .unwrap()
            .borrow_mut()
            .assume_punch_unsupported();
        let allocated = backing.as_file().metadata().unwrap().blocks();

        // A plain discard degrades to a no-op, never to zero-data writes.
        disk.discard_to_any(0, 4 * alignment).unwrap();
        assert_eq!(tracked_bytes(&disk), 0);
        let mut bytes = vec![0; 4 * alignment as usize];
        backing.as_file().read_exact_at(&mut bytes, 0).unwrap();
        assert!(bytes.iter().all(|b| *b == 0x5a));

        // A zeroing discard degrades to accounted explicit zeroes.
        disk.discard_to_zero(0, 4 * alignment).unwrap();
        assert_eq!(tracked_bytes(&disk), 4 * alignment);
        disk.flush_to_disk().unwrap();
        backing.as_file().read_exact_at(&mut bytes, 0).unwrap();
        assert!(bytes.iter().all(|b| *b == 0));
        assert!(backing.as_file().metadata().unwrap().blocks() >= allocated);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn bounded_flush_after_discard_keeps_the_punch_and_stays_healthy() {
        use std::os::unix::fs::{FileExt, MetadataExt};

        let backing = TempFile::new().unwrap();
        let (block, disk, alignment) = active_bounded_disk(&backing, 32);
        let allocated = backing.as_file().metadata().unwrap().blocks();
        let half = 16 * alignment;

        // Dirty pages that never reached writeback, then discard them before the flush.
        write_accounted(&disk, 0, &vec![0xa5; 8 * alignment as usize]);
        assert_eq!(tracked_bytes(&disk), 8 * alignment);
        disk.discard_to_any(0, half).unwrap();
        assert_eq!(tracked_bytes(&disk), 0);

        // Every guest flush ends in the full fsync, which also covers the punch's metadata.
        disk.flush_to_disk().unwrap();
        assert!(block
            .writeback_config
            .as_ref()
            .unwrap()
            .record_full_sync_success());
        let mut bytes = vec![0; half as usize];
        backing.as_file().read_exact_at(&mut bytes, 0).unwrap();
        assert!(bytes.iter().all(|b| *b == 0));
        assert!(backing.as_file().metadata().unwrap().blocks() < allocated);

        // Writes and discards keep working after the flush.
        write_accounted(&disk, 0, &vec![0xa5; alignment as usize]);
        disk.discard_to_zero(0, alignment).unwrap();
        disk.flush_to_disk().unwrap();
        assert_eq!(tracked_bytes(&disk), 0);
        backing
            .as_file()
            .read_exact_at(&mut bytes[..alignment as usize], 0)
            .unwrap();
        assert!(bytes[..alignment as usize].iter().all(|b| *b == 0));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn bounded_writeback_rejects_incompatible_disk_settings() {
        let result = Block::new_with_writeback_limit(
            "raw".to_string(),
            None,
            CacheType::Writeback,
            "missing.raw".to_string(),
            ImageType::Raw,
            false,
            true,
            SyncMode::Full,
            Some(128 * 1024 * 1024),
            MetricsWriter::default().register_block_device("raw".to_string()),
        );

        let error = match result {
            Ok(_) => panic!("bounded writeback with direct I/O should fail"),
            Err(error) => error,
        };
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(error.to_string().contains("buffered I/O"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn bounded_writeback_can_transition_to_a_direct_backend() {
        let original = temp_image_path("bounded-rebind-original");
        let replacement = temp_image_path("bounded-rebind-replacement");
        for path in [&original, &replacement] {
            let file = File::create(path).unwrap();
            file.set_len(4 * 1024 * 1024).unwrap();
        }
        let mut block = Block::new_with_writeback_limit(
            "raw".to_string(),
            None,
            CacheType::Writeback,
            original.to_string_lossy().into_owned(),
            ImageType::Raw,
            false,
            false,
            SyncMode::Full,
            Some(MINIMUM_WRITEBACK_BUDGET_BYTES),
            MetricsWriter::default().register_block_device("raw".to_string()),
        )
        .unwrap();

        let buffered =
            PreparedBlockBackend::open(&BlockBackendSpec::new(vec![BlockLayerSpec::new(
                &replacement,
                ImageType::Raw,
            )]))
            .unwrap();
        assert!(block.replace_backend(buffered).is_err());
        assert!(block.writeback_config.is_some());

        let direct = PreparedBlockBackend::open(
            &BlockBackendSpec::new(vec![BlockLayerSpec::new(&replacement, ImageType::Raw)])
                .direct_io(true),
        )
        .unwrap();
        block.replace_backend(direct).unwrap();

        assert!(block.writeback_config.is_none());
        assert!(!block.disk.as_ref().unwrap().has_writeback_limit());
        std::fs::remove_file(original).unwrap();
        std::fs::remove_file(replacement).unwrap();
    }

    #[test]
    fn restore_masks_destination_only_block_features() {
        let image = temp_image_path("restore-feature-superset");
        let file = File::create(&image).unwrap();
        file.set_len(4 * 1024 * 1024).unwrap();
        drop(file);
        let mut block = Block::new(
            "vdb".to_string(),
            None,
            CacheType::Writeback,
            image.to_string_lossy().into_owned(),
            ImageType::Raw,
            false,
            false,
            SyncMode::Full,
            MetricsWriter::default().register_block_device("vdb".to_string()),
        )
        .unwrap();
        let destination_features = block.avail_features;
        let mut saved = block.capture_state().unwrap();
        saved.avail_features &= !(1u64 << VIRTIO_BLK_F_DISCARD);

        assert_ne!(destination_features, saved.avail_features);
        block.restore_state(&saved).unwrap();
        assert_eq!(block.avail_features, saved.avail_features);
        std::fs::remove_file(image).unwrap();
    }

    #[test]
    fn restore_rejects_block_features_missing_on_destination() {
        let image = temp_image_path("restore-feature-missing");
        let file = File::create(&image).unwrap();
        file.set_len(4 * 1024 * 1024).unwrap();
        drop(file);
        let mut block = Block::new(
            "vdb".to_string(),
            None,
            CacheType::Writeback,
            image.to_string_lossy().into_owned(),
            ImageType::Raw,
            false,
            false,
            SyncMode::Full,
            MetricsWriter::default().register_block_device("vdb".to_string()),
        )
        .unwrap();
        let destination_features = block.avail_features;
        let mut saved = block.capture_state().unwrap();
        saved.avail_features |= 1u64 << 63;

        let error = block.restore_state(&saved).unwrap_err();
        assert!(error.to_string().contains("unavailable on the destination"));
        assert_eq!(block.avail_features, destination_features);
        std::fs::remove_file(image).unwrap();
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn bounded_writeback_is_rejected_on_unsupported_hosts() {
        let result = Block::new_with_writeback_limit(
            "raw".to_string(),
            None,
            CacheType::Writeback,
            "missing.raw".to_string(),
            ImageType::Raw,
            false,
            false,
            SyncMode::Full,
            Some(128 * 1024 * 1024),
            MetricsWriter::default().register_block_device("raw".to_string()),
        );

        let error = match result {
            Ok(_) => panic!("bounded writeback should fail on unsupported hosts"),
            Err(error) => error,
        };
        assert_eq!(error.kind(), io::ErrorKind::Unsupported);
    }

    fn temp_image_path(test_name: &str) -> PathBuf {
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "libkrun-block-{test_name}-{}-{timestamp}.img",
            std::process::id()
        ))
    }
}
