// Copyright 2026 Microsandbox Authors. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Size limits shared by the virtio-fs device and the typed device state codec.

//--------------------------------------------------------------------------------------------------
// Constants
//--------------------------------------------------------------------------------------------------

/// Default maximum size of the backend state carried by one virtio-fs device.
pub const DEFAULT_MAX_FS_BACKEND_STATE_BYTES: usize = 4 * 1024 * 1024;
/// Size of the fixed header preceding the backend state in a virtio-fs device state.
pub const FS_DEVICE_STATE_HEADER_BYTES: usize = 22;

//--------------------------------------------------------------------------------------------------
// Types
//--------------------------------------------------------------------------------------------------

/// Limits shared by VM device capture and standalone state codecs.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DeviceStateLimits {
    fs_state_limit: usize,
}

//--------------------------------------------------------------------------------------------------
// Methods
//--------------------------------------------------------------------------------------------------

impl DeviceStateLimits {
    /// Set the maximum filesystem backend payload per device, in bytes.
    ///
    /// Device headers and transport metadata receive additional bounded space.
    /// The limit must fit a `u32`; larger values cannot be captured or restored.
    pub fn with_fs_state_limit(mut self, bytes: usize) -> Self {
        self.fs_state_limit = bytes;
        self
    }

    /// Maximum filesystem backend payload per device, in bytes.
    pub fn fs_state_limit(self) -> usize {
        self.fs_state_limit
    }
}

//--------------------------------------------------------------------------------------------------
// Trait Implementations
//--------------------------------------------------------------------------------------------------

impl Default for DeviceStateLimits {
    fn default() -> Self {
        Self {
            fs_state_limit: DEFAULT_MAX_FS_BACKEND_STATE_BYTES,
        }
    }
}
