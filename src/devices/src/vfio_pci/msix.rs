// SPDX-License-Identifier: Apache-2.0
//
// Minimal MSI-X capability + table emulation for VFIO PCI passthrough (step 9).
// Trimmed from Cloud Hypervisor's `pci/msix.rs`: no serde/migration, no
// vm-device `InterruptSourceGroup` abstraction. This holds just the parsed cap
// layout and the shadow table; the KVM irqfd / GSI-routing / VFIO SET_IRQS
// wiring lives in `device.rs` + the builder.

use super::vfio::VfioDevice;

/// PCI capability id for MSI-X.
pub const PCI_CAP_ID_MSIX: u8 = 0x11;
/// Config-space offset of the capabilities-list pointer.
const PCI_CAPABILITY_LIST: u64 = 0x34;
/// Config-space offset of the 16-bit status register (bit 4 = capabilities list).
const PCI_STATUS: u64 = 0x06;
const PCI_STATUS_CAP_LIST: u16 = 0x10;
/// Each MSI-X table entry is 16 bytes.
pub const MSIX_TABLE_ENTRY_SIZE: u64 = 16;

const MSIX_ENABLE_BIT: u16 = 15;
const MSIX_FUNCTION_MASK_BIT: u16 = 14;

/// One 16-byte MSI-X table entry (little-endian on the wire).
#[derive(Clone, Copy, Default)]
pub struct MsixTableEntry {
    pub msg_addr_lo: u32,
    pub msg_addr_hi: u32,
    pub msg_data: u32,
    pub vector_ctl: u32,
}

impl MsixTableEntry {
    /// The per-vector mask bit (vector control bit 0).
    pub fn masked(&self) -> bool {
        self.vector_ctl & 0x1 == 0x1
    }

    fn to_bytes(self) -> [u8; 16] {
        let mut b = [0u8; 16];
        b[0..4].copy_from_slice(&self.msg_addr_lo.to_le_bytes());
        b[4..8].copy_from_slice(&self.msg_addr_hi.to_le_bytes());
        b[8..12].copy_from_slice(&self.msg_data.to_le_bytes());
        b[12..16].copy_from_slice(&self.vector_ctl.to_le_bytes());
        b
    }

    fn from_bytes(b: &[u8; 16]) -> Self {
        MsixTableEntry {
            msg_addr_lo: u32::from_le_bytes(b[0..4].try_into().unwrap()),
            msg_addr_hi: u32::from_le_bytes(b[4..8].try_into().unwrap()),
            msg_data: u32::from_le_bytes(b[8..12].try_into().unwrap()),
            vector_ctl: u32::from_le_bytes(b[12..16].try_into().unwrap()),
        }
    }
}

/// Parsed MSI-X capability layout plus the shadow table the guest programs.
pub struct MsixState {
    /// Byte offset of the MSI-X capability in config space.
    pub cap_offset: u64,
    /// Shadow of the 16-bit message-control register (enable / function-mask).
    pub msg_ctl: u16,
    /// BAR index (BIR) holding the MSI-X table, and its offset within that BAR.
    pub table_bir: u32,
    pub table_offset: u64,
    /// BAR index (BIR) holding the PBA, and its offset within that BAR.
    pub pba_bir: u32,
    pub pba_offset: u64,
    /// Number of MSI-X vectors.
    pub table_size: u16,
    /// Shadow table entries (guest-programmed msg addr/data/ctl per vector).
    pub entries: Vec<MsixTableEntry>,
}

impl MsixState {
    /// Walk the capability list of the physical device and parse its MSI-X
    /// capability, if present.
    pub fn parse(vfio: &VfioDevice) -> Option<MsixState> {
        let mut status = [0u8; 2];
        vfio.read_config(PCI_STATUS, &mut status);
        if u16::from_le_bytes(status) & PCI_STATUS_CAP_LIST == 0 {
            return None;
        }

        let mut ptr = {
            let mut b = [0u8; 1];
            vfio.read_config(PCI_CAPABILITY_LIST, &mut b);
            (b[0] as u64) & 0xfc
        };

        // Bounded walk (a device has at most 48 dword-aligned cap slots).
        for _ in 0..48 {
            if ptr == 0 {
                break;
            }
            let mut hdr = [0u8; 2];
            vfio.read_config(ptr, &mut hdr); // [cap_id, next_ptr]
            let cap_id = hdr[0];
            let next = (hdr[1] as u64) & 0xfc;

            if cap_id == PCI_CAP_ID_MSIX {
                let mut mc = [0u8; 2];
                vfio.read_config(ptr + 2, &mut mc);
                let msg_ctl = u16::from_le_bytes(mc);
                let table_size = (msg_ctl & 0x7ff) + 1;

                let mut t = [0u8; 4];
                vfio.read_config(ptr + 4, &mut t);
                let table = u32::from_le_bytes(t);

                let mut p = [0u8; 4];
                vfio.read_config(ptr + 8, &mut p);
                let pba = u32::from_le_bytes(p);

                // Table entries reset masked (vector control bit 0 set), per spec.
                let entries = vec![
                    MsixTableEntry {
                        vector_ctl: 0x1,
                        ..Default::default()
                    };
                    table_size as usize
                ];

                return Some(MsixState {
                    cap_offset: ptr,
                    msg_ctl,
                    table_bir: table & 0x7,
                    table_offset: (table & 0xffff_fff8) as u64,
                    pba_bir: pba & 0x7,
                    pba_offset: (pba & 0xffff_fff8) as u64,
                    table_size,
                    entries,
                });
            }
            ptr = next;
        }
        None
    }

    /// MSI-X enable bit of the message-control register.
    pub fn enabled(&self) -> bool {
        (self.msg_ctl >> MSIX_ENABLE_BIT) & 1 == 1
    }

    /// Function-mask bit (masks all vectors when set).
    pub fn function_masked(&self) -> bool {
        (self.msg_ctl >> MSIX_FUNCTION_MASK_BIT) & 1 == 1
    }

    /// `[offset, size)` of the MSI-X table within its BAR.
    pub fn table_range(&self) -> (u64, u64) {
        (self.table_offset, self.table_size as u64 * MSIX_TABLE_ENTRY_SIZE)
    }

    /// True if a BAR-relative access at `bar_offset` (in BAR `bir`) falls in the
    /// MSI-X table.
    pub fn table_accessed(&self, bir: u32, bar_offset: u64, len: u64) -> bool {
        if bir != self.table_bir {
            return false;
        }
        let (start, size) = self.table_range();
        bar_offset < start + size && bar_offset + len > start
    }

    /// Serve a read of the shadow table for a BAR-relative access.
    pub fn read_table(&self, bar_offset: u64, data: &mut [u8]) {
        let rel = bar_offset.saturating_sub(self.table_offset);
        for (i, byte) in data.iter_mut().enumerate() {
            let abs = rel + i as u64;
            let idx = (abs / MSIX_TABLE_ENTRY_SIZE) as usize;
            let within = (abs % MSIX_TABLE_ENTRY_SIZE) as usize;
            *byte = if idx < self.entries.len() {
                self.entries[idx].to_bytes()[within]
            } else {
                0xff
            };
        }
    }

    /// Apply a write to the shadow table for a BAR-relative access. Returns the
    /// set of affected vector indices (usually one).
    pub fn write_table(&mut self, bar_offset: u64, data: &[u8]) -> Vec<usize> {
        let rel = bar_offset.saturating_sub(self.table_offset);
        let mut affected = Vec::new();
        for (i, byte) in data.iter().enumerate() {
            let abs = rel + i as u64;
            let idx = (abs / MSIX_TABLE_ENTRY_SIZE) as usize;
            let within = (abs % MSIX_TABLE_ENTRY_SIZE) as usize;
            if idx >= self.entries.len() {
                continue;
            }
            let mut bytes = self.entries[idx].to_bytes();
            bytes[within] = *byte;
            self.entries[idx] = MsixTableEntry::from_bytes(&bytes);
            if !affected.contains(&idx) {
                affected.push(idx);
            }
        }
        affected
    }
}
