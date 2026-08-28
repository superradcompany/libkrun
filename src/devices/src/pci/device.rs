// Copyright 2018 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE-BSD-3-Clause file.
//
// SPDX-License-Identifier: Apache-2.0 AND BSD-3-Clause
//
// Ported into libkrun from Cloud Hypervisor's `pci` crate. libkrun's simpler
// device model has no cross-vCPU `Barrier` on config writes and no
// vm-allocator, so the `allocate_bars`/`free_bars` allocator hooks are dropped
// here — the VFIO device (step 5) allocates its own BAR windows from the
// libkrun-side MMIO allocator — and `write_config_register` returns just the
// list of BAR reprogramming requests.

use std::any::Any;
use std::io;
use std::result;

use super::configuration::PciBarRegionType;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BarReprogrammingParams {
    pub old_base: u64,
    pub new_base: u64,
    pub len: u64,
    pub region_type: PciBarRegionType,
}

pub trait PciDevice: Send {
    /// Sets a register in the configuration space.
    /// * `reg_idx` - The index of the config register to modify.
    /// * `offset` - Offset into the register.
    fn write_config_register(
        &mut self,
        reg_idx: usize,
        offset: u64,
        data: &[u8],
    ) -> Vec<BarReprogrammingParams>;
    /// Gets a register from the configuration space.
    /// * `reg_idx` - The index of the config register to read.
    fn read_config_register(&mut self, reg_idx: usize) -> u32;
    /// Reads from a BAR region mapped into the device.
    /// * `base` - The guest address of the BAR base.
    /// * `offset` - Offset into the BAR.
    /// * `data` - Filled with the data from `addr`.
    fn read_bar(&mut self, _base: u64, _offset: u64, _data: &mut [u8]) {}
    /// Writes to a BAR region mapped into the device.
    fn write_bar(&mut self, _base: u64, _offset: u64, _data: &[u8]) {}
    /// Relocates the BAR to a different address in guest address space.
    fn move_bar(&mut self, _old_base: u64, _new_base: u64) -> result::Result<(), io::Error> {
        Ok(())
    }
    /// Restore BAR address in config space after a failed move_bar.
    /// This rolls back the address update made by detect_bar_reprogramming()
    /// so that the config register stays consistent with the MMIO bus mapping.
    fn restore_bar_addr(&mut self, _params: &BarReprogrammingParams) {}
    /// Provides a mutable reference to the Any trait. This is useful to let
    /// the caller have access to the underlying type behind the trait.
    fn as_any_mut(&mut self) -> &mut dyn Any;
    /// Optionally returns a unique identifier.
    fn id(&self) -> Option<String>;
}

/// This trait defines a set of functions which can be triggered whenever a
/// PCI device is modified in any way.
pub trait DeviceRelocation: Send + Sync {
    /// The BAR needs to be moved to a different location in the guest address
    /// space. This follows a decision from the software running in the guest.
    fn move_bar(
        &self,
        old_base: u64,
        new_base: u64,
        len: u64,
        pci_dev: &mut dyn PciDevice,
        region_type: PciBarRegionType,
    ) -> result::Result<(), io::Error>;
}
