// Copyright 2026 Microsandbox Authors. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use imago::{
    file::File as ImagoFile, qcow2::Qcow2, raw::Raw, DenyImplicitOpenGate, DynStorage,
    FormatAccess, FormatDriverBuilder, Storage, StorageOpenOptions, SyncFormatAccess,
};

#[cfg(windows)]
use super::windows::WindowsRawFile;
use super::{ImageType, SyncMode, SECTOR_SIZE};

//--------------------------------------------------------------------------------------------------
// Types
//--------------------------------------------------------------------------------------------------

type SharedFormat = Arc<FormatAccess<Box<dyn DynStorage>>>;

/// One caller-resolved block layer in an explicit dependency chain.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BlockLayerSpec {
    /// Host path already resolved by the caller.
    pub path: PathBuf,
    /// On-disk format of this exact layer.
    pub format: ImageType,
}

/// An explicit base-to-head block dependency chain.
///
/// Every predecessor is opened read-only. The last layer is the only layer that may be writable.
/// Qcow header paths and external data-file paths are never opened implicitly.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BlockBackendSpec {
    /// Layers ordered from the oldest base through the active head.
    pub layers: Vec<BlockLayerSpec>,
    /// Whether the active head is read-only.
    pub read_only: bool,
    /// Whether host files should use direct I/O.
    pub direct_io: bool,
    /// Host synchronization policy for the active head.
    pub sync_mode: SyncMode,
}

/// A fully opened block backend that can be installed without path resolution while quiesced.
pub struct PreparedBlockBackend {
    pub(crate) disk_image: Arc<Mutex<SyncFormatAccess<Box<dyn DynStorage>>>>,
    pub(crate) discard_alignment: usize,
    pub(crate) capacity_sectors: u64,
    pub(crate) read_only: bool,
    pub(crate) sync_mode: SyncMode,
    #[cfg(windows)]
    pub(crate) windows_raw_file: Option<Arc<WindowsRawFile>>,
}

//--------------------------------------------------------------------------------------------------
// Methods
//--------------------------------------------------------------------------------------------------

impl BlockLayerSpec {
    /// Creates a caller-resolved layer description.
    pub fn new(path: impl Into<PathBuf>, format: ImageType) -> Self {
        Self {
            path: path.into(),
            format,
        }
    }
}

impl BlockBackendSpec {
    /// Creates an explicit base-to-head dependency chain.
    pub fn new(layers: Vec<BlockLayerSpec>) -> Self {
        Self {
            layers,
            read_only: false,
            direct_io: false,
            sync_mode: SyncMode::Full,
        }
    }

    /// Sets whether the head is read-only.
    pub fn read_only(mut self, read_only: bool) -> Self {
        self.read_only = read_only;
        self
    }

    /// Sets direct-I/O behavior for every layer.
    pub fn direct_io(mut self, direct_io: bool) -> Self {
        self.direct_io = direct_io;
        self
    }

    /// Sets the synchronization policy for the head.
    pub fn sync_mode(mut self, sync_mode: SyncMode) -> Self {
        self.sync_mode = sync_mode;
        self
    }

    fn validate(&self) -> io::Result<()> {
        if self.layers.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "explicit block backend requires at least one layer",
            ));
        }
        if self
            .layers
            .iter()
            .any(|layer| matches!(layer.format, ImageType::Vmdk))
        {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "explicit block dependencies support raw and qcow2 layers only",
            ));
        }
        if self
            .layers
            .iter()
            .skip(1)
            .any(|layer| matches!(layer.format, ImageType::Raw))
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "a raw layer cannot have a predecessor",
            ));
        }
        Ok(())
    }
}

impl PreparedBlockBackend {
    /// Opens only the paths named by `spec` and composes them behind one image interface.
    pub fn open(spec: &BlockBackendSpec) -> io::Result<Self> {
        spec.validate()?;

        let mut backing: Option<SharedFormat> = None;
        let last = spec.layers.len() - 1;
        let mut head_access = None;
        let mut head_discard_alignment = 1;

        for (index, layer) in spec.layers.iter().enumerate() {
            let is_head = index == last;
            let writable = is_head && !spec.read_only;
            let file = open_storage(&layer.path, writable, spec.direct_io, &spec.sync_mode)?;
            if is_head {
                head_discard_alignment = file.discard_align();
            }

            match (&layer.format, is_head) {
                (ImageType::Raw, true) => {
                    let raw = Raw::<Box<dyn DynStorage>>::builder(Box::new(file))
                        .write(writable)
                        .open_sync(DenyImplicitOpenGate::default())?;
                    head_access = Some(SyncFormatAccess::new(raw)?);
                }
                (ImageType::Raw, false) => {
                    let raw = Raw::<Box<dyn DynStorage>>::builder(Box::new(file))
                        .write(false)
                        .open_sync(DenyImplicitOpenGate::default())?;
                    backing = Some(Arc::new(FormatAccess::new(raw)));
                }
                (ImageType::Qcow2, true) => {
                    let qcow = Qcow2::<Box<dyn DynStorage>, SharedFormat>::builder(Box::new(file))
                        .write(writable)
                        .backing(backing.take())
                        .data_file(None)
                        .open_sync(DenyImplicitOpenGate::default())?;
                    if qcow.requires_external_data_file() {
                        return Err(io::Error::new(
                            io::ErrorKind::Unsupported,
                            "qcow2 external data files require an explicit typed dependency",
                        ));
                    }
                    head_access = Some(SyncFormatAccess::new(qcow)?);
                }
                (ImageType::Qcow2, false) => {
                    let qcow = Qcow2::<Box<dyn DynStorage>, SharedFormat>::builder(Box::new(file))
                        .write(false)
                        .backing(backing.take())
                        .data_file(None)
                        .open_sync(DenyImplicitOpenGate::default())?;
                    if qcow.requires_external_data_file() {
                        return Err(io::Error::new(
                            io::ErrorKind::Unsupported,
                            "qcow2 external data files require an explicit typed dependency",
                        ));
                    }
                    backing = Some(Arc::new(FormatAccess::new(qcow)));
                }
                (ImageType::Vmdk, _) => unreachable!("validated above"),
            }
        }

        let disk_image = Arc::new(Mutex::new(
            head_access.expect("a validated non-empty chain always produces a head"),
        ));
        let capacity_bytes = disk_image.lock().unwrap().size();
        if !capacity_bytes.is_multiple_of(SECTOR_SIZE) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "explicit block head capacity is not a multiple of 512 bytes",
            ));
        }

        #[cfg(windows)]
        let windows_raw_file =
            if spec.layers.len() == 1 && matches!(spec.layers[0].format, ImageType::Raw) {
                Some(Arc::new(WindowsRawFile::open(
                    &spec.layers[0].path,
                    spec.read_only,
                    spec.direct_io,
                )?))
            } else {
                None
            };

        Ok(Self {
            disk_image,
            discard_alignment: head_discard_alignment,
            capacity_sectors: capacity_bytes / SECTOR_SIZE,
            read_only: spec.read_only,
            sync_mode: spec.sync_mode.clone(),
            #[cfg(windows)]
            windows_raw_file,
        })
    }

    /// Returns the guest-visible capacity in 512-byte sectors.
    pub fn capacity_sectors(&self) -> u64 {
        self.capacity_sectors
    }

    /// Returns whether the prepared head is read-only.
    pub fn is_read_only(&self) -> bool {
        self.read_only
    }
}

//--------------------------------------------------------------------------------------------------
// Functions
//--------------------------------------------------------------------------------------------------

fn open_storage(
    path: &Path,
    writable: bool,
    direct_io: bool,
    sync_mode: &SyncMode,
) -> io::Result<ImagoFile> {
    // Every storage open originates from one path in BlockBackendSpec. The deny gate and explicit
    // backing/data overrides prevent format metadata from expanding that set.
    let options = StorageOpenOptions::new()
        .write(writable)
        .filename(path)
        .direct(direct_io);
    #[cfg(target_os = "macos")]
    let options = options.relaxed_sync(*sync_mode == SyncMode::Relaxed);
    #[cfg(not(target_os = "macos"))]
    let _ = sync_mode;
    ImagoFile::open_sync(options)
}

//--------------------------------------------------------------------------------------------------
// Tests
//--------------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_raw_backend_opens_with_exact_capacity() {
        let image = temp_image_path("exact-capacity");
        let file = std::fs::File::create(&image).unwrap();
        file.set_len(2 * 1024 * 1024).unwrap();
        drop(file);
        let spec = BlockBackendSpec::new(vec![BlockLayerSpec::new(&image, ImageType::Raw)]);

        let backend = PreparedBlockBackend::open(&spec).unwrap();
        assert_eq!(backend.capacity_sectors(), 4096);
        assert!(!backend.is_read_only());
        drop(backend);
        std::fs::remove_file(image).unwrap();
    }

    #[test]
    fn explicit_chain_rejects_raw_head_with_predecessor() {
        let spec = BlockBackendSpec::new(vec![
            BlockLayerSpec::new("base.raw", ImageType::Raw),
            BlockLayerSpec::new("head.raw", ImageType::Raw),
        ]);

        let error = match PreparedBlockBackend::open(&spec) {
            Ok(_) => panic!("raw head with a predecessor should be rejected"),
            Err(error) => error,
        };
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(error.to_string().contains("raw layer"));
    }

    #[test]
    fn explicit_chain_rejects_vmdk_before_opening_paths() {
        let spec =
            BlockBackendSpec::new(vec![BlockLayerSpec::new("missing.vmdk", ImageType::Vmdk)]);

        let error = match PreparedBlockBackend::open(&spec) {
            Ok(_) => panic!("VMDK should not enter the explicit raw/qcow path"),
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
            "libkrun-explicit-block-{test_name}-{}-{timestamp}.img",
            std::process::id()
        ))
    }
}
