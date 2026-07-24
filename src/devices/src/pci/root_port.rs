// SPDX-License-Identifier: Apache-2.0
//
// A minimal emulated PCIe root-port bridge (type-1 config header) placed between
// the host bridge (bus 0) and a downstream passed-through device (bus 1).
//
// NVIDIA's RM refuses to initialize a GPU that hangs directly off the root bus:
// on Linux `pci_dev->bus->self` is NULL for a root-bus device, so
// `objClInitPcieChipset` logs "Unable to get PCI port handles" and
// `osVerifySystemEnvironment` bails. Real hardware (and QEMU/VFIO passthrough)
// always place the GPU behind a PCIe root port; this device provides that port.

use std::any::Any;

use crate::bus::BusDevice;

use super::device::{BarReprogrammingParams, PciDevice};

const NUM_REGS: usize = 1024;

/// PCIe root port. Header type 1, with a PCI Express capability advertising
/// device/port type = Root Port. The downstream bus number is programmable by
/// the guest (writable secondary/subordinate bus registers) and seeded so that
/// routing works before the guest re-enumerates.
pub struct PciRootPort {
    regs: [u32; NUM_REGS],
    writable: [u32; NUM_REGS],
}

impl PciRootPort {
    /// Create a root port whose secondary (and subordinate) bus is `secondary_bus`.
    pub fn new(secondary_bus: u8) -> Self {
        let mut regs = [0u32; NUM_REGS];
        let mut writable = [0u32; NUM_REGS];

        // 0x00 Vendor/Device: QEMU PCI Express Root Port (1b36:000c) — a
        // standard, widely-recognized root port. (Linux binds the pcieport
        // driver by class, not vendor, so the exact ID is not critical.)
        regs[0] = (0x000c << 16) | 0x1b36;
        // 0x04 Status/Command. Status bit 4 (Capabilities List present) set so
        // the guest walks the capability pointer at 0x34. Command low 16 r/w.
        regs[1] = 0x0010_0000;
        writable[1] = 0x0000_ffff;
        // 0x08 Class: PCI-to-PCI bridge (class 0x06, subclass 0x04, prog-if 0x00).
        regs[2] = 0x0604_0000;
        // 0x0C Header type 1 (bridge) in byte 2; cacheline size r/w.
        regs[3] = 0x0001_0000;
        writable[3] = 0x0000_00ff;
        // 0x18 Primary=0 / Secondary=secondary_bus / Subordinate=secondary_bus /
        // secondary latency timer=0. Bus-number bytes are writable so the guest
        // can (re)assign them during enumeration.
        regs[6] = ((secondary_bus as u32) << 16) | ((secondary_bus as u32) << 8);
        writable[6] = 0x00ff_ffff;
        // 0x1C Secondary status / I/O limit / I/O base. Disable I/O forwarding
        // (base 0xf0 > limit 0x00); I/O base/limit upper nibbles r/w.
        regs[7] = 0x0000_00f0;
        writable[7] = 0x0000_f0f0;
        // 0x20 Memory base/limit (non-prefetchable) — guest programs it to cover
        // the downstream 32-bit BAR. Bits [15:4] of base and limit r/w.
        writable[8] = 0xfff0_fff0;
        // 0x24 Prefetchable memory base/limit; low nibble 0x1 => 64-bit capable.
        regs[9] = 0x0001_0001;
        writable[9] = 0xfff0_fff0;
        // 0x28 / 0x2C Prefetchable base/limit upper 32 bits — r/w.
        writable[10] = 0xffff_ffff;
        writable[11] = 0xffff_ffff;
        // 0x30 I/O base/limit upper 16 bits — r/w.
        writable[12] = 0xffff_ffff;
        // 0x34 Capability pointer -> PCI Express cap at 0x40.
        regs[13] = 0x0000_0040;
        // 0x3C Bridge control (r/w) / interrupt pin / interrupt line (r/w).
        writable[15] = 0xffff_00ff;

        // --- PCI Express capability at 0x40 (reg 16) ---
        // byte0 cap-id 0x10, byte1 next 0x00, bytes2-3 PCIe caps:
        // version 2 (bits 3:0), device/port type Root Port 0x4 (bits 7:4).
        regs[16] = 0x0042_0010;
        // 0x44 Device Capabilities (reg 17): 0.
        // 0x48 Device Status/Control (reg 18): control r/w.
        writable[18] = 0x0000_ffff;
        // 0x4C Link Capabilities (reg 19): max link speed 3 (8 GT/s), width x8.
        regs[19] = 0x0000_0083;
        // 0x50 Link Status/Control (reg 20): current speed 3, width x8 in the
        // status half; control half r/w.
        regs[20] = 0x0083_0000;
        writable[20] = 0x0000_ffff;
        // 0x54 Slot Capabilities (reg 21): 0.
        // 0x58 Slot Status/Control (reg 22): control r/w.
        writable[22] = 0x0000_ffff;
        // 0x5C Root Control/Capabilities (reg 23): r/w control bits.
        writable[23] = 0x0000_ffff;
        // Root Status / v2 (Device/Link Cap 2) registers left 0.

        PciRootPort { regs, writable }
    }

    /// The currently-programmed secondary bus number (config 0x19).
    pub fn secondary_bus(&self) -> u8 {
        ((self.regs[6] >> 8) & 0xff) as u8
    }
}

impl BusDevice for PciRootPort {}

impl PciDevice for PciRootPort {
    fn write_config_register(
        &mut self,
        reg_idx: usize,
        offset: u64,
        data: &[u8],
    ) -> Vec<BarReprogrammingParams> {
        if reg_idx >= NUM_REGS {
            return Vec::new();
        }
        let mask = self.writable[reg_idx];
        let mut reg = self.regs[reg_idx];
        for (i, b) in data.iter().enumerate() {
            let byte = offset as usize + i;
            if byte >= 4 {
                break;
            }
            let shift = byte * 8;
            let bmask = mask & (0xffu32 << shift);
            reg = (reg & !bmask) | (((u32::from(*b)) << shift) & bmask);
        }
        self.regs[reg_idx] = reg;
        Vec::new()
    }

    fn read_config_register(&mut self, reg_idx: usize) -> u32 {
        *self.regs.get(reg_idx).unwrap_or(&0xffff_ffff)
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }

    fn id(&self) -> Option<String> {
        None
    }
}
