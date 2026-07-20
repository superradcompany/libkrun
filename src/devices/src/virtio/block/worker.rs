use crate::virtio::descriptor_utils::{Reader, Writer};

use super::super::DeviceQueue;
use super::device::{CacheType, DiskProperties};
#[cfg(windows)]
use super::windows::{
    PendingWindowsRawFileOperation, WindowsRawFileBuffer, WindowsRawFileCompletion,
};

#[cfg(any(windows, target_os = "linux"))]
use crate::virtio::queue::DescriptorChain;
use crate::virtio::InterruptTransport;
#[cfg(target_os = "linux")]
use imago::io_buffers::IoBuffer;
#[cfg(target_os = "linux")]
use io_uring::{opcode, types, IoUring};
#[cfg(windows)]
use std::collections::HashMap;
#[cfg(target_os = "linux")]
use std::fs::File;
#[cfg(windows)]
use std::io::Read;
use std::io::{self, Write};
#[cfg(unix)]
use std::os::fd::AsRawFd;
#[cfg(windows)]
use std::os::windows::io::{AsRawHandle, RawHandle};
#[cfg(target_os = "linux")]
use std::ptr::{copy_nonoverlapping, write_volatile};
use std::result;
#[cfg(target_os = "linux")]
use std::sync::Arc;
use std::thread;
#[cfg(feature = "block-io-profile")]
use std::time::{Duration, Instant};
use utils::epoll::{ControlOperation, Epoll, EpollEvent, EventSet};
use utils::eventfd::EventFd;
use utils::metrics::BlockMetricsWriter;
#[cfg(feature = "block-io-profile")]
use utils::metrics::BlockRequestKind;
use utils::performance::PerfExperiment;
use virtio_bindings::virtio_blk::*;
#[cfg(target_os = "linux")]
use vm_memory::VolatileSlice;
#[cfg(windows)]
use vm_memory::{Address, GuestMemoryBackend};
use vm_memory::{ByteValued, GuestMemoryMmap};

#[cfg(target_os = "linux")]
use smallvec::SmallVec;

#[cfg(unix)]
type Pollable = std::os::fd::RawFd;
#[cfg(windows)]
type Pollable = RawHandle;

const QUEUE_EVENT_BASE: u64 = 0;
const STOP_EVENT: u64 = 64;
#[cfg(target_os = "linux")]
const LINUX_RAW_EVENT: u64 = 65;
#[cfg(target_os = "linux")]
const MAX_PENDING_LINUX_RAW_REQUESTS: usize = 64;
#[cfg(target_os = "linux")]
const MAX_LINUX_RAW_REQUEST_BYTES: usize = 1024 * 1024;
#[cfg(target_os = "linux")]
const MAX_LINUX_RAW_BOUNCE_FAST_PATH_BYTES: usize = 64 * 1024;
#[cfg(target_os = "linux")]
const BLOCK_CQ_POLICY_ENV: &str = "MSB_BLOCK_CQ_POLICY";
#[cfg(target_os = "linux")]
const BLOCK_CQ_WATERMARK_LOW: usize = 8;
#[cfg(target_os = "linux")]
const BLOCK_CQ_WATERMARK_HIGH: usize = 16;
#[cfg(windows)]
const MAX_PENDING_WINDOWS_RAW_REQUESTS: usize = 64;

#[allow(dead_code)]
#[derive(Debug)]
pub enum RequestError {
    Discarding(io::Error),
    DiscardingToZero(io::Error),
    FlushingToDisk(io::Error),
    InvalidDataLength,
    ReadingFromDescriptor(io::Error),
    WritingToDescriptor(io::Error),
    WritingZeroes(io::Error),
    UnknownRequest,
}

/// The request header represents the mandatory fields of each block device request.
///
/// A request header contains the following fields:
///   * request_type: an u32 value mapping to a read, write or flush operation.
///   * reserved: 32 bits are reserved for future extensions of the Virtio Spec.
///   * sector: an u64 value representing the offset where a read/write is to occur.
///
/// The header simplifies reading the request from memory as all request follow
/// the same memory layout.
#[derive(Copy, Clone, Default)]
#[repr(C)]
pub struct RequestHeader {
    request_type: u32,
    _reserved: u32,
    sector: u64,
}
// Safe because RequestHeader only contains plain data.
unsafe impl ByteValued for RequestHeader {}

#[derive(Copy, Clone, Default)]
#[repr(C)]
pub struct DiscardWriteData {
    sector: u64,
    num_sectors: u32,
    flags: u32,
}
// Safe because DiscardWriteData only contains plain data.
unsafe impl ByteValued for DiscardWriteData {}

pub struct BlockWorker {
    device_queues: Vec<DeviceQueue>,
    interrupt: InterruptTransport,
    mem: GuestMemoryMmap,
    disk: DiskProperties,
    #[cfg(target_os = "linux")]
    linux_raw: Option<LinuxRawBackend>,
    stop_fd: EventFd,
    metrics: BlockMetricsWriter,
    parse_descriptors_once: bool,
    batch_completions: bool,
    #[cfg(target_os = "linux")]
    linux_raw_completion_policy: LinuxRawCompletionPolicy,
}

#[cfg(target_os = "linux")]
struct LinuxRawBackend {
    file: Arc<File>,
    ring: IoUring,
    direct_io: bool,
    req_align: usize,
    mem_align: usize,
}

#[cfg(target_os = "linux")]
#[derive(Clone)]
pub(super) struct LinuxRawFile {
    file: Arc<File>,
    direct_io: bool,
    req_align: usize,
    mem_align: usize,
}

#[cfg(target_os = "linux")]
struct PendingLinuxRawRequest {
    queue_index: usize,
    head_index: u16,
    direction: LinuxRawDirection,
    data: LinuxRawRequestData,
    status_ptr: *mut u8,
    data_len: usize,
    offset: u64,
    completed: usize,
}

#[cfg(target_os = "linux")]
enum LinuxRawRequestData {
    Guest {
        buffers: SmallVec<[LinuxRawFileBuffer; 4]>,
        iovecs: SmallVec<[libc::iovec; 4]>,
    },
    Bounce {
        buffer: IoBuffer,
        guest_buffers: SmallVec<[LinuxRawFileBuffer; 4]>,
    },
}

#[cfg(target_os = "linux")]
#[derive(Clone, Copy)]
struct LinuxRawFileBuffer {
    ptr: *mut u8,
    len: usize,
}

#[cfg(target_os = "linux")]
struct PendingLinuxRawRequests {
    slots: Vec<PendingLinuxRawSlot>,
    free: Vec<usize>,
    len: usize,
}

#[cfg(target_os = "linux")]
struct PendingLinuxRawSlot {
    generation: u32,
    request: Option<PendingLinuxRawRequest>,
}

#[cfg(target_os = "linux")]
#[derive(Default)]
struct LinuxRawBouncePool {
    buffers: Vec<IoBuffer>,
}

#[cfg(target_os = "linux")]
#[derive(Clone, Copy, Eq, PartialEq)]
enum LinuxRawDirection {
    Read,
    Write,
}

/// Development-only policy for isolating persistent io_uring completion cadence.
#[cfg(target_os = "linux")]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum LinuxRawCompletionPolicy {
    /// Preserve the current behavior: wait for every request in the bounded epoch.
    #[default]
    WaitAll,
    /// Reap every CQE already available when the ring fd becomes readable.
    Immediate,
    /// Wait for up to eight CQEs before publishing and refilling.
    WatermarkLow,
    /// Wait for up to sixteen CQEs before publishing and refilling.
    WatermarkHigh,
    /// Reap immediately and honor the guest's EVENT_IDX interrupt decision.
    EventIdx,
    /// Drain one admitted virtqueue batch locally before returning to epoll.
    LocalDrain,
}

#[cfg(target_os = "linux")]
impl LinuxRawFile {
    pub(super) fn new(
        file: Arc<File>,
        direct_io: bool,
        req_align: usize,
        mem_align: usize,
    ) -> Self {
        Self {
            file,
            direct_io,
            req_align,
            mem_align,
        }
    }
}

#[cfg(target_os = "linux")]
impl LinuxRawCompletionPolicy {
    fn from_env() -> Self {
        let raw = std::env::var(BLOCK_CQ_POLICY_ENV).ok();
        match Self::parse(raw.as_deref()) {
            Some(policy) => policy,
            None => {
                log::warn!(
                    "ignoring invalid {BLOCK_CQ_POLICY_ENV}={:?}; using wait-all",
                    raw.as_deref().unwrap_or_default()
                );
                Self::WaitAll
            }
        }
    }

    fn parse(raw: Option<&str>) -> Option<Self> {
        match raw.map(str::trim).filter(|raw| !raw.is_empty()) {
            None | Some("wait-all") => Some(Self::WaitAll),
            Some("immediate") => Some(Self::Immediate),
            Some("watermark-8") => Some(Self::WatermarkLow),
            Some("watermark-16") => Some(Self::WatermarkHigh),
            Some("event-idx") => Some(Self::EventIdx),
            Some("local-drain") => Some(Self::LocalDrain),
            Some(_) => None,
        }
    }

    fn completion_target(self, pending: usize) -> usize {
        match self {
            Self::WaitAll => pending,
            Self::WatermarkLow => pending.min(BLOCK_CQ_WATERMARK_LOW),
            Self::WatermarkHigh => pending.min(BLOCK_CQ_WATERMARK_HIGH),
            Self::Immediate | Self::EventIdx | Self::LocalDrain => 0,
        }
    }

    fn uses_conditional_interrupts(self) -> bool {
        matches!(self, Self::EventIdx | Self::LocalDrain)
    }

    fn uses_local_drain(self) -> bool {
        self == Self::LocalDrain
    }
}

#[cfg(target_os = "linux")]
impl LinuxRawFileBuffer {
    /// # Safety
    ///
    /// `ptr..ptr+len` must remain valid until the matching io_uring completion is reaped.
    unsafe fn new(ptr: *mut u8, len: usize) -> Self {
        Self { ptr, len }
    }

    fn is_direct_io_aligned(self, alignment: usize) -> bool {
        alignment > 0
            && (self.ptr as usize).is_multiple_of(alignment)
            && self.len.is_multiple_of(alignment)
    }
}

#[cfg(target_os = "linux")]
impl PendingLinuxRawRequest {
    fn remaining(&self) -> usize {
        self.data_len - self.completed
    }

    fn rebuild_iovecs(&mut self) -> io::Result<()> {
        let LinuxRawRequestData::Guest { buffers, iovecs } = &mut self.data else {
            return Ok(());
        };

        iovecs.clear();
        let mut skip = self.completed;
        for buffer in buffers {
            if skip >= buffer.len {
                skip -= buffer.len;
                continue;
            }

            let ptr = unsafe { buffer.ptr.add(skip) };
            iovecs.push(libc::iovec {
                iov_base: ptr.cast(),
                iov_len: buffer.len - skip,
            });
            skip = 0;
        }

        if skip != 0 || iovecs.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "guest iovec cursor exceeded the request length",
            ));
        }
        Ok(())
    }

    fn copy_bounce_read_to_guest(&self) -> io::Result<()> {
        let LinuxRawRequestData::Bounce {
            buffer,
            guest_buffers,
        } = &self.data
        else {
            return Ok(());
        };

        let mut copied = 0usize;
        for guest in guest_buffers {
            let end = copied
                .checked_add(guest.len)
                .ok_or_else(|| io::Error::other("bounce read length overflow"))?;
            let buffer_ref = buffer.as_ref();
            let source = buffer_ref
                .into_slice()
                .get(copied..end)
                .ok_or_else(|| io::Error::other("bounce read exceeded its buffer"))?;
            unsafe {
                copy_nonoverlapping(source.as_ptr(), guest.ptr, guest.len);
            }
            copied = end;
        }

        (copied == self.data_len)
            .then_some(())
            .ok_or_else(|| io::Error::other("bounce read did not cover every guest byte"))
    }
}

#[cfg(target_os = "linux")]
impl PendingLinuxRawRequests {
    fn new() -> Self {
        let slots = (0..MAX_PENDING_LINUX_RAW_REQUESTS)
            .map(|_| PendingLinuxRawSlot {
                generation: 0,
                request: None,
            })
            .collect();
        let free = (0..MAX_PENDING_LINUX_RAW_REQUESTS).rev().collect();
        Self {
            slots,
            free,
            len: 0,
        }
    }

    fn is_empty(&self) -> bool {
        self.len == 0
    }

    fn is_full(&self) -> bool {
        self.len == MAX_PENDING_LINUX_RAW_REQUESTS
    }

    fn insert(&mut self, request: PendingLinuxRawRequest) -> io::Result<u64> {
        let index = self
            .free
            .pop()
            .ok_or_else(|| io::Error::other("io_uring request table is full"))?;
        let slot = &mut self.slots[index];
        slot.generation = slot.generation.wrapping_add(1).max(1);
        slot.request = Some(request);
        self.len += 1;
        Ok((u64::from(slot.generation) << 32) | index as u64)
    }

    fn get_mut(&mut self, request_id: u64) -> Option<&mut PendingLinuxRawRequest> {
        let (index, generation) = Self::decode(request_id)?;
        let slot = self.slots.get_mut(index)?;
        (slot.generation == generation)
            .then_some(slot.request.as_mut())
            .flatten()
    }

    fn remove(&mut self, request_id: u64) -> Option<PendingLinuxRawRequest> {
        let (index, generation) = Self::decode(request_id)?;
        let slot = self.slots.get_mut(index)?;
        if slot.generation != generation {
            return None;
        }
        let request = slot.request.take()?;
        self.free.push(index);
        self.len -= 1;
        Some(request)
    }

    fn decode(request_id: u64) -> Option<(usize, u32)> {
        let index = usize::try_from(request_id & u64::from(u32::MAX)).ok()?;
        let generation = u32::try_from(request_id >> 32).ok()?;
        (generation != 0 && index < MAX_PENDING_LINUX_RAW_REQUESTS).then_some((index, generation))
    }
}

#[cfg(target_os = "linux")]
impl LinuxRawBouncePool {
    fn acquire(&mut self, len: usize, alignment: usize) -> io::Result<IoBuffer> {
        if let Some(index) = self.buffers.iter().position(|buffer| buffer.len() == len) {
            return Ok(self.buffers.swap_remove(index));
        }
        IoBuffer::new(len, alignment)
    }

    fn release(&mut self, buffer: IoBuffer) {
        if self.buffers.len() < MAX_PENDING_LINUX_RAW_REQUESTS {
            self.buffers.push(buffer);
        }
    }
}

#[cfg(windows)]
struct PendingWindowsBlockRequest {
    queue_index: usize,
    head_index: u16,
    mem: GuestMemoryMmap,
    direction: PendingWindowsBlockDirection,
    data_len: usize,
    operation: Option<PendingWindowsRawFileOperation>,
}

#[cfg(windows)]
#[derive(Clone, Copy, Eq, PartialEq)]
enum PendingWindowsBlockDirection {
    Read,
    Write,
}

#[cfg(windows)]
enum WindowsRawSubmission {
    Submitted,
    Fallback,
}

#[cfg(feature = "block-io-profile")]
struct RequestProfile {
    metrics: BlockMetricsWriter,
    request_started: Instant,
    parse_started: Instant,
    parse_recorded: bool,
    failed: bool,
}

#[cfg(feature = "block-io-profile")]
impl RequestProfile {
    fn new(metrics: BlockMetricsWriter, worker_backlog: Duration) -> Self {
        metrics.record_worker_backlog_ns(duration_ns(worker_backlog));
        let now = Instant::now();
        Self {
            metrics,
            request_started: now,
            parse_started: now,
            parse_recorded: false,
            failed: false,
        }
    }

    fn add_scratch_vectors(&self, count: u64) {
        self.metrics.add_scratch_vectors(count);
    }

    fn record_parse(&mut self) {
        if !self.parse_recorded {
            self.metrics
                .record_descriptor_parse_ns(duration_ns(self.parse_started.elapsed()));
            self.parse_recorded = true;
        }
    }

    fn record_kind(&self, request_type: u32) {
        self.metrics
            .record_request_kind(block_request_kind(request_type));
    }

    fn record_failure(&mut self) {
        self.failed = true;
    }

    fn record_completion(&self, started: Instant, interrupted: bool) {
        self.metrics
            .record_completion_ns(duration_ns(started.elapsed()));
        self.metrics.record_completion(interrupted);
    }
}

#[cfg(feature = "block-io-profile")]
impl Drop for RequestProfile {
    fn drop(&mut self) {
        self.record_parse();
        if self.failed {
            self.metrics.record_failed_request();
        }
        self.metrics
            .record_request_ns(duration_ns(self.request_started.elapsed()));
    }
}

impl BlockWorker {
    pub fn new(
        device_queues: Vec<DeviceQueue>,
        interrupt: InterruptTransport,
        mem: GuestMemoryMmap,
        disk: DiskProperties,
        #[cfg(target_os = "linux")] linux_raw_file: Option<LinuxRawFile>,
        stop_fd: EventFd,
        metrics: BlockMetricsWriter,
    ) -> Self {
        #[cfg(target_os = "linux")]
        let linux_raw = linux_raw_file.and_then(|raw| {
            match IoUring::new(MAX_PENDING_LINUX_RAW_REQUESTS as u32) {
                Ok(ring) => Some(LinuxRawBackend {
                    file: raw.file,
                    ring,
                    direct_io: raw.direct_io,
                    req_align: raw.req_align,
                    mem_align: raw.mem_align,
                }),
                Err(error) => {
                    log::warn!("io_uring unavailable, using synchronous block I/O: {error}");
                    None
                }
            }
        });
        Self {
            device_queues,
            interrupt,
            mem,
            disk,
            #[cfg(target_os = "linux")]
            linux_raw,
            stop_fd,
            metrics,
            parse_descriptors_once: PerfExperiment::BlockDescriptors.enabled(),
            batch_completions: PerfExperiment::BlockCompletions.enabled(),
            #[cfg(target_os = "linux")]
            linux_raw_completion_policy: LinuxRawCompletionPolicy::from_env(),
        }
    }

    pub fn run(self) -> thread::JoinHandle<()> {
        thread::Builder::new()
            .name("block worker".into())
            .spawn(|| self.work())
            .unwrap()
    }

    #[cfg(target_os = "linux")]
    fn work(mut self) {
        let epoll = Epoll::new().unwrap();
        for (index, queue) in self.device_queues.iter().enumerate() {
            let _ = epoll.ctl(
                ControlOperation::Add,
                eventfd_pollable(&queue.event),
                &EpollEvent::new(EventSet::IN, QUEUE_EVENT_BASE + index as u64),
            );
        }
        let _ = epoll.ctl(
            ControlOperation::Add,
            eventfd_pollable(&self.stop_fd),
            &EpollEvent::new(EventSet::IN, STOP_EVENT),
        );
        if let Some(backend) = self.linux_raw.as_ref() {
            if let Err(error) = epoll.ctl(
                ControlOperation::Add,
                backend.ring.as_raw_fd(),
                &EpollEvent::new(EventSet::IN, LINUX_RAW_EVENT),
            ) {
                log::warn!(
                    "cannot poll io_uring completions, using synchronous block I/O: {error}"
                );
                self.linux_raw = None;
            }
        }

        let mut pending = PendingLinuxRawRequests::new();
        let mut bounce_pool = LinuxRawBouncePool::default();
        let mut completions = Vec::with_capacity(MAX_PENDING_LINUX_RAW_REQUESTS);
        let mut epoll_events = vec![EpollEvent::new(EventSet::empty(), 0); 32];
        loop {
            match epoll.wait(epoll_events.len(), -1, epoll_events.as_mut_slice()) {
                Ok(event_count) => {
                    for event in &epoll_events[..event_count] {
                        let source = event.data();
                        let event_set = event.event_set();
                        match source {
                            source
                                if source < self.device_queues.len() as u64
                                    && event_set.contains(EventSet::IN) =>
                            {
                                let queue_index = source as usize;
                                if let Err(error) = self.device_queues[queue_index].event.read() {
                                    error!("Failed to get queue event: {error:?}");
                                } else if self.linux_raw.is_some() {
                                    if self.linux_raw_completion_policy.uses_local_drain() {
                                        let mut completed_queues = 0u64;
                                        loop {
                                            self.process_linux_virtio_queue(
                                                queue_index,
                                                &mut pending,
                                                &mut bounce_pool,
                                                &mut completions,
                                            );
                                            if pending.is_empty() {
                                                break;
                                            }
                                            self.submit_linux_raw_requests();
                                            completed_queues |= self.drain_linux_raw_requests(
                                                &mut pending,
                                                &mut bounce_pool,
                                                &mut completions,
                                            );
                                        }
                                        if self.batch_completions && completed_queues != 0 {
                                            self.signal_linux_raw_batch(
                                                completed_queues,
                                                &self.mem.clone(),
                                            );
                                        }
                                    } else {
                                        self.process_linux_virtio_queue(
                                            queue_index,
                                            &mut pending,
                                            &mut bounce_pool,
                                            &mut completions,
                                        );
                                        self.submit_linux_raw_requests();
                                    }
                                } else {
                                    self.process_virtio_queue(queue_index);
                                }
                            }
                            LINUX_RAW_EVENT if event_set.contains(EventSet::IN) => {
                                self.reap_linux_raw_requests(
                                    &mut pending,
                                    &mut bounce_pool,
                                    &mut completions,
                                );
                                self.process_all_linux_raw_queues(
                                    &mut pending,
                                    &mut bounce_pool,
                                    &mut completions,
                                );
                                self.submit_linux_raw_requests();
                            }
                            STOP_EVENT if event_set.contains(EventSet::IN) => {
                                debug!("stopping worker thread");
                                let _ = self.stop_fd.read();
                                self.drain_linux_raw_requests(
                                    &mut pending,
                                    &mut bounce_pool,
                                    &mut completions,
                                );
                                return;
                            }
                            _ => log::warn!(
                                "Received unknown event: {event_set:?} from fd: {source:?}"
                            ),
                        }
                    }
                }
                Err(error) => debug!("failed to consume muxer epoll event: {error}"),
            }
        }
    }

    #[cfg(all(not(windows), not(target_os = "linux")))]
    fn work(mut self) {
        let stop_ev = eventfd_pollable(&self.stop_fd);

        let epoll = Epoll::new().unwrap();

        for (index, queue) in self.device_queues.iter().enumerate() {
            let _ = epoll.ctl(
                ControlOperation::Add,
                eventfd_pollable(&queue.event),
                &EpollEvent::new(EventSet::IN, QUEUE_EVENT_BASE + index as u64),
            );
        }

        let _ = epoll.ctl(
            ControlOperation::Add,
            stop_ev,
            &EpollEvent::new(EventSet::IN, STOP_EVENT),
        );

        let mut epoll_events = vec![EpollEvent::new(EventSet::empty(), 0); 32];
        loop {
            match epoll.wait(epoll_events.len(), -1, epoll_events.as_mut_slice()) {
                Ok(ev_cnt) => {
                    for event in &epoll_events[0..ev_cnt] {
                        let source = event.data();
                        let event_set = event.event_set();
                        match source {
                            source
                                if source < self.device_queues.len() as u64
                                    && event_set.contains(EventSet::IN) =>
                            {
                                self.process_queue_event(source as usize);
                            }
                            STOP_EVENT if event_set.contains(EventSet::IN) => {
                                debug!("stopping worker thread");
                                let _ = self.stop_fd.read();
                                return;
                            }
                            _ => {
                                log::warn!(
                                    "Received unknown event: {event_set:?} from fd: {source:?}"
                                );
                            }
                        }
                    }
                }
                Err(e) => {
                    debug!("failed to consume muxer epoll event: {e}");
                }
            }
        }
    }

    #[cfg(windows)]
    fn work(mut self) {
        let stop_ev = eventfd_pollable(&self.stop_fd);

        let epoll = Epoll::new().unwrap();

        for (index, queue) in self.device_queues.iter().enumerate() {
            let _ = epoll.ctl(
                ControlOperation::Add,
                eventfd_pollable(&queue.event),
                &EpollEvent::new(EventSet::IN, QUEUE_EVENT_BASE + index as u64),
            );
        }

        let _ = epoll.ctl(
            ControlOperation::Add,
            stop_ev,
            &EpollEvent::new(EventSet::IN, STOP_EVENT),
        );

        let mut pending_raw = HashMap::new();
        let mut epoll_events = vec![EpollEvent::new(EventSet::empty(), 0); 32];
        loop {
            if !pending_raw.is_empty() {
                self.complete_one_windows_raw_request(&mut pending_raw);
                self.process_all_virtio_queues(&mut pending_raw);
                continue;
            }

            match epoll.wait(epoll_events.len(), -1, epoll_events.as_mut_slice()) {
                Ok(ev_cnt) => {
                    for event in &epoll_events[0..ev_cnt] {
                        let source = event.data();
                        let event_set = event.event_set();
                        match source {
                            source
                                if source < self.device_queues.len() as u64
                                    && event_set.contains(EventSet::IN) =>
                            {
                                self.process_queue_event(source as usize, &mut pending_raw);
                            }
                            STOP_EVENT if event_set.contains(EventSet::IN) => {
                                debug!("stopping worker thread");
                                let _ = self.stop_fd.read();
                                return;
                            }
                            _ => {
                                log::warn!(
                                    "Received unknown event: {event_set:?} from fd: {source:?}"
                                );
                            }
                        }
                    }
                }
                Err(e) => {
                    debug!("failed to consume muxer epoll event: {e}");
                }
            }
        }
    }

    #[cfg(all(not(windows), not(target_os = "linux")))]
    fn process_queue_event(&mut self, queue_index: usize) {
        if let Err(e) = self.device_queues[queue_index].event.read() {
            error!("Failed to get queue event: {e:?}");
        } else {
            self.process_virtio_queue(queue_index);
        }
    }

    #[cfg(windows)]
    fn process_queue_event(
        &mut self,
        queue_index: usize,
        pending_raw: &mut HashMap<usize, PendingWindowsBlockRequest>,
    ) {
        if let Err(e) = self.device_queues[queue_index].event.read() {
            error!("Failed to get queue event: {e:?}");
        } else {
            self.process_virtio_queue(queue_index, pending_raw);
        }
    }

    /// Process device virtio queue(s).
    #[cfg(not(windows))]
    fn process_virtio_queue(&mut self, queue_index: usize) {
        let mem = self.mem.clone();
        loop {
            self.device_queues[queue_index]
                .queue
                .disable_notification(&mem)
                .unwrap();

            self.process_queue(queue_index, &mem);

            if !self.device_queues[queue_index]
                .queue
                .enable_notification(&mem)
                .unwrap()
            {
                break;
            }
        }
    }

    /// Process device virtio queue(s).
    #[cfg(windows)]
    fn process_virtio_queue(
        &mut self,
        queue_index: usize,
        pending_raw: &mut HashMap<usize, PendingWindowsBlockRequest>,
    ) {
        let mem = self.mem.clone();
        loop {
            self.device_queues[queue_index]
                .queue
                .disable_notification(&mem)
                .unwrap();

            self.process_queue(queue_index, &mem, pending_raw);

            let queue_needs_more_processing = self.device_queues[queue_index]
                .queue
                .enable_notification(&mem)
                .unwrap();

            if pending_raw.len() >= MAX_PENDING_WINDOWS_RAW_REQUESTS {
                break;
            }

            if !queue_needs_more_processing {
                break;
            }
        }
    }

    #[cfg(windows)]
    fn process_all_virtio_queues(
        &mut self,
        pending_raw: &mut HashMap<usize, PendingWindowsBlockRequest>,
    ) {
        for queue_index in 0..self.device_queues.len() {
            self.process_virtio_queue(queue_index, pending_raw);
            if pending_raw.len() >= MAX_PENDING_WINDOWS_RAW_REQUESTS {
                break;
            }
        }
    }

    #[cfg(not(windows))]
    fn process_queue(&mut self, queue_index: usize, mem: &GuestMemoryMmap) {
        #[cfg(feature = "block-io-profile")]
        let drain_started = Instant::now();
        let mut completed_any = false;
        while let Some(head) = self.device_queues[queue_index].queue.pop(mem) {
            #[cfg(feature = "block-io-profile")]
            let mut profile = RequestProfile::new(self.metrics.clone(), drain_started.elapsed());
            let views = if self.parse_descriptors_once {
                Reader::new_pair(mem, head.clone())
            } else {
                Reader::new(mem, head.clone()).and_then(|reader| {
                    Writer::new(mem, head.clone()).map(|writer| (reader, writer))
                })
            };
            let (mut reader, mut writer) = match views {
                Ok(views) => {
                    #[cfg(feature = "block-io-profile")]
                    profile.add_scratch_vectors(2);
                    views
                }
                Err(e) => {
                    #[cfg(feature = "block-io-profile")]
                    profile.record_failure();
                    error!("invalid descriptor chain: {e:?}");
                    continue;
                }
            };
            #[cfg(feature = "block-io-profile")]
            profile.add_scratch_vectors(1);
            let request_header: RequestHeader = match reader.read_obj() {
                Ok(h) => h,
                Err(e) => {
                    #[cfg(feature = "block-io-profile")]
                    profile.record_failure();
                    error!("invalid request header: {e:?}");
                    continue;
                }
            };
            #[cfg(feature = "block-io-profile")]
            {
                profile.record_parse();
                profile.record_kind(request_header.request_type);
                profile
                    .add_scratch_vectors(request_data_scratch_vectors(request_header.request_type));
            }

            let (status, len): (u8, usize) =
                match self.process_request(request_header, &mut reader, &mut writer) {
                    Ok(l) => (VIRTIO_BLK_S_OK.try_into().unwrap(), l),
                    Err(e) => {
                        #[cfg(feature = "block-io-profile")]
                        profile.record_failure();
                        error!("error processing request: {e:?}");
                        (VIRTIO_BLK_S_IOERR.try_into().unwrap(), 0)
                    }
                };

            #[cfg(feature = "block-io-profile")]
            let completion_started = Instant::now();
            #[cfg(feature = "block-io-profile")]
            profile.add_scratch_vectors(1);
            if let Err(e) = writer.write_obj(status) {
                #[cfg(feature = "block-io-profile")]
                profile.record_failure();
                error!("Failed to write virtio block status: {e:?}")
            }

            if let Err(e) = self.device_queues[queue_index]
                .queue
                .add_used(mem, head.index, len as u32)
            {
                #[cfg(feature = "block-io-profile")]
                profile.record_failure();
                error!("failed to add used elements to the queue: {e:?}");
            } else {
                completed_any = true;
            }

            #[cfg(feature = "block-io-profile")]
            let mut interrupted = false;
            if !self.batch_completions
                && self.device_queues[queue_index]
                    .queue
                    .needs_notification(mem)
                    .unwrap()
            {
                if let Err(e) = self.interrupt.try_signal_used_queue() {
                    #[cfg(feature = "block-io-profile")]
                    profile.record_failure();
                    error!("error signalling queue: {e:?}");
                } else {
                    #[cfg(feature = "block-io-profile")]
                    {
                        interrupted = true;
                    }
                }
            }
            #[cfg(feature = "block-io-profile")]
            profile.record_completion(completion_started, interrupted);
        }

        if self.batch_completions
            && completed_any
            && self.device_queues[queue_index]
                .queue
                .needs_notification(mem)
                .unwrap()
        {
            if let Err(e) = self.interrupt.try_signal_used_queue() {
                error!("error signalling batched block completions: {e:?}");
            } else {
                #[cfg(feature = "block-io-profile")]
                self.metrics.record_interrupt();
            }
        }
    }

    #[cfg(target_os = "linux")]
    fn process_linux_virtio_queue(
        &mut self,
        queue_index: usize,
        pending: &mut PendingLinuxRawRequests,
        bounce_pool: &mut LinuxRawBouncePool,
        completions: &mut Vec<(u64, i32)>,
    ) {
        let mem = self.mem.clone();
        loop {
            self.device_queues[queue_index]
                .queue
                .disable_notification(&mem)
                .unwrap();
            self.process_linux_raw_queue(queue_index, &mem, pending, bounce_pool, completions);
            let queue_needs_more_processing = self.device_queues[queue_index]
                .queue
                .enable_notification(&mem)
                .unwrap();
            if pending.is_full() || !queue_needs_more_processing {
                break;
            }
        }
    }

    #[cfg(target_os = "linux")]
    fn process_all_linux_raw_queues(
        &mut self,
        pending: &mut PendingLinuxRawRequests,
        bounce_pool: &mut LinuxRawBouncePool,
        completions: &mut Vec<(u64, i32)>,
    ) {
        for queue_index in 0..self.device_queues.len() {
            self.process_linux_virtio_queue(queue_index, pending, bounce_pool, completions);
            if pending.is_full() {
                break;
            }
        }
    }

    #[cfg(target_os = "linux")]
    fn process_linux_raw_queue(
        &mut self,
        queue_index: usize,
        mem: &GuestMemoryMmap,
        pending: &mut PendingLinuxRawRequests,
        bounce_pool: &mut LinuxRawBouncePool,
        completions: &mut Vec<(u64, i32)>,
    ) {
        while !pending.is_full() {
            let Some(head) = self.device_queues[queue_index].queue.pop(mem) else {
                break;
            };
            let views = if self.parse_descriptors_once {
                Reader::new_pair(mem, head.clone())
            } else {
                Reader::new(mem, head.clone()).and_then(|reader| {
                    Writer::new(mem, head.clone()).map(|writer| (reader, writer))
                })
            };
            let (mut reader, mut writer) = match views {
                Ok(views) => views,
                Err(error) => {
                    error!("invalid descriptor chain: {error:?}");
                    continue;
                }
            };
            let request_header: RequestHeader = match reader.read_obj() {
                Ok(header) => header,
                Err(error) => {
                    error!("invalid request header: {error:?}");
                    continue;
                }
            };

            let (direct_io, req_align, mem_align) = {
                let backend = self.linux_raw.as_ref().expect("io_uring backend selected");
                (backend.direct_io, backend.req_align, backend.mem_align)
            };
            let async_request = match request_header.request_type {
                VIRTIO_BLK_T_IN => writer
                    .available_bytes()
                    .checked_sub(1)
                    .filter(|length| {
                        *length > 0
                            && length.is_multiple_of(512)
                            && *length <= MAX_LINUX_RAW_REQUEST_BYTES
                    })
                    .and_then(|length| {
                        let offset = sector_offset(request_header.sector).ok()?;
                        linux_raw_request_is_aligned(direct_io, req_align, offset, length)
                            .then_some((LinuxRawDirection::Read, length, offset))
                    }),
                VIRTIO_BLK_T_OUT => {
                    let length = reader.available_bytes();
                    let offset = sector_offset(request_header.sector).ok();
                    offset
                        .filter(|offset| {
                            length > 0
                                && length.is_multiple_of(512)
                                && length <= MAX_LINUX_RAW_REQUEST_BYTES
                                && linux_raw_request_is_aligned(
                                    direct_io, req_align, *offset, length,
                                )
                        })
                        .map(|offset| (LinuxRawDirection::Write, length, offset))
                }
                _ => None,
            };

            if let Some((direction, length, offset)) = async_request {
                let status_offset = writer.available_bytes().saturating_sub(1);
                let status_writer = match writer.split_at(status_offset) {
                    Ok(status_writer) => status_writer,
                    Err(error) => {
                        error!("failed to isolate io_uring status byte: {error:?}");
                        self.complete_linux_raw_error(queue_index, head.index, mem);
                        continue;
                    }
                };
                let status_buffers =
                    snapshot_linux_raw_buffers(status_writer.remaining_slices(), true, 1);
                let status_ptr = match status_buffers {
                    Ok(buffers) if buffers.len() == 1 && buffers[0].len == 1 => buffers[0].ptr,
                    Ok(_) => {
                        error!("io_uring status snapshot was not exactly one byte");
                        self.complete_linux_raw_error(queue_index, head.index, mem);
                        continue;
                    }
                    Err(error) => {
                        error!("failed to snapshot io_uring status byte: {error:?}");
                        self.complete_linux_raw_error(queue_index, head.index, mem);
                        continue;
                    }
                };
                let guest_buffers = match direction {
                    LinuxRawDirection::Read => {
                        snapshot_linux_raw_buffers(writer.remaining_slices(), true, length)
                    }
                    LinuxRawDirection::Write => {
                        snapshot_linux_raw_buffers(reader.remaining_slices(), false, length)
                    }
                };
                let guest_buffers = match guest_buffers {
                    Ok(buffers) => buffers,
                    Err(error) => {
                        error!("failed to snapshot io_uring data buffers: {error:?}");
                        self.complete_linux_raw_admission_error(
                            queue_index,
                            head.index,
                            status_ptr,
                            mem,
                        );
                        continue;
                    }
                };

                // Small buffered requests use the bounded reusable pool: copying at most 64 KiB
                // is cheaper than importing and pinning arbitrary guest mappings in io_uring for
                // every operation. Large requests retain the zero-bounce guest-iovec path where
                // avoiding a full data copy and request-sized allocator churn matters most.
                let can_use_guest_iovecs = if direct_io {
                    guest_buffers
                        .iter()
                        .all(|buffer| buffer.is_direct_io_aligned(mem_align))
                } else {
                    length > MAX_LINUX_RAW_BOUNCE_FAST_PATH_BYTES
                };
                let data = if can_use_guest_iovecs {
                    LinuxRawRequestData::Guest {
                        iovecs: SmallVec::with_capacity(guest_buffers.len()),
                        buffers: guest_buffers,
                    }
                } else {
                    let mut buffer = match bounce_pool.acquire(length, mem_align) {
                        Ok(buffer) => buffer,
                        Err(error) => {
                            error!("failed to acquire aligned io_uring buffer: {error:?}");
                            self.complete_linux_raw_admission_error(
                                queue_index,
                                head.index,
                                status_ptr,
                                mem,
                            );
                            continue;
                        }
                    };
                    if direction == LinuxRawDirection::Write {
                        if let Err(error) = copy_linux_guest_buffers_to_bounce(
                            &guest_buffers,
                            buffer.as_mut().into_slice(),
                        ) {
                            error!("failed to stage io_uring block write: {error:?}");
                            bounce_pool.release(buffer);
                            self.complete_linux_raw_admission_error(
                                queue_index,
                                head.index,
                                status_ptr,
                                mem,
                            );
                            continue;
                        }
                    }
                    LinuxRawRequestData::Bounce {
                        buffer,
                        guest_buffers,
                    }
                };
                let mut request = PendingLinuxRawRequest {
                    queue_index,
                    head_index: head.index,
                    direction,
                    data,
                    status_ptr,
                    data_len: length,
                    offset,
                    completed: 0,
                };
                if let Err(error) = request.rebuild_iovecs() {
                    error!("failed to build io_uring guest iovecs: {error:?}");
                    self.complete_linux_raw_request(
                        request,
                        false,
                        -libc::EINVAL,
                        mem,
                        bounce_pool,
                    );
                    continue;
                }
                if let Err(error) = self.submit_linux_raw_request(request, pending) {
                    error!("failed to submit io_uring block request: {error:?}");
                    self.complete_linux_raw_admission_error(
                        queue_index,
                        head.index,
                        status_ptr,
                        mem,
                    );
                }
                continue;
            }

            // Flush, discard, write-zeroes, identity, and malformed read/write requests use the
            // mature synchronous path. Draining first is the global write-epoch barrier: every
            // request dequeued before a flush is complete before the durability syscall begins.
            self.drain_linux_raw_requests(pending, bounce_pool, completions);
            let (status, len): (u8, usize) =
                match self.process_request(request_header, &mut reader, &mut writer) {
                    Ok(length) => (VIRTIO_BLK_S_OK as u8, length),
                    Err(error) => {
                        log::error!("error processing synchronous block request: {error:?}");
                        (VIRTIO_BLK_S_IOERR as u8, 0)
                    }
                };
            self.complete_linux_sync_request(
                queue_index,
                head.index,
                mem,
                &mut writer,
                status,
                len,
            );
            // A synchronous barrier has no io_uring CQE to wake the worker again. When batched
            // completions are enabled, publish its one shared interrupt here; otherwise a flush
            // can sit in the used ring forever and deadlock the guest during boot.
            if self.batch_completions {
                self.signal_linux_raw_batch(1u64 << queue_index, mem);
            }
        }
    }

    #[cfg(target_os = "linux")]
    fn submit_linux_raw_request(
        &mut self,
        request: PendingLinuxRawRequest,
        pending: &mut PendingLinuxRawRequests,
    ) -> io::Result<()> {
        let request_id = pending.insert(request)?;
        let request = pending.get_mut(request_id).expect("request was inserted");
        let backend = self.linux_raw.as_mut().expect("io_uring backend selected");
        if let Err(error) = Self::push_linux_raw_entry(backend, request_id, request) {
            pending.remove(request_id);
            return Err(error);
        }
        Ok(())
    }

    #[cfg(target_os = "linux")]
    fn push_linux_raw_entry(
        backend: &mut LinuxRawBackend,
        request_id: u64,
        request: &mut PendingLinuxRawRequest,
    ) -> io::Result<()> {
        let remaining = request.remaining();
        let length = u32::try_from(remaining)
            .map_err(|_| io::Error::other("io_uring block request is too large"))?;
        let offset = request
            .offset
            .checked_add(request.completed as u64)
            .ok_or_else(|| io::Error::other("io_uring block offset overflow"))?;
        let entry = match (&mut request.data, request.direction) {
            (LinuxRawRequestData::Guest { iovecs, .. }, LinuxRawDirection::Read) => {
                opcode::Readv::new(
                    types::Fd(backend.file.as_raw_fd()),
                    iovecs.as_ptr(),
                    u32::try_from(iovecs.len())
                        .map_err(|_| io::Error::other("too many guest iovecs"))?,
                )
                .offset(offset)
                .build()
            }
            (LinuxRawRequestData::Guest { iovecs, .. }, LinuxRawDirection::Write) => {
                opcode::Writev::new(
                    types::Fd(backend.file.as_raw_fd()),
                    iovecs.as_ptr(),
                    u32::try_from(iovecs.len())
                        .map_err(|_| io::Error::other("too many guest iovecs"))?,
                )
                .offset(offset)
                .build()
            }
            (LinuxRawRequestData::Bounce { buffer, .. }, LinuxRawDirection::Read) => {
                opcode::Read::new(
                    types::Fd(backend.file.as_raw_fd()),
                    unsafe { buffer.as_mut().as_ptr().add(request.completed) },
                    length,
                )
                .offset(offset)
                .build()
            }
            (LinuxRawRequestData::Bounce { buffer, .. }, LinuxRawDirection::Write) => {
                opcode::Write::new(
                    types::Fd(backend.file.as_raw_fd()),
                    unsafe { buffer.as_ref().as_ptr().add(request.completed) },
                    length,
                )
                .offset(offset)
                .build()
            }
        }
        .user_data(request_id);

        // SAFETY: the request remains in `pending`, keeping its heap allocation and file alive
        // until the matching CQE is consumed. The queue depth and all buffers are bounded.
        if unsafe { backend.ring.submission().push(&entry) }.is_err() {
            backend.ring.submit()?;
            // SAFETY: same ownership argument as above; submission cannot outlive `pending`.
            if unsafe { backend.ring.submission().push(&entry) }.is_err() {
                return Err(io::Error::other("io_uring submission queue remained full"));
            }
        }
        Ok(())
    }

    #[cfg(target_os = "linux")]
    fn submit_linux_raw_requests(&mut self) {
        let Some(backend) = self.linux_raw.as_mut() else {
            return;
        };
        if let Err(error) = backend.ring.submit() {
            error!("io_uring submission failed: {error:?}");
        }
    }

    #[cfg(target_os = "linux")]
    fn reap_linux_raw_requests(
        &mut self,
        pending: &mut PendingLinuxRawRequests,
        bounce_pool: &mut LinuxRawBouncePool,
        completions: &mut Vec<(u64, i32)>,
    ) -> u64 {
        let mut completed_queues = 0u64;
        completions.clear();
        {
            let backend = self.linux_raw.as_mut().expect("io_uring backend selected");
            let completion_target = if self.batch_completions {
                self.linux_raw_completion_policy
                    .completion_target(pending.len)
            } else {
                0
            };
            if completion_target > 0 {
                // The watermark is always bounded by the 64-request table. `wait-all` preserves
                // the previous experiment; lower watermarks isolate completion cadence without
                // changing request buffers, descriptor ownership, or flush ordering.
                if let Err(error) = backend.ring.submit_and_wait(completion_target) {
                    error!("io_uring completion-batch wait failed: {error:?}");
                }
            }
            completions.extend(
                backend
                    .ring
                    .completion()
                    .map(|entry| (entry.user_data(), entry.result())),
            );
        }

        let mem = self.mem.clone();
        for (request_id, result) in completions.drain(..) {
            let Some(request) = pending.get_mut(request_id) else {
                error!("io_uring returned unknown or stale block request {request_id}");
                continue;
            };
            let completed = usize::try_from(result).ok();
            let remaining = request.remaining();
            if let Some(completed) = completed.filter(|completed| *completed < remaining) {
                if completed > 0 {
                    request.completed += completed;
                    let resubmit_result = request.rebuild_iovecs().and_then(|()| {
                        let can_resubmit = self.linux_raw.as_ref().is_some_and(|backend| {
                            linux_raw_remainder_is_aligned(backend, request)
                        });
                        if can_resubmit {
                            let backend =
                                self.linux_raw.as_mut().expect("io_uring backend selected");
                            Self::push_linux_raw_entry(backend, request_id, request)
                        } else {
                            Err(io::Error::other(
                                "partial direct I/O left an unaligned remainder",
                            ))
                        }
                    });
                    if resubmit_result.is_ok() {
                        continue;
                    }
                    if let Err(error) = resubmit_result {
                        error!("failed to resubmit partial io_uring request: {error:?}");
                    }
                }
            }

            let success = completed == Some(remaining);
            let request = pending.remove(request_id).expect("request still pending");
            if request.queue_index < u64::BITS as usize {
                completed_queues |= 1u64 << request.queue_index;
            }
            self.complete_linux_raw_request(request, success, result, &mem, bounce_pool);
        }

        if self.batch_completions
            && completed_queues != 0
            && !self.linux_raw_completion_policy.uses_local_drain()
        {
            self.signal_linux_raw_batch(completed_queues, &mem);
        }
        completed_queues
    }

    #[cfg(target_os = "linux")]
    fn drain_linux_raw_requests(
        &mut self,
        pending: &mut PendingLinuxRawRequests,
        bounce_pool: &mut LinuxRawBouncePool,
        completions: &mut Vec<(u64, i32)>,
    ) -> u64 {
        let mut completed_queues = 0u64;
        while !pending.is_empty() {
            let backend = self.linux_raw.as_mut().expect("io_uring backend selected");
            if let Err(error) = backend.ring.submit_and_wait(1) {
                error!("io_uring completion wait failed: {error:?}");
                // The kernel may already own SQEs. Retain every guest pointer and buffer until a
                // target CQE is observed instead of turning an uncertain submit into use-after-free.
                thread::yield_now();
                continue;
            }
            completed_queues |= self.reap_linux_raw_requests(pending, bounce_pool, completions);
        }
        completed_queues
    }

    #[cfg(target_os = "linux")]
    fn complete_linux_raw_request(
        &mut self,
        request: PendingLinuxRawRequest,
        success: bool,
        result: i32,
        mem: &GuestMemoryMmap,
        bounce_pool: &mut LinuxRawBouncePool,
    ) {
        let data_result = if success && request.direction == LinuxRawDirection::Read {
            request.copy_bounce_read_to_guest()
        } else {
            Ok(())
        };
        let status = if success && data_result.is_ok() {
            match request.direction {
                LinuxRawDirection::Read => self.metrics.add_read_bytes(request.data_len as u64),
                LinuxRawDirection::Write => self.metrics.add_write_bytes(request.data_len as u64),
            }
            VIRTIO_BLK_S_OK as u8
        } else {
            if result < 0 {
                error!("io_uring block operation failed with errno {}", -result);
            }
            if let Err(error) = data_result {
                error!("failed to copy io_uring bounce read into guest memory: {error:?}");
            }
            VIRTIO_BLK_S_IOERR as u8
        };
        unsafe {
            write_volatile(request.status_ptr, status);
        }
        let used_len = if status == VIRTIO_BLK_S_OK as u8 {
            request.data_len
        } else {
            0
        };
        let queue_index = request.queue_index;
        let head_index = request.head_index;
        if let LinuxRawRequestData::Bounce { buffer, .. } = request.data {
            bounce_pool.release(buffer);
        }
        self.publish_linux_raw_completion(queue_index, head_index, used_len, mem);
    }

    #[cfg(target_os = "linux")]
    fn complete_linux_raw_admission_error(
        &mut self,
        queue_index: usize,
        head_index: u16,
        status_ptr: *mut u8,
        mem: &GuestMemoryMmap,
    ) {
        unsafe {
            write_volatile(status_ptr, VIRTIO_BLK_S_IOERR as u8);
        }
        self.publish_linux_raw_completion(queue_index, head_index, 0, mem);
    }

    #[cfg(target_os = "linux")]
    fn linux_raw_writer<'a>(
        &self,
        queue_index: usize,
        head_index: u16,
        mem: &'a GuestMemoryMmap,
    ) -> io::Result<Writer<'a>> {
        let queue = &self.device_queues[queue_index].queue;
        let chain =
            DescriptorChain::checked_new(mem, queue.desc_table, queue.actual_size(), head_index)
                .ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidData, "invalid descriptor chain")
                })?;
        Writer::new(mem, chain).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
    }

    #[cfg(target_os = "linux")]
    fn complete_linux_raw_error(
        &mut self,
        queue_index: usize,
        head_index: u16,
        mem: &GuestMemoryMmap,
    ) {
        if let Ok(mut writer) = self.linux_raw_writer(queue_index, head_index, mem) {
            let status_offset = writer.available_bytes().saturating_sub(1);
            if let Ok(mut status_writer) = writer.split_at(status_offset) {
                let _ = status_writer.write_obj(VIRTIO_BLK_S_IOERR as u8);
            }
        }
        self.publish_linux_raw_completion(queue_index, head_index, 0, mem);
    }

    #[cfg(target_os = "linux")]
    fn complete_linux_sync_request(
        &mut self,
        queue_index: usize,
        head_index: u16,
        mem: &GuestMemoryMmap,
        writer: &mut Writer,
        status: u8,
        used_len: usize,
    ) {
        if let Err(error) = writer.write_obj(status) {
            log::error!("failed to write block completion status: {error:?}");
        }
        self.publish_linux_raw_completion(queue_index, head_index, used_len, mem);
    }

    #[cfg(target_os = "linux")]
    fn publish_linux_raw_completion(
        &mut self,
        queue_index: usize,
        head_index: u16,
        used_len: usize,
        mem: &GuestMemoryMmap,
    ) {
        if let Err(error) =
            self.device_queues[queue_index]
                .queue
                .add_used(mem, head_index, used_len as u32)
        {
            log::error!("failed to publish io_uring block completion: {error:?}");
            return;
        }
        if !self.batch_completions {
            self.signal_linux_raw_queue(queue_index, mem);
        }
    }

    #[cfg(target_os = "linux")]
    fn signal_linux_raw_queue(&mut self, queue_index: usize, mem: &GuestMemoryMmap) {
        // Consume EVENT_IDX accounting, but do not let suppression defer progress to a future
        // request: persistent io_uring may have no later event after publishing this used entry.
        // The async path therefore emits one interrupt per completion, or one per CQ batch when
        // `block-completions` is enabled.
        let _ = self.device_queues[queue_index]
            .queue
            .needs_notification(mem);
        if let Err(error) = self.interrupt.try_signal_used_queue() {
            log::error!("failed to signal io_uring block completion: {error:?}");
        }
    }

    #[cfg(target_os = "linux")]
    fn signal_linux_raw_batch(&mut self, completed_queues: u64, mem: &GuestMemoryMmap) {
        let mut notification_requested = false;
        for queue_index in 0..self.device_queues.len().min(u64::BITS as usize) {
            if completed_queues & (1u64 << queue_index) == 0 {
                continue;
            }
            notification_requested |= self.device_queues[queue_index]
                .queue
                .needs_notification(mem)
                .unwrap_or(false);
        }

        if !self
            .linux_raw_completion_policy
            .uses_conditional_interrupts()
            || notification_requested
        {
            if let Err(error) = self.interrupt.try_signal_used_queue() {
                log::error!("failed to signal batched io_uring completions: {error:?}");
            }
        }
    }

    #[cfg(windows)]
    fn process_queue(
        &mut self,
        queue_index: usize,
        mem: &GuestMemoryMmap,
        pending_raw: &mut HashMap<usize, PendingWindowsBlockRequest>,
    ) {
        while pending_raw.len() < MAX_PENDING_WINDOWS_RAW_REQUESTS {
            let Some(head) = self.device_queues[queue_index].queue.pop(mem) else {
                break;
            };

            let mut reader = match Reader::new(mem, head.clone()) {
                Ok(r) => r,
                Err(e) => {
                    error!("invalid descriptor chain: {e:?}");
                    continue;
                }
            };
            let mut writer = match Writer::new(mem, head.clone()) {
                Ok(r) => r,
                Err(e) => {
                    error!("invalid descriptor chain: {e:?}");
                    continue;
                }
            };
            let request_header: RequestHeader = match reader.read_obj() {
                Ok(h) => h,
                Err(e) => {
                    error!("invalid request header: {e:?}");
                    continue;
                }
            };

            match self.try_submit_windows_raw_request(
                queue_index,
                mem,
                head.clone(),
                request_header,
                &mut reader,
                &mut writer,
                pending_raw,
            ) {
                Ok(WindowsRawSubmission::Submitted) => continue,
                Ok(WindowsRawSubmission::Fallback) => {}
                Err(e) => {
                    error!("error submitting raw Windows block request: {e:?}");
                    self.complete_sync_request(
                        queue_index,
                        mem,
                        head.index,
                        &mut writer,
                        VIRTIO_BLK_S_IOERR.try_into().unwrap(),
                        0,
                    );
                    continue;
                }
            }

            if !pending_raw.is_empty() {
                self.complete_all_windows_raw_requests(pending_raw);
            }

            let (status, len): (u8, usize) =
                match self.process_request(request_header, &mut reader, &mut writer) {
                    Ok(l) => (VIRTIO_BLK_S_OK.try_into().unwrap(), l),
                    Err(e) => {
                        error!("error processing request: {e:?}");
                        (VIRTIO_BLK_S_IOERR.try_into().unwrap(), 0)
                    }
                };

            self.complete_sync_request(queue_index, mem, head.index, &mut writer, status, len);
        }
    }

    #[cfg(windows)]
    fn complete_sync_request(
        &mut self,
        queue_index: usize,
        mem: &GuestMemoryMmap,
        head_index: u16,
        writer: &mut Writer,
        status: u8,
        len: usize,
    ) {
        if let Err(e) = writer.write_obj(status) {
            error!("Failed to write virtio block status: {e:?}")
        }

        if let Err(e) = self.device_queues[queue_index]
            .queue
            .add_used(mem, head_index, len as u32)
        {
            error!("failed to add used elements to the queue: {e:?}");
        }

        if self.device_queues[queue_index]
            .queue
            .needs_notification(mem)
            .unwrap()
        {
            if let Err(e) = self.interrupt.try_signal_used_queue() {
                error!("error signalling queue: {e:?}");
            }
        }
    }

    #[cfg(windows)]
    fn try_submit_windows_raw_request(
        &mut self,
        queue_index: usize,
        mem: &GuestMemoryMmap,
        head: DescriptorChain,
        request_header: RequestHeader,
        reader: &mut Reader,
        writer: &mut Writer,
        pending_raw: &mut HashMap<usize, PendingWindowsBlockRequest>,
    ) -> result::Result<WindowsRawSubmission, RequestError> {
        if !self.disk.has_windows_raw_file() {
            return Ok(WindowsRawSubmission::Fallback);
        }

        match request_header.request_type {
            VIRTIO_BLK_T_IN => {
                let data_len = writer
                    .available_bytes()
                    .checked_sub(1)
                    .ok_or(RequestError::InvalidDataLength)?;
                if data_len == 0 {
                    return Ok(WindowsRawSubmission::Fallback);
                }
                if !data_len.is_multiple_of(512) {
                    return Err(RequestError::InvalidDataLength);
                }

                let offset = sector_offset(request_header.sector)?;
                let segments = collect_windows_raw_buffers(mem, head.clone(), true, 0, data_len)
                    .map_err(RequestError::WritingToDescriptor)?;
                let (operation, _) =
                    self.submit_windows_raw_read_operation(offset, data_len, &segments)?;
                self.queue_windows_raw_request(
                    pending_raw,
                    queue_index,
                    head.index,
                    mem.clone(),
                    PendingWindowsBlockDirection::Read,
                    data_len,
                    operation,
                );
                Ok(WindowsRawSubmission::Submitted)
            }
            VIRTIO_BLK_T_OUT => {
                let data_len = reader.available_bytes();
                if data_len == 0 {
                    return Ok(WindowsRawSubmission::Fallback);
                }
                if !data_len.is_multiple_of(512) {
                    return Err(RequestError::InvalidDataLength);
                }

                let offset = sector_offset(request_header.sector)?;
                let segments = collect_windows_raw_buffers(
                    mem,
                    head.clone(),
                    false,
                    std::mem::size_of::<RequestHeader>(),
                    data_len,
                )
                .map_err(RequestError::ReadingFromDescriptor)?;
                let operation =
                    self.submit_windows_raw_write_operation(offset, data_len, reader, &segments)?;
                self.queue_windows_raw_request(
                    pending_raw,
                    queue_index,
                    head.index,
                    mem.clone(),
                    PendingWindowsBlockDirection::Write,
                    data_len,
                    operation,
                );
                Ok(WindowsRawSubmission::Submitted)
            }
            _ => Ok(WindowsRawSubmission::Fallback),
        }
    }

    #[cfg(windows)]
    fn submit_windows_raw_read_operation(
        &self,
        offset: u64,
        data_len: usize,
        segments: &[WindowsRawFileBuffer],
    ) -> result::Result<(PendingWindowsRawFileOperation, bool), RequestError> {
        if let Some(buffer) = single_direct_windows_raw_buffer(segments) {
            if self
                .disk
                .windows_raw_can_submit_direct_buffer(buffer, offset)
            {
                let operation = self
                    .disk
                    .submit_windows_raw_read_buffer(buffer, offset)
                    .expect("checked windows raw file presence")
                    .map_err(RequestError::WritingToDescriptor)?;
                return Ok((operation, false));
            }
        }

        log::debug!("using Windows raw block bounce buffer for fragmented or unaligned read");
        let operation = self
            .disk
            .submit_windows_raw_read_bounce(offset, data_len)
            .expect("checked windows raw file presence")
            .map_err(RequestError::WritingToDescriptor)?;
        Ok((operation, true))
    }

    #[cfg(windows)]
    fn submit_windows_raw_write_operation(
        &self,
        offset: u64,
        data_len: usize,
        reader: &mut Reader,
        segments: &[WindowsRawFileBuffer],
    ) -> result::Result<PendingWindowsRawFileOperation, RequestError> {
        if let Some(buffer) = single_direct_windows_raw_buffer(segments) {
            if self
                .disk
                .windows_raw_can_submit_direct_buffer(buffer, offset)
            {
                return self
                    .disk
                    .submit_windows_raw_write_buffer(buffer, offset)
                    .expect("checked windows raw file presence")
                    .map_err(RequestError::ReadingFromDescriptor);
            }
        }

        log::debug!("using Windows raw block bounce buffer for fragmented or unaligned write");
        let mut bounce = vec![0; data_len];
        reader
            .read_exact(&mut bounce)
            .map_err(RequestError::ReadingFromDescriptor)?;
        self.disk
            .submit_windows_raw_write_bounce(offset, bounce)
            .expect("checked windows raw file presence")
            .map_err(RequestError::ReadingFromDescriptor)
    }

    #[cfg(windows)]
    fn queue_windows_raw_request(
        &self,
        pending_raw: &mut HashMap<usize, PendingWindowsBlockRequest>,
        queue_index: usize,
        head_index: u16,
        mem: GuestMemoryMmap,
        direction: PendingWindowsBlockDirection,
        data_len: usize,
        operation: PendingWindowsRawFileOperation,
    ) {
        let key = operation.completion_key();
        pending_raw.insert(
            key,
            PendingWindowsBlockRequest {
                queue_index,
                head_index,
                mem,
                direction,
                data_len,
                operation: Some(operation),
            },
        );
    }

    #[cfg(windows)]
    fn complete_one_windows_raw_request(
        &mut self,
        pending_raw: &mut HashMap<usize, PendingWindowsBlockRequest>,
    ) {
        let completion = match self.disk.wait_windows_raw_completion() {
            Some(Ok(completion)) => completion,
            Some(Err(e)) => {
                error!("error waiting for Windows raw block completion: {e:?}");
                return;
            }
            None => {
                error!("Windows raw completion requested without a raw file");
                return;
            }
        };

        let key = completion.key();
        let Some(request) = pending_raw.remove(&key) else {
            error!("received completion for unknown Windows raw block request: {key}");
            return;
        };

        self.complete_windows_raw_request(request, completion);
    }

    #[cfg(windows)]
    fn complete_all_windows_raw_requests(
        &mut self,
        pending_raw: &mut HashMap<usize, PendingWindowsBlockRequest>,
    ) {
        while !pending_raw.is_empty() {
            let pending_count = pending_raw.len();
            self.complete_one_windows_raw_request(pending_raw);
            if pending_raw.len() == pending_count {
                break;
            }
        }
    }

    #[cfg(windows)]
    fn complete_windows_raw_request(
        &mut self,
        mut request: PendingWindowsBlockRequest,
        completion: WindowsRawFileCompletion,
    ) {
        let operation = request
            .operation
            .take()
            .expect("pending Windows raw request must own an operation");
        let completed = match operation.complete(completion) {
            Ok(completed) => completed,
            Err(e) => {
                error!("error completing Windows raw block request: {e:?}");
                self.write_windows_raw_completion(
                    &request,
                    VIRTIO_BLK_S_IOERR.try_into().unwrap(),
                    None,
                    0,
                );
                return;
            }
        };

        let status = VIRTIO_BLK_S_OK.try_into().unwrap();
        let used_len = completed.bytes;
        let buffer = completed.buffer.as_deref();
        if self.write_windows_raw_completion(&request, status, buffer, used_len) {
            match request.direction {
                PendingWindowsBlockDirection::Read => self.metrics.add_read_bytes(used_len as u64),
                PendingWindowsBlockDirection::Write => {
                    self.metrics.add_write_bytes(used_len as u64)
                }
            }
        }
    }

    #[cfg(windows)]
    fn write_windows_raw_completion(
        &mut self,
        request: &PendingWindowsBlockRequest,
        mut status: u8,
        bounce_read: Option<&[u8]>,
        used_len: usize,
    ) -> bool {
        let write_result = match request.direction {
            PendingWindowsBlockDirection::Read => {
                self.write_windows_raw_read_completion(request, status, bounce_read)
            }
            PendingWindowsBlockDirection::Write => {
                self.write_windows_raw_write_completion(request, status)
            }
        };

        if let Err(e) = write_result {
            error!("failed to write Windows raw block completion status: {e:?}");
            status = VIRTIO_BLK_S_IOERR.try_into().unwrap();
            let _ = self.write_windows_raw_status_at_offset(request, status);
        }

        let len = if status == VIRTIO_BLK_S_OK.try_into().unwrap() {
            used_len
        } else {
            0
        };
        if let Err(e) = self.device_queues[request.queue_index].queue.add_used(
            &request.mem,
            request.head_index,
            len as u32,
        ) {
            error!("failed to add used elements to the queue: {e:?}");
        }

        if self.device_queues[request.queue_index]
            .queue
            .needs_notification(&request.mem)
            .unwrap()
        {
            if let Err(e) = self.interrupt.try_signal_used_queue() {
                error!("error signalling queue: {e:?}");
            }
        }

        status == VIRTIO_BLK_S_OK.try_into().unwrap()
    }

    #[cfg(windows)]
    fn write_windows_raw_read_completion(
        &self,
        request: &PendingWindowsBlockRequest,
        status: u8,
        bounce_read: Option<&[u8]>,
    ) -> io::Result<()> {
        if let Some(buffer) = bounce_read {
            let mut writer = self.windows_raw_writer(request)?;
            writer.write_all(buffer)?;
            writer.write_obj(status)
        } else {
            self.write_windows_raw_status_at_offset(request, status)
        }
    }

    #[cfg(windows)]
    fn write_windows_raw_write_completion(
        &self,
        request: &PendingWindowsBlockRequest,
        status: u8,
    ) -> io::Result<()> {
        let mut writer = self.windows_raw_writer(request)?;
        writer.write_obj(status)
    }

    #[cfg(windows)]
    fn write_windows_raw_status_at_offset(
        &self,
        request: &PendingWindowsBlockRequest,
        status: u8,
    ) -> io::Result<()> {
        let mut writer = self.windows_raw_writer(request)?;
        let mut status_writer = writer
            .split_at(request.data_len)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        status_writer.write_obj(status)
    }

    #[cfg(windows)]
    fn windows_raw_writer<'a>(
        &self,
        request: &'a PendingWindowsBlockRequest,
    ) -> io::Result<Writer<'a>> {
        let chain = DescriptorChain::checked_new(
            &request.mem,
            self.device_queues[request.queue_index].queue.desc_table,
            self.device_queues[request.queue_index].queue.actual_size(),
            request.head_index,
        )
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid descriptor chain"))?;
        Writer::new(&request.mem, chain).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
    }

    fn process_request(
        &mut self,
        request_header: RequestHeader,
        reader: &mut Reader,
        writer: &mut Writer,
    ) -> result::Result<usize, RequestError> {
        match request_header.request_type {
            VIRTIO_BLK_T_IN => {
                let data_len = writer
                    .available_bytes()
                    .checked_sub(1)
                    .ok_or(RequestError::InvalidDataLength)?;
                if !data_len.is_multiple_of(512) {
                    Err(RequestError::InvalidDataLength)
                } else {
                    let offset = sector_offset(request_header.sector)?;
                    let len = writer
                        .write_from_at(&self.disk, data_len, offset)
                        .map_err(RequestError::WritingToDescriptor)?;
                    self.metrics.add_read_bytes(len as u64);
                    Ok(len)
                }
            }
            VIRTIO_BLK_T_OUT => {
                let data_len = reader.available_bytes();
                if !data_len.is_multiple_of(512) {
                    Err(RequestError::InvalidDataLength)
                } else {
                    let offset = sector_offset(request_header.sector)?;
                    let len = reader
                        .read_to_at(&self.disk, data_len, offset)
                        .map_err(RequestError::ReadingFromDescriptor)?;
                    self.metrics.add_write_bytes(len as u64);
                    Ok(len)
                }
            }
            VIRTIO_BLK_T_FLUSH => match self.disk.cache_type() {
                CacheType::Writeback => {
                    #[cfg(feature = "block-io-profile")]
                    let started = Instant::now();
                    let result = self.disk.flush_to_disk();
                    #[cfg(feature = "block-io-profile")]
                    self.metrics.record_flush_ns(duration_ns(started.elapsed()));
                    result.map_err(RequestError::FlushingToDisk)?;
                    Ok(0)
                }
                CacheType::Unsafe => {
                    #[cfg(feature = "block-io-profile")]
                    self.metrics.record_flush_ns(0);
                    Ok(0)
                }
            },
            VIRTIO_BLK_T_GET_ID => {
                let data_len = writer.available_bytes();
                let disk_id = self.disk.image_id();
                if data_len < disk_id.len() {
                    Err(RequestError::InvalidDataLength)
                } else {
                    writer
                        .write_all(disk_id)
                        .map_err(RequestError::WritingToDescriptor)?;
                    Ok(disk_id.len())
                }
            }
            VIRTIO_BLK_T_DISCARD => {
                let discard_write_data: DiscardWriteData = reader
                    .read_obj()
                    .map_err(RequestError::ReadingFromDescriptor)?;
                self.disk
                    .discard_to_any(
                        sector_offset(discard_write_data.sector)?,
                        sector_count_bytes(discard_write_data.num_sectors)?,
                    )
                    .map_err(RequestError::Discarding)?;
                Ok(0)
            }
            VIRTIO_BLK_T_WRITE_ZEROES => {
                let discard_write_data: DiscardWriteData = reader
                    .read_obj()
                    .map_err(RequestError::ReadingFromDescriptor)?;
                let unmap = (discard_write_data.flags & VIRTIO_BLK_WRITE_ZEROES_FLAG_UNMAP) != 0;
                if unmap {
                    self.disk
                        .discard_to_zero(
                            sector_offset(discard_write_data.sector)?,
                            sector_count_bytes(discard_write_data.num_sectors)?,
                        )
                        .map_err(RequestError::DiscardingToZero)?;
                } else {
                    self.disk
                        .write_zeroes(
                            sector_offset(discard_write_data.sector)?,
                            sector_count_bytes(discard_write_data.num_sectors)?,
                        )
                        .map_err(RequestError::WritingZeroes)?;
                }
                Ok(0)
            }
            _ => Err(RequestError::UnknownRequest),
        }
    }
}

#[cfg(target_os = "linux")]
fn snapshot_linux_raw_buffers<'a>(
    slices: impl IntoIterator<Item = VolatileSlice<'a>>,
    writable: bool,
    expected_len: usize,
) -> io::Result<SmallVec<[LinuxRawFileBuffer; 4]>> {
    let mut buffers = SmallVec::new();
    let mut total = 0usize;
    for slice in slices {
        if slice.is_empty() {
            continue;
        }
        let ptr = if writable {
            slice.ptr_guard_mut().as_ptr()
        } else {
            slice.ptr_guard().as_ptr() as *mut u8
        };
        let buffer = unsafe { LinuxRawFileBuffer::new(ptr, slice.len()) };
        push_or_merge_linux_raw_buffer(&mut buffers, buffer);
        total = total
            .checked_add(slice.len())
            .ok_or_else(|| io::Error::other("guest iovec length overflow"))?;
    }

    if total != expected_len {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            format!("guest iovecs contain {total} bytes, expected {expected_len}"),
        ));
    }
    Ok(buffers)
}

#[cfg(target_os = "linux")]
fn push_or_merge_linux_raw_buffer(
    buffers: &mut SmallVec<[LinuxRawFileBuffer; 4]>,
    buffer: LinuxRawFileBuffer,
) {
    if let Some(last) = buffers.last_mut() {
        let last_end = (last.ptr as usize).saturating_add(last.len);
        if last_end == buffer.ptr as usize {
            last.len += buffer.len;
            return;
        }
    }
    buffers.push(buffer);
}

#[cfg(target_os = "linux")]
fn copy_linux_guest_buffers_to_bounce(
    guest_buffers: &[LinuxRawFileBuffer],
    bounce: &mut [u8],
) -> io::Result<()> {
    let mut copied = 0usize;
    for guest in guest_buffers {
        let end = copied
            .checked_add(guest.len)
            .ok_or_else(|| io::Error::other("bounce write length overflow"))?;
        let destination = bounce
            .get_mut(copied..end)
            .ok_or_else(|| io::Error::other("bounce write exceeded its buffer"))?;
        unsafe {
            copy_nonoverlapping(guest.ptr.cast_const(), destination.as_mut_ptr(), guest.len);
        }
        copied = end;
    }
    (copied == bounce.len())
        .then_some(())
        .ok_or_else(|| io::Error::other("bounce write did not cover every source byte"))
}

#[cfg(target_os = "linux")]
fn linux_raw_remainder_is_aligned(
    backend: &LinuxRawBackend,
    request: &PendingLinuxRawRequest,
) -> bool {
    if !backend.direct_io {
        return true;
    }
    let Some(offset) = request.offset.checked_add(request.completed as u64) else {
        return false;
    };
    if !linux_raw_request_is_aligned(true, backend.req_align, offset, request.remaining()) {
        return false;
    }
    match &request.data {
        LinuxRawRequestData::Guest { iovecs, .. } => iovecs.iter().all(|iovec| {
            (iovec.iov_base as usize).is_multiple_of(backend.mem_align)
                && iovec.iov_len.is_multiple_of(backend.mem_align)
        }),
        LinuxRawRequestData::Bounce { .. } => request.completed.is_multiple_of(backend.mem_align),
    }
}

#[cfg(windows)]
fn collect_windows_raw_buffers(
    mem: &GuestMemoryMmap,
    head: DescriptorChain,
    writable: bool,
    mut skip: usize,
    mut remaining: usize,
) -> io::Result<Vec<WindowsRawFileBuffer>> {
    let mut buffers = Vec::new();

    for desc in head.into_iter() {
        if desc.is_write_only() != writable {
            continue;
        }

        let mut addr = desc.addr;
        let mut len = desc.len as usize;
        if skip > 0 {
            let skipped = skip.min(len);
            addr = addr
                .checked_add(skipped as u64)
                .ok_or_else(|| io::Error::other("descriptor address overflow"))?;
            len -= skipped;
            skip -= skipped;
        }
        if len == 0 {
            continue;
        }

        let chunk_len = len.min(remaining);
        let slice = mem
            .get_slice(addr, chunk_len)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        let ptr = if writable {
            slice.ptr_guard_mut().as_ptr()
        } else {
            slice.ptr_guard().as_ptr() as *mut u8
        };

        // SAFETY: `ptr` was validated through `GuestMemoryMmap::get_slice`.
        // The pending request keeps a cloned `GuestMemoryMmap` alive until IOCP
        // completion, and the descriptor is not returned to the guest before then.
        let buffer = unsafe { WindowsRawFileBuffer::new(ptr, chunk_len) };
        push_or_merge_windows_raw_buffer(&mut buffers, buffer);

        remaining -= chunk_len;
        if remaining == 0 {
            break;
        }
    }

    if remaining != 0 || skip != 0 {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "descriptor chain does not contain enough raw block data",
        ));
    }

    Ok(buffers)
}

#[cfg(windows)]
fn push_or_merge_windows_raw_buffer(
    buffers: &mut Vec<WindowsRawFileBuffer>,
    buffer: WindowsRawFileBuffer,
) {
    if let Some(last) = buffers.last_mut() {
        let last_end = (last.as_mut_ptr() as usize).saturating_add(last.len());
        if last_end == buffer.as_mut_ptr() as usize {
            // SAFETY: both buffers came from adjacent validated guest-memory
            // ranges and remain covered by the pending request's memory guard.
            *last =
                unsafe { WindowsRawFileBuffer::new(last.as_mut_ptr(), last.len() + buffer.len()) };
            return;
        }
    }

    buffers.push(buffer);
}

#[cfg(windows)]
fn single_direct_windows_raw_buffer(
    buffers: &[WindowsRawFileBuffer],
) -> Option<WindowsRawFileBuffer> {
    match buffers {
        [buffer] => Some(*buffer),
        _ => None,
    }
}

#[cfg(feature = "block-io-profile")]
fn block_request_kind(request_type: u32) -> BlockRequestKind {
    match request_type {
        VIRTIO_BLK_T_IN => BlockRequestKind::Read,
        VIRTIO_BLK_T_OUT => BlockRequestKind::Write,
        VIRTIO_BLK_T_FLUSH => BlockRequestKind::Flush,
        _ => BlockRequestKind::Other,
    }
}

#[cfg(feature = "block-io-profile")]
fn request_data_scratch_vectors(request_type: u32) -> u64 {
    match request_type {
        VIRTIO_BLK_T_IN
        | VIRTIO_BLK_T_OUT
        | VIRTIO_BLK_T_GET_ID
        | VIRTIO_BLK_T_DISCARD
        | VIRTIO_BLK_T_WRITE_ZEROES => 1,
        _ => 0,
    }
}

#[cfg(feature = "block-io-profile")]
fn duration_ns(duration: Duration) -> u64 {
    duration.as_nanos().try_into().unwrap_or(u64::MAX)
}

#[cfg(unix)]
fn eventfd_pollable(event: &EventFd) -> Pollable {
    event.as_raw_fd()
}

#[cfg(windows)]
fn eventfd_pollable(event: &EventFd) -> Pollable {
    event.as_raw_handle()
}

fn sector_offset(sector: u64) -> result::Result<u64, RequestError> {
    sector
        .checked_mul(512)
        .ok_or(RequestError::InvalidDataLength)
}

fn sector_count_bytes(sectors: u32) -> result::Result<u64, RequestError> {
    u64::from(sectors)
        .checked_mul(512)
        .ok_or(RequestError::InvalidDataLength)
}

#[cfg(target_os = "linux")]
fn linux_raw_request_is_aligned(
    direct_io: bool,
    req_align: usize,
    offset: u64,
    length: usize,
) -> bool {
    if !direct_io {
        return true;
    }

    req_align > 0 && offset.is_multiple_of(req_align as u64) && length.is_multiple_of(req_align)
}

//--------------------------------------------------------------------------------------------------
// Tests
//--------------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "linux")]
    fn pending_guest_request(
        buffers: SmallVec<[LinuxRawFileBuffer; 4]>,
        status_ptr: *mut u8,
        data_len: usize,
    ) -> PendingLinuxRawRequest {
        PendingLinuxRawRequest {
            queue_index: 0,
            head_index: 0,
            direction: LinuxRawDirection::Read,
            data: LinuxRawRequestData::Guest {
                iovecs: SmallVec::with_capacity(buffers.len()),
                buffers,
            },
            status_ptr,
            data_len,
            offset: 0,
            completed: 0,
        }
    }

    #[test]
    fn sector_offset_rejects_overflow() {
        assert!(matches!(
            sector_offset(u64::MAX),
            Err(RequestError::InvalidDataLength)
        ));
    }

    #[test]
    fn sector_count_bytes_converts_to_bytes() {
        assert_eq!(sector_count_bytes(8).unwrap(), 4096);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn direct_linux_raw_requests_require_file_alignment() {
        assert!(linux_raw_request_is_aligned(true, 4096, 8192, 4096));
        assert!(!linux_raw_request_is_aligned(true, 4096, 512, 4096));
        assert!(!linux_raw_request_is_aligned(true, 4096, 8192, 512));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn buffered_linux_raw_requests_do_not_require_direct_alignment() {
        assert!(linux_raw_request_is_aligned(false, 4096, 512, 512));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn guest_iovec_cursor_advances_across_segments() {
        let mut first = [0u8; 4];
        let mut second = [0u8; 8];
        let mut status = 0u8;
        let buffers = SmallVec::from_vec(vec![
            unsafe { LinuxRawFileBuffer::new(first.as_mut_ptr(), first.len()) },
            unsafe { LinuxRawFileBuffer::new(second.as_mut_ptr(), second.len()) },
        ]);
        let mut request = pending_guest_request(buffers, &mut status, 12);

        request.rebuild_iovecs().unwrap();
        let LinuxRawRequestData::Guest { iovecs, .. } = &request.data else {
            unreachable!();
        };
        assert_eq!(iovecs.len(), 2);
        assert_eq!(iovecs[0].iov_base, first.as_mut_ptr().cast());
        assert_eq!(iovecs[0].iov_len, 4);

        request.completed = 6;
        request.rebuild_iovecs().unwrap();
        let LinuxRawRequestData::Guest { iovecs, .. } = &request.data else {
            unreachable!();
        };
        assert_eq!(iovecs.len(), 1);
        assert_eq!(
            iovecs[0].iov_base,
            unsafe { second.as_mut_ptr().add(2) }.cast()
        );
        assert_eq!(iovecs[0].iov_len, 6);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn request_table_rejects_stale_generation_ids() {
        let mut data = [0u8; 512];
        let mut status = 0u8;
        let buffer = unsafe { LinuxRawFileBuffer::new(data.as_mut_ptr(), data.len()) };
        let mut pending = PendingLinuxRawRequests::new();
        let first_id = pending
            .insert(pending_guest_request(
                SmallVec::from_slice(&[buffer]),
                &mut status,
                data.len(),
            ))
            .unwrap();
        pending.remove(first_id).unwrap();
        let second_id = pending
            .insert(pending_guest_request(
                SmallVec::from_slice(&[buffer]),
                &mut status,
                data.len(),
            ))
            .unwrap();

        assert_ne!(first_id, second_id);
        assert!(pending.get_mut(first_id).is_none());
        assert!(pending.get_mut(second_id).is_some());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn bounce_pool_reuses_matching_allocations() {
        let mut pool = LinuxRawBouncePool::default();
        let mut first = pool.acquire(4096, 4096).unwrap();
        let first_ptr = first.as_mut().as_ptr();
        pool.release(first);
        let mut second = pool.acquire(4096, 4096).unwrap();

        assert_eq!(second.as_mut().as_ptr(), first_ptr);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn completion_policy_parses_development_modes() {
        assert_eq!(
            LinuxRawCompletionPolicy::parse(None),
            Some(LinuxRawCompletionPolicy::WaitAll)
        );
        assert_eq!(
            LinuxRawCompletionPolicy::parse(Some("watermark-8")),
            Some(LinuxRawCompletionPolicy::WatermarkLow)
        );
        assert_eq!(
            LinuxRawCompletionPolicy::parse(Some("watermark-16")),
            Some(LinuxRawCompletionPolicy::WatermarkHigh)
        );
        assert_eq!(
            LinuxRawCompletionPolicy::parse(Some("local-drain")),
            Some(LinuxRawCompletionPolicy::LocalDrain)
        );
        assert_eq!(LinuxRawCompletionPolicy::parse(Some("unknown")), None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn completion_policy_targets_are_bounded_by_pending_work() {
        assert_eq!(LinuxRawCompletionPolicy::WaitAll.completion_target(64), 64);
        assert_eq!(
            LinuxRawCompletionPolicy::WatermarkLow.completion_target(64),
            BLOCK_CQ_WATERMARK_LOW
        );
        assert_eq!(
            LinuxRawCompletionPolicy::WatermarkHigh.completion_target(4),
            4
        );
        assert_eq!(LinuxRawCompletionPolicy::Immediate.completion_target(64), 0);
    }
}
