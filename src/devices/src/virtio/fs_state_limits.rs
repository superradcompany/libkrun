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
