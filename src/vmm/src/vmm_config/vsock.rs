// Copyright 2018 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use std::collections::HashMap;
use std::fmt;
use std::path::PathBuf;
#[cfg(not(target_os = "windows"))]
use std::sync::{Arc, Mutex};

#[cfg(not(target_os = "windows"))]
use devices::virtio::vsock::VsockPortBackend;
#[cfg(not(target_os = "windows"))]
use devices::virtio::{TsiFlags, Vsock, VsockError};

#[cfg(target_os = "windows")]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct TsiFlags(u32);

#[cfg(target_os = "windows")]
impl TsiFlags {
    pub const HIJACK_INET: Self = Self(1 << 0);
    pub const HIJACK_UNIX: Self = Self(1 << 1);

    pub fn empty() -> Self {
        Self(0)
    }

    pub fn from_bits(bits: u32) -> Option<Self> {
        let supported = Self::HIJACK_INET.0 | Self::HIJACK_UNIX.0;
        if bits & !supported == 0 {
            Some(Self(bits))
        } else {
            None
        }
    }

    pub fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    pub fn is_empty(self) -> bool {
        self.0 == 0
    }
}

#[cfg(target_os = "windows")]
impl std::ops::BitOrAssign for TsiFlags {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

#[cfg(not(target_os = "windows"))]
type MutexVsock = Arc<Mutex<Vsock>>;
#[cfg(target_os = "windows")]
type MutexVsock = ();

/// Errors associated with `NetworkInterfaceConfig`.
#[derive(Debug)]
pub enum VsockConfigError {
    /// Failed to create the vsock device.
    #[cfg(not(target_os = "windows"))]
    CreateVsockDevice(VsockError),
    #[cfg(target_os = "windows")]
    UnsupportedOnWindows,
}

impl fmt::Display for VsockConfigError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        use self::VsockConfigError::*;
        match *self {
            #[cfg(not(target_os = "windows"))]
            CreateVsockDevice(ref e) => write!(f, "Cannot create vsock device: {e:?}"),
            #[cfg(target_os = "windows")]
            UnsupportedOnWindows => write!(f, "Vsock device support is not implemented on Windows"),
        }
    }
}

type Result<T> = std::result::Result<T, VsockConfigError>;

/// This struct represents the strongly typed equivalent of the json body
/// from vsock related requests.
#[derive(Clone)]
pub struct VsockDeviceConfig {
    /// ID of the vsock device.
    pub vsock_id: String,
    /// A 32-bit Context Identifier (CID) used to identify the guest.
    pub guest_cid: u32,
    /// An optional map of host to guest port mappings.
    pub host_port_map: Option<HashMap<u16, u16>>,
    /// An optional map of guest port to host UNIX domain sockets for IPC.
    pub unix_ipc_port_map: Option<HashMap<u32, (PathBuf, bool)>>,
    /// Optional custom in-process services keyed by host vsock port.
    #[cfg(not(target_os = "windows"))]
    pub custom_port_map: Option<HashMap<u32, Arc<dyn VsockPortBackend>>>,
    /// TSI feature flags
    pub tsi_flags: TsiFlags,
}

impl fmt::Debug for VsockDeviceConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut debug = f.debug_struct("VsockDeviceConfig");
        debug
            .field("vsock_id", &self.vsock_id)
            .field("guest_cid", &self.guest_cid)
            .field("host_port_map", &self.host_port_map)
            .field("unix_ipc_port_map", &self.unix_ipc_port_map);
        #[cfg(not(target_os = "windows"))]
        debug.field(
            "custom_ports",
            &self
                .custom_port_map
                .as_ref()
                .map(|services| services.keys().collect::<Vec<_>>()),
        );
        debug.field("tsi_flags", &self.tsi_flags).finish()
    }
}

struct VsockWrapper {
    vsock: MutexVsock,
}

/// A builder of Vsock from 'VsockDeviceConfig'.
#[derive(Default)]
pub struct VsockBuilder {
    inner: Option<VsockWrapper>,
    tsi_flags: TsiFlags,
}

impl VsockBuilder {
    /// Creates an empty Vsock.
    pub fn new() -> Self {
        Self {
            inner: None,
            tsi_flags: TsiFlags::empty(),
        }
    }

    /// Inserts a Vsock in the store.
    /// If an entry already exists, it will overwrite it.
    #[cfg(not(target_os = "windows"))]
    pub fn insert(&mut self, cfg: VsockDeviceConfig) -> Result<()> {
        self.tsi_flags = cfg.tsi_flags;
        self.inner = Some(VsockWrapper {
            vsock: Arc::new(Mutex::new(Self::create_vsock(cfg)?)),
        });
        Ok(())
    }

    /// Inserts a placeholder Vsock configuration on Windows.
    #[cfg(target_os = "windows")]
    pub fn insert(&mut self, cfg: VsockDeviceConfig) -> Result<()> {
        self.tsi_flags = cfg.tsi_flags;
        self.inner = Some(VsockWrapper { vsock: () });
        Ok(())
    }

    /// Provides a reference to the Vsock if present.
    pub fn get(&self) -> Option<&MutexVsock> {
        self.inner.as_ref().map(|pair| &pair.vsock)
    }

    pub fn tsi_flags(&self) -> TsiFlags {
        self.tsi_flags
    }

    /// Creates a Vsock device from a VsockDeviceConfig.
    #[cfg(not(target_os = "windows"))]
    pub fn create_vsock(cfg: VsockDeviceConfig) -> Result<Vsock> {
        Vsock::new(
            u64::from(cfg.guest_cid),
            cfg.host_port_map,
            cfg.unix_ipc_port_map,
            cfg.custom_port_map,
            cfg.tsi_flags,
        )
        .map_err(VsockConfigError::CreateVsockDevice)
    }
}

#[cfg(all(test, not(target_os = "windows")))]
pub(crate) mod tests {
    use super::*;
    use utils::tempfile::TempFile;

    // Placeholder for the path where a socket file will be created.
    // The socket file will be removed when the scope ends.
    pub(crate) struct TempSockFile {
        path: String,
    }

    impl TempSockFile {
        pub fn new(tmp_file: TempFile) -> Self {
            TempSockFile {
                path: String::from(tmp_file.as_path().to_str().unwrap()),
            }
        }
    }

    impl Drop for TempSockFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
        }
    }

    pub(crate) fn default_config(_tmp_sock_file: &TempSockFile) -> VsockDeviceConfig {
        let vsock_dev_id = "vsock";
        VsockDeviceConfig {
            vsock_id: vsock_dev_id.to_string(),
            guest_cid: 3,
            host_port_map: None,
            unix_ipc_port_map: None,
            custom_port_map: None,
            tsi_flags: TsiFlags::empty(),
        }
    }

    #[test]
    fn test_vsock_insert() {
        let mut store = VsockBuilder::new();
        let tmp_sock_file = TempSockFile::new(TempFile::new().unwrap());
        let mut vsock_config = default_config(&tmp_sock_file);

        store.insert(vsock_config.clone()).unwrap();
        let vsock = store.get().unwrap();
        assert_eq!(vsock.lock().unwrap().id(), &vsock_config.vsock_id);

        let new_cid = vsock_config.guest_cid + 1;
        vsock_config.guest_cid = new_cid;
        store.insert(vsock_config).unwrap();
        let vsock = store.get().unwrap();
        assert_eq!(vsock.lock().unwrap().cid(), new_cid as u64);
    }

    #[test]
    fn test_error_messages() {
        use super::VsockConfigError::*;
        use std::io;

        let err = CreateVsockDevice(devices::virtio::VsockError::EventFd(
            io::Error::from_raw_os_error(0),
        ));
        let _ = format!("{err}{err:?}");
    }
}
