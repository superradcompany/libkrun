// SPDX-License-Identifier: Apache-2.0
//
// VfioPciDevice — a passed-through PCI device presented to the guest as a
// `PciDevice`. Ported (trimmed) from Cloud Hypervisor's `pci/vfio.rs`. Config
// space is served straight from the physical device via VFIO pread/pwrite,
// EXCEPT the BAR registers, which are emulated so the guest sees guest-side BAR
// addresses/sizes. BAR mmap→memslot (step 6) and interrupts/MSI-X (steps 8-9)
// build on this.

use std::any::Any;

use arch::aarch64::layout::{PCIE_MMIO32_BASE, PCIE_MMIO64_BASE};

use super::vfio::{VfioDevice, VfioError};
use crate::pci::configuration::{
    PciBarConfiguration, PciBarPrefetchable, PciBarRegionType, PciClassCode, PciConfiguration,
    PciHeaderType, PciSubclass,
};
use crate::pci::device::{BarReprogrammingParams, PciDevice};

// Config-space dword indices.
const BAR0_REG_IDX: usize = 4;
const NUM_BAR_REGS: usize = 6;
const ROM_BAR_REG_IDX: usize = 12;
const HEADER_TYPE_REG_IDX: usize = 3;
const MULTIFUNCTION_MASK: u32 = 0xff7f_ffff;

// BAR flag bits (low bits of a memory/IO BAR register).
const PCI_BAR_IO_SPACE: u32 = 0x0000_0001;
const PCI_BAR_MEM_TYPE_MASK: u32 = 0x0000_0006;
const PCI_BAR_MEM_TYPE_64: u32 = 0x0000_0004;
const PCI_BAR_PREFETCHABLE: u32 = 0x0000_0008;

// VFIO PCI region index of BAR0 (BAR N is region index N).
const VFIO_PCI_BAR0_REGION_INDEX: u32 = 0;

/// A dummy subclass — the emulated class register is never returned to the
/// guest (class reads are passthrough), so its value is irrelevant.
struct GenericSubclass;
impl PciSubclass for GenericSubclass {
    fn get_register_value(&self) -> u8 {
        0x80
    }
}

/// One BAR window of a passed-through device.
#[derive(Clone, Copy, Debug)]
pub struct MmioRegion {
    /// Guest physical base allocated for this BAR.
    pub start: u64,
    /// BAR size in bytes.
    pub length: u64,
    /// BAR index (0..5).
    pub index: u32,
    pub region_type: PciBarRegionType,
}

pub struct VfioPciDevice {
    id: String,
    vfio: VfioDevice,
    /// Emulated config — only the BAR registers are authoritative here.
    config: PciConfiguration,
    mmio_regions: Vec<MmioRegion>,
}

impl VfioPciDevice {
    /// Open the device at sysfs BDF `bdf` and allocate its BARs from the guest
    /// PCIe MMIO windows.
    pub fn new(id: String, bdf: &str) -> Result<Self, VfioError> {
        let vfio = VfioDevice::new(bdf)?;

        // Real vendor/device just for a tidy shadow; never read by the guest.
        let id_dword = vfio.read_config_dword(0);
        let vendor_id = (id_dword & 0xffff) as u16;
        let device_id = (id_dword >> 16) as u16;

        let config = PciConfiguration::new(
            vendor_id,
            device_id,
            0,
            PciClassCode::Other,
            &GenericSubclass,
            None,
            PciHeaderType::Device,
            0,
            0,
        );

        let mut dev = VfioPciDevice {
            id,
            vfio,
            config,
            mmio_regions: Vec::new(),
        };
        dev.allocate_bars()?;
        Ok(dev)
    }

    /// The allocated BAR windows (for the caller to map as KVM memslots — step 6).
    #[allow(dead_code)]
    pub fn mmio_regions(&self) -> &[MmioRegion] {
        &self.mmio_regions
    }

    /// The underlying VFIO device (for mmap in step 6).
    #[allow(dead_code)]
    pub fn vfio(&self) -> &VfioDevice {
        &self.vfio
    }

    fn allocate_bars(&mut self) -> Result<(), VfioError> {
        // Simple bump cursors within the guest PCIe MMIO windows. One device for
        // now, so no global allocator is needed.
        let mut mmio32_cursor = PCIE_MMIO32_BASE;
        let mut mmio64_cursor = PCIE_MMIO64_BASE;

        let mut bar_id = 0usize;
        while bar_id < NUM_BAR_REGS {
            let bar_offset = 0x10 + (bar_id * 4) as u64;
            let flags = self.vfio.read_config_dword(bar_offset);

            let region_index = VFIO_PCI_BAR0_REGION_INDEX + bar_id as u32;
            let size = self.vfio.region(region_index).map(|r| r.size).unwrap_or(0);
            if size == 0 {
                bar_id += 1;
                continue;
            }

            let is_io = flags & PCI_BAR_IO_SPACE != 0;
            let is_64 = !is_io && (flags & PCI_BAR_MEM_TYPE_MASK) == PCI_BAR_MEM_TYPE_64;
            let prefetchable = if !is_io && (flags & PCI_BAR_PREFETCHABLE != 0) {
                PciBarPrefetchable::Prefetchable
            } else {
                PciBarPrefetchable::NotPrefetchable
            };

            // aarch64 has no port I/O aperture — skip IO BARs (GPUs use memory BARs).
            if is_io {
                bar_id += 1;
                continue;
            }

            let region_type = if is_64 {
                PciBarRegionType::Memory64BitRegion
            } else {
                PciBarRegionType::Memory32BitRegion
            };

            let addr = if is_64 {
                let a = align_up(mmio64_cursor, size);
                mmio64_cursor = a + size;
                a
            } else {
                let a = align_up(mmio32_cursor, size);
                mmio32_cursor = a + size;
                a
            };

            let bar_cfg = PciBarConfiguration::new(bar_id, size, region_type, prefetchable)
                .set_address(addr);
            if let Err(e) = self.config.add_pci_bar(&bar_cfg) {
                warn!("vfio-pci {}: failed to add BAR {bar_id}: {e}", self.id);
                bar_id += 1;
                continue;
            }

            debug!(
                "vfio-pci {}: BAR{bar_id} {:?}{} size {:#x} -> guest {:#x}",
                self.id,
                region_type,
                if matches!(prefetchable, PciBarPrefetchable::Prefetchable) {
                    " pref"
                } else {
                    ""
                },
                size,
                addr
            );

            self.mmio_regions.push(MmioRegion {
                start: addr,
                length: size,
                index: bar_id as u32,
                region_type,
            });

            bar_id += if is_64 { 2 } else { 1 };
        }

        Ok(())
    }

    fn is_bar_reg(reg_idx: usize) -> bool {
        (BAR0_REG_IDX..BAR0_REG_IDX + NUM_BAR_REGS).contains(&reg_idx)
            || reg_idx == ROM_BAR_REG_IDX
    }
}

fn align_up(value: u64, align: u64) -> u64 {
    debug_assert!(align.is_power_of_two());
    (value + align - 1) & !(align - 1)
}

impl PciDevice for VfioPciDevice {
    fn write_config_register(
        &mut self,
        reg_idx: usize,
        offset: u64,
        data: &[u8],
    ) -> Vec<BarReprogrammingParams> {
        // BAR writes are captured by the emulated config (so guest BAR
        // programming/reprogramming is under our control). Everything else is
        // passed straight through to the physical device.
        if Self::is_bar_reg(reg_idx) {
            return self.config.write_config_register(reg_idx, offset, data);
        }
        self.vfio
            .write_config(reg_idx as u64 * 4 + offset, data);
        Vec::new()
    }

    fn read_config_register(&mut self, reg_idx: usize) -> u32 {
        // BARs come from the emulated config; the rest passes through.
        if Self::is_bar_reg(reg_idx) {
            return self.config.read_reg(reg_idx);
        }
        let mut value = self.vfio.read_config_dword(reg_idx as u64 * 4);
        // Hide the multi-function bit so the guest doesn't probe phantom functions.
        if reg_idx == HEADER_TYPE_REG_IDX {
            value &= MULTIFUNCTION_MASK;
        }
        value
    }

    fn read_bar(&mut self, base: u64, offset: u64, data: &mut [u8]) {
        // Trapped-BAR path (used once the MSI-X page is trapped; mmappable BARs
        // go through memslots and never reach here). Dispatch to the owning BAR.
        if let Some(region) = self.find_region(base + offset) {
            let region_offset = base + offset - region.start;
            let _ = self.vfio.region_read(region.index, region_offset, data);
        }
    }

    fn write_bar(&mut self, base: u64, offset: u64, data: &[u8]) {
        if let Some(region) = self.find_region(base + offset) {
            let region_offset = base + offset - region.start;
            let _ = self.vfio.region_write(region.index, region_offset, data);
        }
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }

    fn id(&self) -> Option<String> {
        Some(self.id.clone())
    }
}

impl VfioPciDevice {
    fn find_region(&self, addr: u64) -> Option<MmioRegion> {
        self.mmio_regions
            .iter()
            .find(|r| addr >= r.start && addr < r.start + r.length)
            .copied()
    }
}
