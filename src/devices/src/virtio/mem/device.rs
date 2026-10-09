use std::cmp;
use std::io::{self, Write};

use utils::eventfd::EventFd;
#[cfg(target_os = "linux")]
use vm_memory::{Address, GuestMemoryRegion};
use vm_memory::{ByteValued, Bytes, GuestAddress, GuestMemoryBackend, GuestMemoryMmap};

use super::super::{
    ActivateError, ActivateResult, DeviceQueue, DeviceState, HostMemoryRange, MemError,
    QueueConfig, VirtioDevice, VirtioStateError,
};
use super::{defs, defs::uapi};
use crate::virtio::InterruptTransport;

// Request queue.
pub(crate) const REQ_INDEX: usize = 0;

/// Hot(un)plug granularity. 2 MiB satisfies the Linux driver's requirement of
/// max(page size, pageblock size) on 4K x86_64 and aarch64 kernels.
pub const VIRTIO_MEM_BLOCK_SIZE: u64 = 2 << 20;

// Guest request types.
const VIRTIO_MEM_REQ_PLUG: u16 = 0;
const VIRTIO_MEM_REQ_UNPLUG: u16 = 1;
const VIRTIO_MEM_REQ_UNPLUG_ALL: u16 = 2;
const VIRTIO_MEM_REQ_STATE: u16 = 3;

// Response types.
const VIRTIO_MEM_RESP_ACK: u16 = 0;
const VIRTIO_MEM_RESP_NACK: u16 = 1;
const VIRTIO_MEM_RESP_ERROR: u16 = 3;

// STATE response payloads.
const VIRTIO_MEM_STATE_PLUGGED: u16 = 0;
const VIRTIO_MEM_STATE_UNPLUGGED: u16 = 1;
const VIRTIO_MEM_STATE_MIXED: u16 = 2;

pub(crate) const BASE_AVAIL_FEATURES: u64 = 1 << uapi::VIRTIO_F_VERSION_1 as u64;

const MEM_STATE_MAGIC: &[u8; 8] = b"MSBKMEM\0";
// Schema 1 did not guarantee that acknowledged unplugged memory reads as zero.
// Reject those development captures instead of inferring a zero promise from their bitmap.
const MEM_STATE_SCHEMA: u16 = 2;
const MEM_STATE_HEADER_LEN: usize = 60;
const MAX_MEM_STATE_BYTES: usize = 64 * 1024;

#[derive(Copy, Clone, Debug, Default)]
#[repr(C, packed)]
struct VirtioMemConfig {
    block_size: u64,
    node_id: u16,
    padding: [u8; 6],
    addr: u64,
    region_size: u64,
    usable_region_size: u64,
    plugged_size: u64,
    requested_size: u64,
}

// Safe because it only has data and has no implicit padding.
unsafe impl ByteValued for VirtioMemConfig {}

#[derive(Copy, Clone, Debug, Default)]
#[repr(C, packed)]
struct VirtioMemReq {
    req_type: u16,
    padding: [u16; 3],
    addr: u64,
    nb_blocks: u16,
    padding2: [u16; 3],
}

unsafe impl ByteValued for VirtioMemReq {}

#[derive(Copy, Clone, Debug, Default)]
#[repr(C, packed)]
struct VirtioMemResp {
    resp_type: u16,
    padding: [u16; 3],
    state: u16,
}

unsafe impl ByteValued for VirtioMemResp {}

/// Point-in-time view of the device for host-side control and reporting.
#[derive(Debug, Clone, Copy)]
pub struct MemStateSnapshot {
    /// Bytes the host asked the guest to converge on.
    pub requested_size: u64,

    /// Bytes the guest currently has plugged.
    pub plugged_size: u64,

    /// Total hotpluggable capacity in bytes.
    pub region_size: u64,
}

pub struct Mem {
    pub(crate) queues: Option<Vec<DeviceQueue>>,
    pub(crate) avail_features: u64,
    pub(crate) acked_features: u64,
    pub(crate) activate_evt: EventFd,
    pub(crate) device_state: DeviceState,
    config: VirtioMemConfig,
    /// One bit per block; set = plugged.
    plugged_blocks: Vec<bool>,
}

impl Mem {
    /// Create a virtio-mem device. The hotplug region location is supplied
    /// later through [`set_region`](Self::set_region) once the memory layout
    /// is known; `requested_size` starts at zero until the host raises it.
    pub fn new() -> super::Result<Mem> {
        Ok(Mem {
            queues: None,
            avail_features: BASE_AVAIL_FEATURES,
            acked_features: 0,
            activate_evt: EventFd::new(utils::eventfd::EFD_NONBLOCK).map_err(MemError::EventFd)?,
            device_state: DeviceState::Inactive,
            config: VirtioMemConfig {
                block_size: VIRTIO_MEM_BLOCK_SIZE,
                ..Default::default()
            },
            plugged_blocks: Vec::new(),
        })
    }

    pub fn id(&self) -> &str {
        defs::MEM_DEV_ID
    }

    /// Set the guest-physical placement of the hotplug region. Must be called
    /// before the VM boots; both values must be block-aligned.
    pub fn set_region(&mut self, addr: u64, size: u64) -> super::Result<()> {
        if !addr.is_multiple_of(VIRTIO_MEM_BLOCK_SIZE)
            || !size.is_multiple_of(VIRTIO_MEM_BLOCK_SIZE)
        {
            return Err(MemError::UnalignedRegion);
        }
        self.config.addr = addr;
        self.config.region_size = size;
        self.config.usable_region_size = size;
        self.plugged_blocks = vec![false; (size / VIRTIO_MEM_BLOCK_SIZE) as usize];
        Ok(())
    }

    /// Host control: ask the guest to converge on `size` plugged bytes. Rounds
    /// down to block granularity and caps at the region size. Signals a config
    /// change when the device is active.
    pub fn set_requested_size(&mut self, size: u64) -> u64 {
        let size = cmp::min(
            size - (size % VIRTIO_MEM_BLOCK_SIZE),
            self.config.region_size,
        );
        self.config.requested_size = size;
        if let DeviceState::Activated(_, ref interrupt) = self.device_state {
            interrupt.signal_config_change();
        }
        size
    }

    /// Host control: current requested/plugged/capacity view.
    pub fn state_snapshot(&self) -> MemStateSnapshot {
        MemStateSnapshot {
            requested_size: self.config.requested_size,
            plugged_size: self.config.plugged_size,
            region_size: self.config.region_size,
        }
    }

    /// Returns coalesced zero ranges from actual block ownership, not the requested target.
    ///
    /// Call at a paused, drained host-access boundary so this inventory and the device checkpoint
    /// describe the same request epoch. Every unplugged block starts zero and becomes unplugged
    /// again only after its zeroing succeeds; schema-2 restore preserves that contract.
    pub fn unplugged_ranges(&self) -> Vec<HostMemoryRange> {
        self.block_runs(false)
    }

    fn block_runs(&self, plugged: bool) -> Vec<HostMemoryRange> {
        let mut ranges: Vec<HostMemoryRange> = Vec::new();
        for (index, state) in self.plugged_blocks.iter().enumerate() {
            if *state != plugged {
                continue;
            }
            let start = self.config.addr + index as u64 * VIRTIO_MEM_BLOCK_SIZE;
            if let Some(last) = ranges.last_mut() {
                if last.start + last.length == start {
                    last.length += VIRTIO_MEM_BLOCK_SIZE;
                    continue;
                }
            }
            ranges.push(HostMemoryRange {
                start,
                length: VIRTIO_MEM_BLOCK_SIZE,
            });
        }
        ranges
    }

    fn decode_checkpoint_state(
        &self,
        state: &[u8],
    ) -> Result<(VirtioMemConfig, Vec<bool>), VirtioStateError> {
        let incompatible =
            || VirtioStateError::Incompatible("invalid virtio-mem checkpoint state".into());
        if state.len() < MEM_STATE_HEADER_LEN
            || state.len() > MAX_MEM_STATE_BYTES
            || &state[..8] != MEM_STATE_MAGIC
            || u16::from_le_bytes(state[8..10].try_into().unwrap()) != MEM_STATE_SCHEMA
        {
            return Err(incompatible());
        }
        let word = |offset| u64::from_le_bytes(state[offset..offset + 8].try_into().unwrap());
        let config = VirtioMemConfig {
            node_id: u16::from_le_bytes(state[10..12].try_into().unwrap()),
            block_size: word(12),
            addr: word(20),
            region_size: word(28),
            usable_region_size: word(36),
            plugged_size: word(44),
            requested_size: word(52),
            padding: [0; 6],
        };
        // Construction geometry cannot be inferred from the current requested/plugged total.
        // Validate it before mutating any destination state or allocating the decoded bitmap.
        if config.block_size != self.config.block_size
            || config.node_id != self.config.node_id
            || config.addr != self.config.addr
            || config.region_size != self.config.region_size
            || config.usable_region_size > config.region_size
            || config.requested_size > config.usable_region_size
            || config.plugged_size > config.usable_region_size
            || !config
                .usable_region_size
                .is_multiple_of(VIRTIO_MEM_BLOCK_SIZE)
            || !config.requested_size.is_multiple_of(VIRTIO_MEM_BLOCK_SIZE)
            || !config.plugged_size.is_multiple_of(VIRTIO_MEM_BLOCK_SIZE)
            || config.addr.checked_add(config.region_size).is_none()
        {
            return Err(incompatible());
        }
        let count = self.plugged_blocks.len();
        let packed = &state[MEM_STATE_HEADER_LEN..];
        if packed.len() != count.div_ceil(8)
            || (!count.is_multiple_of(8) && packed.last().unwrap() >> (count % 8) != 0)
        {
            return Err(incompatible());
        }
        let blocks = (0..count)
            .map(|index| packed[index / 8] & (1 << (index % 8)) != 0)
            .collect::<Vec<_>>();
        let plugged = blocks.iter().filter(|bit| **bit).count() as u64 * VIRTIO_MEM_BLOCK_SIZE;
        let usable_blocks = (config.usable_region_size / VIRTIO_MEM_BLOCK_SIZE) as usize;
        if plugged != config.plugged_size || blocks[usable_blocks..].iter().any(|bit| *bit) {
            return Err(incompatible());
        }
        // requested_size may be below plugged_size while a shrink is still converging.
        Ok((config, blocks))
    }

    /// Map a guest request range onto block indices, if fully inside the region.
    fn block_range(&self, addr: u64, nb_blocks: u16) -> Option<std::ops::Range<usize>> {
        if nb_blocks == 0 {
            return None;
        }
        let offset = addr.checked_sub(self.config.addr)?;
        if !offset.is_multiple_of(VIRTIO_MEM_BLOCK_SIZE) {
            return None;
        }
        let first = (offset / VIRTIO_MEM_BLOCK_SIZE) as usize;
        let end = first.checked_add(nb_blocks as usize)?;
        if end > self.plugged_blocks.len() {
            return None;
        }
        Some(first..end)
    }

    fn handle_plug(&mut self, addr: u64, nb_blocks: u16) -> VirtioMemResp {
        let Some(range) = self.block_range(addr, nb_blocks) else {
            return resp(VIRTIO_MEM_RESP_ERROR, 0);
        };
        let add = nb_blocks as u64 * VIRTIO_MEM_BLOCK_SIZE;
        if self.config.plugged_size + add > self.config.requested_size
            || self.plugged_blocks[range.clone()].iter().any(|b| *b)
        {
            return resp(VIRTIO_MEM_RESP_NACK, 0);
        }
        // Ownership transitions are changes even if the guest has not written the newly plugged
        // pages. Mark before publishing the bitmap so incremental capture cannot inherit stale RAM.
        if let Err(error) = self.mark_transition(addr, add) {
            error!("virtio-mem: cannot track plug: {error}");
            return resp(VIRTIO_MEM_RESP_ERROR, 0);
        }
        for block in &mut self.plugged_blocks[range] {
            *block = true;
        }
        self.config.plugged_size += add;
        resp(VIRTIO_MEM_RESP_ACK, 0)
    }

    fn handle_unplug(&mut self, addr: u64, nb_blocks: u16) -> VirtioMemResp {
        let Some(range) = self.block_range(addr, nb_blocks) else {
            return resp(VIRTIO_MEM_RESP_ERROR, 0);
        };
        if self.plugged_blocks[range.clone()].iter().any(|b| !*b) {
            return resp(VIRTIO_MEM_RESP_NACK, 0);
        }
        let length = nb_blocks as u64 * VIRTIO_MEM_BLOCK_SIZE;
        if let Err(error) = self.discard_range(addr, length) {
            error!("virtio-mem: cannot zero unplugged range: {error}");
            return resp(VIRTIO_MEM_RESP_ERROR, 0);
        }
        // ACK and ownership change form the commit point. A failed discard must leave the
        // bitmap/count plugged, and any partial write remains covered by mark-before-write.
        for block in &mut self.plugged_blocks[range.clone()] {
            *block = false;
        }
        self.config.plugged_size -= length;
        resp(VIRTIO_MEM_RESP_ACK, 0)
    }

    fn handle_unplug_all(&mut self) -> VirtioMemResp {
        // Never touch untouched capacity merely because UNPLUG_ALL was requested. Disjoint
        // occupied runs are all zeroed successfully before publishing any ownership change.
        for range in self.block_runs(true) {
            if let Err(error) = self.discard_range(range.start, range.length) {
                error!("virtio-mem: cannot zero all unplugged ranges: {error}");
                return resp(VIRTIO_MEM_RESP_ERROR, 0);
            }
        }
        self.plugged_blocks.fill(false);
        self.config.plugged_size = 0;
        resp(VIRTIO_MEM_RESP_ACK, 0)
    }

    fn handle_state(&self, addr: u64, nb_blocks: u16) -> VirtioMemResp {
        let Some(range) = self.block_range(addr, nb_blocks) else {
            return resp(VIRTIO_MEM_RESP_ERROR, 0);
        };
        let plugged = self.plugged_blocks[range.clone()]
            .iter()
            .filter(|b| **b)
            .count();
        let state = if plugged == range.len() {
            VIRTIO_MEM_STATE_PLUGGED
        } else if plugged == 0 {
            VIRTIO_MEM_STATE_UNPLUGGED
        } else {
            VIRTIO_MEM_STATE_MIXED
        };
        resp(VIRTIO_MEM_RESP_ACK, state)
    }

    fn mark_transition(&self, guest_addr: u64, len: u64) -> io::Result<()> {
        let queue = self
            .queues
            .as_ref()
            .and_then(|queues| queues.get(REQ_INDEX))
            .ok_or_else(|| io::Error::other("virtio-mem request queue is unavailable"))?;
        if !queue.queue.mark_host_write(HostMemoryRange {
            start: guest_addr,
            length: len,
        }) {
            return Err(io::Error::other(
                "virtio-mem transition is not covered by an admitted request",
            ));
        }
        Ok(())
    }

    /// Guarantee zero-on-reuse while keeping registered mappings and private backing intact.
    fn discard_range(&self, guest_addr: u64, len: u64) -> io::Result<()> {
        let DeviceState::Activated(ref memory, _) = self.device_state else {
            return Err(io::Error::other("virtio-mem device is inactive"));
        };
        self.mark_transition(guest_addr, len)?;
        zero_guest_range(memory, guest_addr, len)
    }

    pub fn process_req_queue(&mut self) -> bool {
        let mem = match self.device_state {
            DeviceState::Activated(ref mem, _) => mem.clone(),
            DeviceState::Inactive => unreachable!(),
        };

        let mut have_used = false;
        loop {
            let head = {
                let queues = self
                    .queues
                    .as_mut()
                    .expect("queues should exist when activated");
                match queues[REQ_INDEX].queue.pop(&mem) {
                    Some(head) => head,
                    None => break,
                }
            };
            let index = head.index;

            let mut req = VirtioMemReq::default();
            let mut resp_addr = None;
            for desc in head.into_iter() {
                if desc.is_write_only() {
                    resp_addr = Some(desc.addr);
                } else if mem.read_obj::<VirtioMemReq>(desc.addr).is_ok() {
                    req = mem.read_obj(desc.addr).unwrap();
                }
            }

            let response = match req.req_type {
                VIRTIO_MEM_REQ_PLUG => self.handle_plug(req.addr, req.nb_blocks),
                VIRTIO_MEM_REQ_UNPLUG => self.handle_unplug(req.addr, req.nb_blocks),
                VIRTIO_MEM_REQ_UNPLUG_ALL => self.handle_unplug_all(),
                VIRTIO_MEM_REQ_STATE => self.handle_state(req.addr, req.nb_blocks),
                other => {
                    error!("virtio-mem: unknown request type {other}");
                    resp(VIRTIO_MEM_RESP_ERROR, 0)
                }
            };

            let mut written = 0;
            if let Some(addr) = resp_addr {
                if let Err(e) = mem.write_obj(response, addr) {
                    error!("virtio-mem: failed to write response: {e}");
                } else {
                    written = std::mem::size_of::<VirtioMemResp>() as u32;
                }
            }

            have_used = true;
            let queues = self
                .queues
                .as_mut()
                .expect("queues should exist when activated");
            if let Err(e) = queues[REQ_INDEX].queue.add_used(&mem, index, written) {
                error!("virtio-mem: failed to add used element: {e:?}");
            }
        }

        have_used
    }
}

fn resp(resp_type: u16, state: u16) -> VirtioMemResp {
    VirtioMemResp {
        resp_type,
        padding: [0; 3],
        state,
    }
}

fn zero_guest_range(memory: &GuestMemoryMmap, start: u64, length: u64) -> io::Result<()> {
    let len = usize::try_from(length)
        .map_err(|_| io::Error::other("virtio-mem zero length exceeds host address space"))?;
    if length == 0
        || start.checked_add(length).is_none()
        || !memory.check_range(GuestAddress(start), len)
    {
        return Err(io::Error::other("virtio-mem zero range is not mapped RAM"));
    }

    // Linux promises demand-zero refault only for private anonymous DONTNEED mappings. Private
    // file mappings refault their old file bytes; Darwin/Windows discard APIs do not promise zero.
    // Fall back to bounded writes there, or if Linux refuses the optimization, without remapping
    // memory beneath registered hypervisor slots and existing worker pointers.
    #[cfg(target_os = "linux")]
    if let Some(region) = memory.find_region(GuestAddress(start)) {
        if region.file_offset().is_none()
            && region.flags() & libc::MAP_PRIVATE != 0
            && region.flags() & libc::MAP_ANONYMOUS != 0
            && region.last_addr().raw_value() >= start + length - 1
        {
            let host = memory
                .get_host_address(GuestAddress(start))
                .map_err(io::Error::other)?;
            // Never one MADV_DONTNEED over a whole page table's range (see `virtio::discard`);
            // the pages it leaves resident are zeroed by hand.
            if let Ok(kept) = crate::virtio::discard::dontneed(host, len) {
                let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as usize;
                for p in kept {
                    unsafe { std::ptr::write_bytes(p, 0, page) };
                }
                return Ok(());
            }
        }
    }
    let zeros = [0u8; 64 * 1024];
    let mut done = 0;
    while done < length {
        let count = (length - done).min(zeros.len() as u64) as usize;
        memory
            .write_slice(&zeros[..count], GuestAddress(start + done))
            .map_err(io::Error::other)?;
        done += count as u64;
    }
    Ok(())
}

impl VirtioDevice for Mem {
    fn avail_features(&self) -> u64 {
        self.avail_features
    }

    fn acked_features(&self) -> u64 {
        self.acked_features
    }

    fn set_acked_features(&mut self, acked_features: u64) {
        self.acked_features = acked_features
    }

    fn device_type(&self) -> u32 {
        uapi::VIRTIO_ID_MEM
    }

    fn device_name(&self) -> &str {
        "mem"
    }

    fn queue_config(&self) -> &[QueueConfig] {
        &defs::QUEUE_CONFIG
    }

    fn read_config(&self, offset: u64, mut data: &mut [u8]) {
        let config_slice = self.config.as_slice();
        let config_len = config_slice.len() as u64;
        if offset >= config_len {
            error!("virtio-mem: failed to read config space");
            return;
        }
        if let Some(end) = offset.checked_add(data.len() as u64) {
            // This write can't fail, offset and end are checked against config_len.
            data.write_all(&config_slice[offset as usize..cmp::min(end, config_len) as usize])
                .unwrap();
        }
    }

    fn write_config(&mut self, offset: u64, data: &[u8]) {
        warn!(
            "virtio-mem: guest driver attempted to write device config (offset={:x}, len={:x})",
            offset,
            data.len()
        );
    }

    fn activate(
        &mut self,
        mem: GuestMemoryMmap,
        interrupt: InterruptTransport,
        queues: Vec<DeviceQueue>,
    ) -> ActivateResult {
        if queues.len() != defs::NUM_QUEUES {
            error!(
                "Cannot perform activate. Expected {} queue(s), got {}",
                defs::NUM_QUEUES,
                queues.len()
            );
            return Err(ActivateError::BadActivate);
        }

        if self.activate_evt.write(1).is_err() {
            error!("Cannot write to activate_evt");
            return Err(ActivateError::BadActivate);
        }

        self.queues = Some(queues);
        self.device_state = DeviceState::Activated(mem, interrupt);

        Ok(())
    }

    fn is_activated(&self) -> bool {
        self.device_state.is_activated()
    }

    fn reset(&mut self) -> bool {
        self.queues = None;
        self.device_state = DeviceState::Inactive;
        true
    }

    fn supports_quiesce(&self) -> bool {
        true
    }

    fn quiesce(&mut self) -> Result<Vec<DeviceQueue>, VirtioStateError> {
        if !self.device_state.is_activated() {
            return Err(VirtioStateError::InvalidLifecycle(
                "virtio-mem must be activated before quiescence",
            ));
        }
        let queues = self
            .queues
            .take()
            .ok_or(VirtioStateError::InvalidLifecycle(
                "virtio-mem is activated without queues",
            ))?;
        self.device_state = DeviceState::Inactive;
        Ok(queues)
    }

    fn capture_device_state(&self) -> Result<Vec<u8>, VirtioStateError> {
        if self.device_state.is_activated() {
            return Err(VirtioStateError::InvalidLifecycle(
                "virtio-mem must be quiesced before capture",
            ));
        }
        let length = MEM_STATE_HEADER_LEN + self.plugged_blocks.len().div_ceil(8);
        if length > MAX_MEM_STATE_BYTES {
            return Err(VirtioStateError::Incompatible(
                "virtio-mem checkpoint exceeds device-state limit".into(),
            ));
        }
        let mut state = Vec::with_capacity(length);
        state.extend_from_slice(MEM_STATE_MAGIC);
        state.extend_from_slice(&MEM_STATE_SCHEMA.to_le_bytes());
        state.extend_from_slice(&self.config.node_id.to_le_bytes());
        for value in [
            self.config.block_size,
            self.config.addr,
            self.config.region_size,
            self.config.usable_region_size,
            self.config.plugged_size,
            self.config.requested_size,
        ] {
            state.extend_from_slice(&value.to_le_bytes());
        }
        state.resize(length, 0);
        for (index, plugged) in self.plugged_blocks.iter().enumerate() {
            if *plugged {
                state[MEM_STATE_HEADER_LEN + index / 8] |= 1 << (index % 8);
            }
        }
        self.decode_checkpoint_state(&state)?;
        Ok(state)
    }

    fn validate_device_state(&self, state: &[u8]) -> Result<(), VirtioStateError> {
        self.decode_checkpoint_state(state).map(|_| ())
    }

    fn restore_device_state(&mut self, state: &[u8]) -> Result<(), VirtioStateError> {
        if self.device_state.is_activated() {
            return Err(VirtioStateError::InvalidLifecycle(
                "virtio-mem must be inactive before restore",
            ));
        }
        let (config, blocks) = self.decode_checkpoint_state(state)?;
        self.config = config;
        self.plugged_blocks = blocks;
        Ok(())
    }
}

#[cfg(test)]
mod checkpoint_tests {
    use std::sync::Arc;
    use std::time::Duration;

    use crate::legacy::DummyIrqChip;
    use crate::virtio::queue::tests::VirtQueue;
    use crate::virtio::{MemoryAccessDomain, Queue};

    use super::*;

    fn device(blocks: u64) -> Mem {
        let mut device = Mem::new().unwrap();
        device
            .set_region(2 << 30, blocks * VIRTIO_MEM_BLOCK_SIZE)
            .unwrap();
        device
    }

    fn memory(device: &Mem) -> GuestMemoryMmap {
        GuestMemoryMmap::from_ranges(&[
            (GuestAddress(0), 0x4000),
            (
                GuestAddress(device.config.addr),
                device.config.region_size as usize,
            ),
        ])
        .unwrap()
    }

    fn activate(device: &mut Mem, memory: &GuestMemoryMmap, queue: Queue) {
        let interrupt =
            InterruptTransport::new(DummyIrqChip::new().into(), "mem-test".into()).unwrap();
        device
            .activate(
                memory.clone(),
                interrupt,
                vec![DeviceQueue::new(queue, Arc::new(EventFd::new(0).unwrap()))],
            )
            .unwrap();
    }

    /// Drive the actual request/response queue, including request-scoped admission and dirty marks.
    fn request(
        device: &mut Mem,
        memory: &GuestMemoryMmap,
        domain: &MemoryAccessDomain,
        kind: u16,
        block: u64,
        count: u16,
    ) -> u16 {
        let ring = VirtQueue::new(GuestAddress(0), memory, 8);
        ring.dtable[0].set(0x1000, 24, 1, 1);
        ring.dtable[1].set(0x2000, 10, 2, 0);
        ring.avail.ring[0].set(0);
        ring.avail.idx.set(1);
        let mut queue = ring.create_queue();
        queue.set_memory_access_domain(domain);
        memory
            .write_obj(
                VirtioMemReq {
                    req_type: kind,
                    addr: device.config.addr + block * VIRTIO_MEM_BLOCK_SIZE,
                    nb_blocks: count,
                    ..Default::default()
                },
                GuestAddress(0x1000),
            )
            .unwrap();
        activate(device, memory, queue);
        assert!(device.process_req_queue());
        device.quiesce().unwrap();
        memory
            .read_obj::<VirtioMemResp>(GuestAddress(0x2000))
            .unwrap()
            .resp_type
    }

    fn dirty_epoch(domain: &MemoryAccessDomain) -> Vec<HostMemoryRange> {
        domain.freeze(Duration::from_secs(1)).unwrap();
        let dirty = domain.take_dirty_ranges();
        domain.begin_tracking().unwrap();
        dirty
    }

    fn assert_marked(ranges: &[HostMemoryRange], start: u64, length: u64) {
        assert!(ranges
            .iter()
            .any(|range| range.start <= start && range.start + range.length >= start + length));
    }

    #[test]
    fn checkpoint_preserves_noncontiguous_blocks_and_pending_targets() {
        let mut source = device(9);
        activate(&mut source, &memory(&device(9)), Queue::new(128));
        let base = source.config.addr;
        source.set_requested_size(6 * VIRTIO_MEM_BLOCK_SIZE);
        assert_eq!(
            { source.handle_plug(base, 2).resp_type },
            VIRTIO_MEM_RESP_ACK
        );
        assert_eq!(
            {
                source
                    .handle_plug(base + 5 * VIRTIO_MEM_BLOCK_SIZE, 1)
                    .resp_type
            },
            VIRTIO_MEM_RESP_ACK
        );
        source.quiesce().unwrap();
        for target in [6, 1] {
            source.set_requested_size(target * VIRTIO_MEM_BLOCK_SIZE);
            let state = source.capture_device_state().unwrap();
            let mut restored = device(9);
            restored.restore_device_state(&state).unwrap();
            assert_eq!(restored.plugged_blocks, source.plugged_blocks);
            assert_eq!(
                restored.state_snapshot().requested_size,
                target * VIRTIO_MEM_BLOCK_SIZE
            );
            assert_eq!(
                restored.state_snapshot().plugged_size,
                3 * VIRTIO_MEM_BLOCK_SIZE
            );
            assert_eq!(restored.capture_device_state().unwrap(), state);
            let restored_memory = memory(&restored);
            activate(&mut restored, &restored_memory, Queue::new(128));
            // Subsequent requests must consult restored occupancy, not a fresh all-unplugged map.
            assert_eq!(
                { restored.handle_plug(base, 1).resp_type },
                VIRTIO_MEM_RESP_NACK
            );
            assert_eq!(
                { restored.handle_unplug(base, 1).resp_type },
                VIRTIO_MEM_RESP_ACK
            );
            restored.set_requested_size(9 * VIRTIO_MEM_BLOCK_SIZE);
            assert_eq!(
                { restored.handle_plug(base, 1).resp_type },
                VIRTIO_MEM_RESP_ACK
            );
        }
    }

    #[test]
    fn checkpoint_rejects_missing_inconsistent_and_mismatched_state_without_mutation() {
        let source = device(9);
        let state = source.capture_device_state().unwrap();
        let mut invalid = vec![Vec::new(), state[..59].to_vec()];
        for offset in [8, 10, 12, 20, 28, 36, 44, 52, MEM_STATE_HEADER_LEN] {
            let mut corrupt = state.clone();
            corrupt[offset] ^= 1;
            invalid.push(corrupt);
        }
        let mut padding = state.clone();
        *padding.last_mut().unwrap() = 0x80;
        invalid.push(padding);
        let mut trailing = state.clone();
        trailing.push(0);
        invalid.push(trailing);
        let mut destination = device(9);
        for corrupt in invalid {
            assert!(destination.restore_device_state(&corrupt).is_err());
            assert_eq!(destination.capture_device_state().unwrap(), state);
        }
        assert!(device(10).validate_device_state(&state).is_err());
    }

    #[test]
    fn legacy_checkpoint_cannot_claim_the_new_zero_guarantee() {
        let source = device(1);
        let mut legacy = source.capture_device_state().unwrap();
        legacy[8..10].copy_from_slice(&1u16.to_le_bytes());
        assert!(source.validate_device_state(&legacy).is_err());
    }

    #[test]
    fn unplug_ack_zeroes_noncontiguous_blocks_and_replug_marks_entire_ranges() {
        let mut device = device(5);
        device.set_requested_size(5 * VIRTIO_MEM_BLOCK_SIZE);
        let memory = memory(&device);
        let base = device.config.addr;
        let domain = MemoryAccessDomain::new();
        domain.freeze(Duration::from_secs(1)).unwrap();
        domain
            .configure_tracking(vec![
                HostMemoryRange {
                    start: 0,
                    length: 0x4000,
                },
                HostMemoryRange {
                    start: base,
                    length: 5 * VIRTIO_MEM_BLOCK_SIZE,
                },
            ])
            .unwrap();
        domain.begin_tracking().unwrap();

        for block in [0, 3] {
            assert_eq!(
                request(&mut device, &memory, &domain, VIRTIO_MEM_REQ_PLUG, block, 1),
                VIRTIO_MEM_RESP_ACK
            );
            memory
                .write_slice(
                    &vec![0xa5; VIRTIO_MEM_BLOCK_SIZE as usize],
                    GuestAddress(base + block * VIRTIO_MEM_BLOCK_SIZE),
                )
                .unwrap();
        }
        let first = dirty_epoch(&domain);
        assert_marked(&first, base, VIRTIO_MEM_BLOCK_SIZE);
        assert_marked(
            &first,
            base + 3 * VIRTIO_MEM_BLOCK_SIZE,
            VIRTIO_MEM_BLOCK_SIZE,
        );

        assert_eq!(
            request(&mut device, &memory, &domain, VIRTIO_MEM_REQ_UNPLUG, 0, 1),
            VIRTIO_MEM_RESP_ACK
        );
        assert_eq!(device.plugged_blocks, [false, false, false, true, false]);
        let mut bytes = vec![0xff; VIRTIO_MEM_BLOCK_SIZE as usize];
        memory.read_slice(&mut bytes, GuestAddress(base)).unwrap();
        assert!(bytes.iter().all(|byte| *byte == 0));
        memory
            .read_slice(&mut bytes, GuestAddress(base + 3 * VIRTIO_MEM_BLOCK_SIZE))
            .unwrap();
        assert!(bytes.iter().all(|byte| *byte == 0xa5));
        assert_marked(&dirty_epoch(&domain), base, VIRTIO_MEM_BLOCK_SIZE);
        assert_eq!(
            device.unplugged_ranges(),
            [
                HostMemoryRange {
                    start: base,
                    length: 3 * VIRTIO_MEM_BLOCK_SIZE
                },
                HostMemoryRange {
                    start: base + 4 * VIRTIO_MEM_BLOCK_SIZE,
                    length: VIRTIO_MEM_BLOCK_SIZE
                },
            ]
        );

        assert_eq!(
            request(&mut device, &memory, &domain, VIRTIO_MEM_REQ_PLUG, 0, 1),
            VIRTIO_MEM_RESP_ACK
        );
        assert_marked(&dirty_epoch(&domain), base, VIRTIO_MEM_BLOCK_SIZE);
        memory.read_slice(&mut bytes, GuestAddress(base)).unwrap();
        assert!(bytes.iter().all(|byte| *byte == 0));
        assert_eq!(
            request(
                &mut device,
                &memory,
                &domain,
                VIRTIO_MEM_REQ_UNPLUG_ALL,
                0,
                0
            ),
            VIRTIO_MEM_RESP_ACK
        );
        assert_eq!(device.state_snapshot().plugged_size, 0);
        memory
            .read_slice(&mut bytes, GuestAddress(base + 3 * VIRTIO_MEM_BLOCK_SIZE))
            .unwrap();
        assert!(bytes.iter().all(|byte| *byte == 0));
    }

    #[test]
    fn failed_unplug_and_unplug_all_do_not_commit_ownership() {
        let mut device = device(3);
        let base = device.config.addr;
        // The last plugged block has no mapping, injecting a real zeroing failure. UNPLUG_ALL
        // has already zeroed the first disjoint run when it encounters that failure.
        let memory = GuestMemoryMmap::from_ranges(&[
            (GuestAddress(0), 0x4000),
            (GuestAddress(base), VIRTIO_MEM_BLOCK_SIZE as usize),
        ])
        .unwrap();
        let domain = MemoryAccessDomain::new();
        device.set_requested_size(3 * VIRTIO_MEM_BLOCK_SIZE);
        for block in [0, 2] {
            assert_eq!(
                request(&mut device, &memory, &domain, VIRTIO_MEM_REQ_PLUG, block, 1),
                VIRTIO_MEM_RESP_ACK
            );
        }
        let state = device.capture_device_state().unwrap();
        assert_eq!(
            request(&mut device, &memory, &domain, VIRTIO_MEM_REQ_UNPLUG, 2, 1),
            VIRTIO_MEM_RESP_ERROR
        );
        assert_eq!(device.capture_device_state().unwrap(), state);
        memory.write_obj(0xfeed_u64, GuestAddress(base)).unwrap();
        assert_eq!(
            request(
                &mut device,
                &memory,
                &domain,
                VIRTIO_MEM_REQ_UNPLUG_ALL,
                0,
                0
            ),
            VIRTIO_MEM_RESP_ERROR
        );
        assert_eq!(device.capture_device_state().unwrap(), state);
        assert_eq!(memory.read_obj::<u64>(GuestAddress(base)).unwrap(), 0);

        // A later valid unplug still succeeds; the failed attempt did not lose occupancy.
        assert_eq!(
            request(&mut device, &memory, &domain, VIRTIO_MEM_REQ_UNPLUG, 0, 1),
            VIRTIO_MEM_RESP_ACK
        );
        assert_eq!(device.plugged_blocks, [false, false, true]);
    }

    #[test]
    fn absent_request_admission_cannot_change_ownership_or_bytes() {
        let mut device = device(1);
        let memory = memory(&device);
        let base = device.config.addr;
        let domain = MemoryAccessDomain::new();
        device.set_requested_size(VIRTIO_MEM_BLOCK_SIZE);
        assert_eq!(
            request(&mut device, &memory, &domain, VIRTIO_MEM_REQ_PLUG, 0, 1),
            VIRTIO_MEM_RESP_ACK
        );
        memory.write_obj(0xfeed_u64, GuestAddress(base)).unwrap();
        let mut queue = Queue::new(128);
        queue.set_memory_access_domain(&domain);
        activate(&mut device, &memory, queue);
        assert_eq!(
            { device.handle_unplug(base, 1).resp_type },
            VIRTIO_MEM_RESP_ERROR
        );
        assert_eq!(device.plugged_blocks, [true]);
        assert_eq!(memory.read_obj::<u64>(GuestAddress(base)).unwrap(), 0xfeed);
    }

    #[test]
    fn unplug_all_does_not_touch_never_plugged_capacity() {
        let mut device = device(3);
        let memory = memory(&device);
        let base = device.config.addr;
        // Poison unused capacity to prove the operation has no payload access to it. This test
        // deliberately violates the guest ownership contract solely to make reads/writes visible.
        memory.write_obj(0xfeed_u64, GuestAddress(base)).unwrap();
        let domain = MemoryAccessDomain::new();
        assert_eq!(
            request(
                &mut device,
                &memory,
                &domain,
                VIRTIO_MEM_REQ_UNPLUG_ALL,
                0,
                0
            ),
            VIRTIO_MEM_RESP_ACK
        );
        assert_eq!(memory.read_obj::<u64>(GuestAddress(base)).unwrap(), 0xfeed);
    }

    #[cfg(unix)]
    #[test]
    fn private_discard_preserves_sibling_and_immutable_backing_bytes() {
        use std::fs::File;
        use std::io::{Read, Seek};
        use vm_memory::mmap::MmapRegionBuilder;
        use vm_memory::{FileOffset, GuestRegionMmap};

        let mut device = device(1);
        let base = device.config.addr;
        let block_size = VIRTIO_MEM_BLOCK_SIZE as usize;
        let (fd, path) =
            nix::unistd::mkstemp(&std::env::temp_dir().join("mem-discard-XXXXXX")).unwrap();
        let mut backing = File::from(fd);
        std::fs::remove_file(path).unwrap();
        backing.write_all(&vec![0xa5; block_size]).unwrap();
        let map = || {
            let hotplug = MmapRegionBuilder::new(block_size)
                .with_file_offset(FileOffset::new(backing.try_clone().unwrap(), 0))
                .with_mmap_prot(libc::PROT_READ | libc::PROT_WRITE)
                .with_mmap_flags(libc::MAP_PRIVATE)
                .build()
                .unwrap();
            GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x4000)])
                .unwrap()
                .insert_region(Arc::new(
                    GuestRegionMmap::new(hotplug, GuestAddress(base)).unwrap(),
                ))
                .unwrap()
        };
        let memory = map();
        let sibling = map();
        let domain = MemoryAccessDomain::new();
        device.set_requested_size(VIRTIO_MEM_BLOCK_SIZE);
        assert_eq!(
            request(&mut device, &memory, &domain, VIRTIO_MEM_REQ_PLUG, 0, 1),
            VIRTIO_MEM_RESP_ACK
        );
        assert_eq!(
            request(&mut device, &memory, &domain, VIRTIO_MEM_REQ_UNPLUG, 0, 1),
            VIRTIO_MEM_RESP_ACK
        );
        let mut bytes = vec![0; block_size];
        memory.read_slice(&mut bytes, GuestAddress(base)).unwrap();
        assert!(bytes.iter().all(|byte| *byte == 0));
        sibling.read_slice(&mut bytes, GuestAddress(base)).unwrap();
        assert!(bytes.iter().all(|byte| *byte == 0xa5));
        backing.rewind().unwrap();
        backing.read_exact(&mut bytes).unwrap();
        assert!(bytes.iter().all(|byte| *byte == 0xa5));
        assert_eq!(
            request(&mut device, &memory, &domain, VIRTIO_MEM_REQ_PLUG, 0, 1),
            VIRTIO_MEM_RESP_ACK
        );
        memory.write_obj(0xdead_u64, GuestAddress(base)).unwrap();
        assert_eq!(
            sibling.read_obj::<u64>(GuestAddress(base)).unwrap(),
            0xa5a5a5a5a5a5a5a5
        );
    }
}
