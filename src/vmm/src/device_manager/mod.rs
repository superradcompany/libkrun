// Copyright 2018 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0
//
// Portions Copyright 2017 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the THIRD-PARTY file.

/// Legacy Device Manager.
#[cfg(not(target_os = "windows"))]
pub mod legacy;

/// Device Shared Memory Region Manager.
pub mod shm;

/// Memory Mapped I/O Manager.
#[cfg(target_os = "linux")]
pub mod kvm;
#[cfg(target_os = "linux")]
pub use self::kvm::mmio;
#[cfg(any(target_os = "macos", target_os = "windows"))]
pub mod hvf;
#[cfg(any(target_os = "macos", target_os = "windows"))]
pub use self::hvf::mmio;
