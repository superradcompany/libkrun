// Copyright 2018 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE-BSD-3-Clause file.
//
// SPDX-License-Identifier: Apache-2.0 AND BSD-3-Clause
//
// Ported into libkrun from Cloud Hypervisor's `pci` crate. The port keeps only
// the ECAM (memory-mapped) config mechanism — aarch64 has no port-I/O, so
// `PciConfigIo` is dropped — and adapts `PciConfigMmio` to libkrun's
// `BusDevice` trait, which has no cross-vCPU `Barrier` return.

use std::collections::HashMap;
use std::ops::DerefMut;
use std::result;
use std::sync::{Arc, Mutex};

use crate::bus::BusDevice;

use super::configuration::{
    PciBridgeSubclass, PciClassCode, PciConfiguration, PciHeaderType,
};
use super::device::{DeviceRelocation, PciDevice};

/// Denotes the PCI device ID of a bus' root bridge device.
pub const PCI_ROOT_DEVICE_ID: u8 = 0;
/// Denotes the maximum number of PCI devices allowed on a bus. 32 per PCI spec.
pub const NUM_DEVICE_IDS: u8 = 32;

const VENDOR_ID_INTEL: u16 = 0x8086;
const DEVICE_ID_INTEL_VIRT_PCIE_HOST: u16 = 0x0d57;

/// Errors for the PCI root bus.
#[derive(Debug)]
pub enum PciRootError {
    /// Could not find an available device slot on the PCI bus.
    NoPciDeviceSlotAvailable,
    /// Invalid PCI device identifier provided.
    InvalidPciDeviceSlot(usize),
    /// Valid PCI device identifier but already used.
    AlreadyInUsePciDeviceSlot(usize),
}
pub type Result<T> = result::Result<T, PciRootError>;

/// Emulates the PCI Root bridge device.
pub struct PciRoot {
    /// Configuration space.
    config: PciConfiguration,
}

impl PciRoot {
    /// Create an empty PCI root bridge.
    pub fn new(config: Option<PciConfiguration>) -> Self {
        if let Some(config) = config {
            PciRoot { config }
        } else {
            PciRoot {
                config: PciConfiguration::new(
                    VENDOR_ID_INTEL,
                    DEVICE_ID_INTEL_VIRT_PCIE_HOST,
                    0,
                    PciClassCode::BridgeDevice,
                    &PciBridgeSubclass::HostBridge,
                    None,
                    PciHeaderType::Device,
                    0,
                    0,
                ),
            }
        }
    }
}

impl BusDevice for PciRoot {}

impl PciDevice for PciRoot {
    fn write_config_register(
        &mut self,
        reg_idx: usize,
        offset: u64,
        data: &[u8],
    ) -> Vec<super::device::BarReprogrammingParams> {
        self.config.write_config_register(reg_idx, offset, data)
    }

    fn read_config_register(&mut self, reg_idx: usize) -> u32 {
        self.config.read_reg(reg_idx)
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }

    fn id(&self) -> Option<String> {
        None
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DeviceIdState {
    Free,
    Reserved,
    Allocated,
}

pub struct PciBus {
    /// Devices attached to this bus.
    /// Device 0 is host bridge.
    devices: HashMap<u8, Arc<Mutex<dyn PciDevice>>>,
    device_reloc: Arc<dyn DeviceRelocation>,
    device_ids: [DeviceIdState; NUM_DEVICE_IDS as usize],
}

impl PciBus {
    pub fn new(pci_root: PciRoot, device_reloc: Arc<dyn DeviceRelocation>) -> Self {
        let mut devices: HashMap<u8, Arc<Mutex<dyn PciDevice>>> = HashMap::new();
        let mut device_ids = [DeviceIdState::Free; NUM_DEVICE_IDS as usize];

        devices.insert(PCI_ROOT_DEVICE_ID, Arc::new(Mutex::new(pci_root)));
        device_ids[PCI_ROOT_DEVICE_ID as usize] = DeviceIdState::Allocated;

        PciBus {
            devices,
            device_reloc,
            device_ids,
        }
    }

    pub fn add_device(&mut self, device_id: u8, device: Arc<Mutex<dyn PciDevice>>) -> Result<()> {
        self.devices.insert(device_id, device);
        Ok(())
    }

    /// Allocates a PCI device ID on the bus. If `id` is `None`, the next free
    /// device ID is allocated, else the requested ID is allocated.
    pub fn allocate_device_id(&mut self, id: Option<u8>) -> Result<u8> {
        if let Some(idx) = id.map(|i| i as usize) {
            if idx < NUM_DEVICE_IDS as usize {
                if self.device_ids[idx] == DeviceIdState::Allocated {
                    Err(PciRootError::AlreadyInUsePciDeviceSlot(idx))
                } else {
                    self.device_ids[idx] = DeviceIdState::Allocated;
                    Ok(idx as u8)
                }
            } else {
                Err(PciRootError::InvalidPciDeviceSlot(idx))
            }
        } else {
            for (idx, device_id) in self.device_ids.iter_mut().enumerate() {
                if *device_id == DeviceIdState::Free {
                    *device_id = DeviceIdState::Allocated;
                    return Ok(idx as u8);
                }
            }
            Err(PciRootError::NoPciDeviceSlotAvailable)
        }
    }
}

/// Emulates PCI memory-mapped configuration access mechanism (ECAM).
pub struct PciConfigMmio {
    pci_bus: Arc<Mutex<PciBus>>,
}

impl PciConfigMmio {
    pub fn new(pci_bus: Arc<Mutex<PciBus>>) -> Self {
        PciConfigMmio { pci_bus }
    }

    fn config_space_read(&self, config_address: u32) -> u32 {
        let (bus, device, _function, register) = parse_mmio_config_address(config_address);

        // Only support one bus.
        if bus != 0 {
            return 0xffff_ffff;
        }

        self.pci_bus
            .lock()
            .unwrap()
            .devices
            .get(&(device as u8))
            .map_or(0xffff_ffff, |d| {
                d.lock().unwrap().read_config_register(register)
            })
    }

    fn config_space_write(&mut self, config_address: u32, offset: u64, data: &[u8]) {
        if offset as usize + data.len() > 4 {
            return;
        }

        let (bus, device, _function, register) = parse_mmio_config_address(config_address);

        // Only support one bus.
        if bus != 0 {
            return;
        }

        let pci_bus = self.pci_bus.lock().unwrap();
        if let Some(d) = pci_bus.devices.get(&(device as u8)) {
            let mut device = d.lock().unwrap();

            // Update the register value
            let bar_reprogram = device.write_config_register(register, offset, data);

            // Move the device's BAR if needed
            for params in &bar_reprogram {
                if let Err(e) = pci_bus.device_reloc.move_bar(
                    params.old_base,
                    params.new_base,
                    params.len,
                    device.deref_mut(),
                    params.region_type,
                ) {
                    warn!(
                        "Failed moving device BAR: {}: 0x{:x}->0x{:x}(0x{:x}), keeping old BAR",
                        e, params.old_base, params.new_base, params.len
                    );
                    device.restore_bar_addr(params);
                }
            }
        }
    }
}

impl BusDevice for PciConfigMmio {
    fn read(&mut self, _vcpuid: u64, offset: u64, data: &mut [u8]) {
        // Only allow reads to the register boundary.
        let start = offset as usize % 4;
        let end = start + data.len();
        if end > 4 || offset > u64::from(u32::MAX) {
            for d in data {
                *d = 0xff;
            }
            return;
        }

        let value = self.config_space_read(offset as u32);
        for i in start..end {
            data[i - start] = (value >> (i * 8)) as u8;
        }
    }

    fn write(&mut self, _vcpuid: u64, offset: u64, data: &[u8]) {
        if offset > u64::from(u32::MAX) {
            return;
        }
        self.config_space_write(offset as u32, offset % 4, data);
    }
}

fn shift_and_mask(value: u32, offset: usize, mask: u32) -> usize {
    ((value >> offset) & mask) as usize
}

// Parse the MMIO address offset to a (bus, device, function, register) tuple.
// See section 7.2.2 PCI Express Enhanced Configuration Access Mechanism (ECAM)
// from the Pci Express Base Specification Revision 5.0 Version 1.0.
fn parse_mmio_config_address(config_address: u32) -> (usize, usize, usize, usize) {
    const BUS_NUMBER_OFFSET: usize = 20;
    const BUS_NUMBER_MASK: u32 = 0x00ff;
    const DEVICE_NUMBER_OFFSET: usize = 15;
    const DEVICE_NUMBER_MASK: u32 = 0x1f;
    const FUNCTION_NUMBER_OFFSET: usize = 12;
    const FUNCTION_NUMBER_MASK: u32 = 0x07;
    const REGISTER_NUMBER_OFFSET: usize = 2;
    const REGISTER_NUMBER_MASK: u32 = 0x3ff;

    (
        shift_and_mask(config_address, BUS_NUMBER_OFFSET, BUS_NUMBER_MASK),
        shift_and_mask(config_address, DEVICE_NUMBER_OFFSET, DEVICE_NUMBER_MASK),
        shift_and_mask(config_address, FUNCTION_NUMBER_OFFSET, FUNCTION_NUMBER_MASK),
        shift_and_mask(config_address, REGISTER_NUMBER_OFFSET, REGISTER_NUMBER_MASK),
    )
}
