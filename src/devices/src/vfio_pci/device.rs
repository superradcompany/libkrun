// SPDX-License-Identifier: Apache-2.0
//
// VfioPciDevice — a passed-through PCI device presented to the guest as a
// `PciDevice`. Ported (trimmed) from Cloud Hypervisor's `pci/vfio.rs`. Config
// space is served straight from the physical device via VFIO pread/pwrite,
// EXCEPT the BAR registers, which are emulated so the guest sees guest-side BAR
// addresses/sizes. BAR mmap→memslot (step 6) and interrupts/MSI-X (steps 8-9)
// build on this.

use std::any::Any;
use std::os::unix::io::{AsRawFd, RawFd};

use arch::aarch64::layout::{PCIE_MMIO32_BASE, PCIE_MMIO64_BASE};
use crossbeam_channel::{unbounded, Sender};
use kvm_bindings::{
    kvm_irq_routing_entry, kvm_irq_routing_entry__bindgen_ty_1, kvm_irq_routing_irqchip,
    kvm_irq_routing_msi, kvm_irq_routing_msi__bindgen_ty_1, KVM_IRQ_ROUTING_IRQCHIP,
    KVM_IRQ_ROUTING_MSI, KVM_MSI_VALID_DEVID,
};
use kvm_ioctls::VmFd;
use utils::eventfd::EventFd;
use utils::worker_message::WorkerMessage;

use super::msix::MsixState;
use super::vfio::{VfioDevice, VfioError};
use crate::pci::configuration::{
    PciBarConfiguration, PciBarPrefetchable, PciBarRegionType, PciClassCode, PciConfiguration,
    PciHeaderType, PciSubclass,
};
use crate::pci::device::{BarReprogrammingParams, PciDevice};

// VFIO PCI interrupt index for MSI-X (VFIO_PCI_MSIX_IRQ_INDEX).
const VFIO_PCI_MSIX_IRQ_INDEX: u32 = 2;

// Number of SPIs in the guest GICv3 (nr_irqs 224 − 32 private). Installing an
// explicit KVM GSI routing table replaces the kernel's default SPI routing, so
// we must re-emit an identity IRQCHIP route for every SPI to keep the virtio /
// legacy interrupts working, alongside the MSI routes. Kept in sync with
// `KvmGicV3::new` (`(IRQ_MAX+1).div_ceil(32)*32`).
const AARCH64_NR_SPIS: u32 = 192;

// Base GSI for MSI-X vectors: immediately above the SPI GSI range so it never
// collides with a virtio SPI route.
const MSI_GSI_BASE: u32 = AARCH64_NR_SPIS;

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
    /// Parsed MSI-X capability + shadow table, if the device has MSI-X.
    msix: Option<MsixState>,
    /// One eventfd (irqfd) per MSI-X vector, created and bound to a GSI by the
    /// builder. The physical device signals `msix_eventfds[i]` for vector `i`.
    msix_eventfds: Vec<EventFd>,
    /// GSI assigned to MSI-X vector 0 (vector `i` uses `msix_gsi_base + i`).
    msix_gsi_base: u32,
    /// PCI requester id (bus/dev/fn) used as the ITS device id in MSI routing.
    devid: u32,
    /// Channel to the VMM worker for `KVM_SET_GSI_ROUTING` from the vCPU thread.
    irq_sender: Option<Sender<WorkerMessage>>,
    /// Whether the physical MSI-X vectors are currently armed (VFIO SET_IRQS).
    msix_armed: bool,
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
            msix: None,
            msix_eventfds: Vec::new(),
            msix_gsi_base: 0,
            devid: 0,
            irq_sender: None,
            msix_armed: false,
        };
        dev.allocate_bars()?;
        dev.msix = MsixState::parse(&dev.vfio);
        if let Some(msix) = &dev.msix {
            debug!(
                "vfio-pci {}: MSI-X cap @ {:#x}, {} vectors, table BAR{}+{:#x}",
                dev.id, msix.cap_offset, msix.table_size, msix.table_bir, msix.table_offset
            );
        }
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
        // Intercept the MSI-X capability control dword: emulate enable/mask
        // ourselves rather than forwarding to the device (VFIO owns hardware
        // MSI-X enablement).
        if let Some(msix) = &self.msix {
            if reg_idx as u64 == msix.cap_offset / 4 {
                self.write_msix_ctl(offset, data);
                return Vec::new();
            }
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
        // Overlay our shadow MSI-X message control (enable/mask) so the guest
        // sees the state we emulate, not the device's.
        if let Some(msix) = &self.msix {
            if reg_idx as u64 == msix.cap_offset / 4 {
                value = (value & 0x0000_ffff) | ((msix.msg_ctl as u32) << 16);
            }
        }
        value
    }

    fn read_bar(&mut self, base: u64, offset: u64, data: &mut [u8]) {
        // Trapped-BAR path: mmappable BAR bytes go through memslots and never
        // reach here; only the trapped MSI-X table page (and any non-mmappable
        // BAR) does. Serve MSI-X table reads from the shadow table.
        if let Some(region) = self.find_region(base + offset) {
            let region_offset = base + offset - region.start;
            if let Some(msix) = &self.msix {
                if msix.table_accessed(region.index, region_offset, data.len() as u64) {
                    msix.read_table(region_offset, data);
                    return;
                }
            }
            let _ = self.vfio.region_read(region.index, region_offset, data);
        }
    }

    fn write_bar(&mut self, base: u64, offset: u64, data: &[u8]) {
        if let Some(region) = self.find_region(base + offset) {
            let region_offset = base + offset - region.start;
            let is_table = self
                .msix
                .as_ref()
                .is_some_and(|m| m.table_accessed(region.index, region_offset, data.len() as u64));
            if is_table {
                // Capture the guest's per-vector message into the shadow table
                // and re-push routing.
                self.msix.as_mut().unwrap().write_table(region_offset, data);
                self.on_msix_changed();
                return;
            }
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

    /// The parsed MSI-X capability, if the device has one (used by the builder
    /// to size the vector table and locate the table page to trap).
    pub fn msix(&self) -> Option<&MsixState> {
        self.msix.as_ref()
    }

    /// Wire up the runtime MSI-X handles: the per-vector eventfds (already
    /// bound to GSIs by the builder), the GSI base, the requester id, and the
    /// worker channel used to update KVM routing from the vCPU thread.
    pub fn set_msix_runtime(
        &mut self,
        gsi_base: u32,
        devid: u32,
        eventfds: Vec<EventFd>,
        irq_sender: Sender<WorkerMessage>,
    ) {
        self.msix_gsi_base = gsi_base;
        self.devid = devid;
        self.msix_eventfds = eventfds;
        self.irq_sender = Some(irq_sender);
    }

    /// The base GSI to assign this device's MSI-X vectors (single passthrough
    /// device for now; a real allocator lands with multi-device support).
    pub fn default_msi_gsi_base() -> u32 {
        MSI_GSI_BASE
    }

    /// Raw fds of the MSI-X eventfds, for `VFIO_DEVICE_SET_IRQS`.
    fn msix_eventfd_fds(&self) -> Vec<RawFd> {
        self.msix_eventfds.iter().map(|e| e.as_raw_fd()).collect()
    }

    /// Build the full guest GSI routing table: an identity IRQCHIP route for
    /// every SPI (replicating the kernel default so virtio keeps working) plus
    /// an MSI route per MSI-X vector. Vectors that are disabled or masked route
    /// with a null message (never translated to an LPI).
    pub fn build_routing(&self) -> Vec<kvm_irq_routing_entry> {
        let mut routes = Vec::with_capacity(AARCH64_NR_SPIS as usize + self.msix_eventfds.len());
        for i in 0..AARCH64_NR_SPIS {
            routes.push(kvm_irq_routing_entry {
                gsi: i,
                type_: KVM_IRQ_ROUTING_IRQCHIP,
                flags: 0,
                u: kvm_irq_routing_entry__bindgen_ty_1 {
                    irqchip: kvm_irq_routing_irqchip { irqchip: 0, pin: i },
                },
                ..Default::default()
            });
        }

        if let Some(msix) = &self.msix {
            let deliver = msix.enabled() && !msix.function_masked();
            for (v, entry) in msix.entries.iter().enumerate() {
                let gsi = self.msix_gsi_base + v as u32;
                let (address_lo, address_hi, data) = if deliver && !entry.masked() {
                    (entry.msg_addr_lo, entry.msg_addr_hi, entry.msg_data)
                } else {
                    (0, 0, 0)
                };
                routes.push(kvm_irq_routing_entry {
                    gsi,
                    type_: KVM_IRQ_ROUTING_MSI,
                    flags: KVM_MSI_VALID_DEVID,
                    u: kvm_irq_routing_entry__bindgen_ty_1 {
                        msi: kvm_irq_routing_msi {
                            address_lo,
                            address_hi,
                            data,
                            __bindgen_anon_1: kvm_irq_routing_msi__bindgen_ty_1 {
                                devid: self.devid,
                            },
                        },
                    },
                    ..Default::default()
                });
            }
        }
        routes
    }

    /// Install the initial GSI routing table and bind each MSI-X eventfd to its
    /// GSI. Ordering matters on aarch64: routing must exist before the irqfd is
    /// registered. Called once by the builder at attach time (main thread).
    pub fn setup_msix_kvm(&self, vm: &VmFd) -> Result<(), VfioError> {
        if self.msix.is_none() {
            return Ok(());
        }
        let entries = self.build_routing();
        let mut routing = kvm_bindings::KvmIrqRouting::new(entries.len()).unwrap();
        routing.as_mut_slice().copy_from_slice(&entries);
        vm.set_gsi_routing(&routing).map_err(|e| {
            VfioError::Ioctl(
                "KVM_SET_GSI_ROUTING",
                std::io::Error::from_raw_os_error(e.errno()),
            )
        })?;

        for (i, evt) in self.msix_eventfds.iter().enumerate() {
            vm.register_irqfd(evt, self.msix_gsi_base + i as u32)
                .map_err(|e| {
                    VfioError::Ioctl("KVM_IRQFD", std::io::Error::from_raw_os_error(e.errno()))
                })?;
        }
        Ok(())
    }

    /// React to a change in MSI-X state (enable/mask bit or a table entry):
    /// push the updated routing to KVM and arm/disarm the physical vectors.
    /// Runs on the vCPU thread, so routing goes through the worker channel.
    fn on_msix_changed(&mut self) {
        let Some(sender) = self.irq_sender.clone() else {
            return;
        };
        let routing = self.build_routing();
        let (tx, rx) = unbounded();
        if sender.send(WorkerMessage::GsiRoute(tx, routing)).is_ok() {
            if let Ok(false) = rx.recv() {
                error!("vfio-pci {}: KVM_SET_GSI_ROUTING (MSI-X update) failed", self.id);
            }
        }

        let enabled = self.msix.as_ref().is_some_and(|m| m.enabled());
        if enabled && !self.msix_armed {
            let fds = self.msix_eventfd_fds();
            match self.vfio.set_irqs_eventfds(VFIO_PCI_MSIX_IRQ_INDEX, &fds) {
                Ok(()) => {
                    self.msix_armed = true;
                    info!("vfio-pci {}: MSI-X armed ({} vectors)", self.id, fds.len());
                }
                Err(e) => error!("vfio-pci {}: VFIO SET_IRQS (arm MSI-X) failed: {e}", self.id),
            }
        } else if !enabled && self.msix_armed {
            let _ = self.vfio.disable_irqs(VFIO_PCI_MSIX_IRQ_INDEX);
            self.msix_armed = false;
        }
    }

    /// Handle a guest write to the MSI-X capability's message-control dword:
    /// update the shadow control register (enable / function-mask) and react.
    /// The write is NOT forwarded to the device — VFIO owns hardware MSI-X
    /// enablement via SET_IRQS.
    fn write_msix_ctl(&mut self, offset: u64, data: &[u8]) {
        let Some(msix) = &self.msix else { return };
        // The dword is [cap_id, next_ptr, msg_ctl_lo, msg_ctl_hi]; only the
        // upper 16 bits (message control) are writable and shadowed.
        let mut ctl = msix.msg_ctl.to_le_bytes();
        for (i, b) in data.iter().enumerate() {
            match offset as usize + i {
                2 => ctl[0] = *b,
                3 => ctl[1] = *b,
                _ => {}
            }
        }
        let new_ctl = u16::from_le_bytes(ctl);
        self.msix.as_mut().unwrap().msg_ctl = new_ctl;
        self.on_msix_changed();
    }
}
