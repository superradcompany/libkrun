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
use std::collections::HashMap;
#[cfg(target_os = "linux")]
use std::fs::File;
#[cfg(any(windows, target_os = "linux"))]
use std::io::Read;
use std::io::{self, Write};
#[cfg(unix)]
use std::os::fd::AsRawFd;
#[cfg(windows)]
use std::os::windows::io::{AsRawHandle, RawHandle};
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
#[cfg(windows)]
use vm_memory::{Address, GuestMemoryBackend};
use vm_memory::{ByteValued, GuestMemoryMmap};

#[cfg(unix)]
type Pollable = std::os::fd::RawFd;
#[cfg(windows)]
type Pollable = RawHandle;

const QUEUE_EVENT_BASE: u64 = 0;
const STOP_EVENT: u64 = 64;
#[cfg(target_os = "linux")]
const MAX_PENDING_LINUX_RAW_REQUESTS: usize = 64;
#[cfg(target_os = "linux")]
const MAX_LINUX_RAW_REQUEST_BYTES: usize = 1024 * 1024;
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
}

#[cfg(target_os = "linux")]
struct LinuxRawBackend {
    file: Arc<File>,
    ring: IoUring,
    next_request_id: u64,
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
    buffer: IoBuffer,
    offset: u64,
    completed: usize,
}

#[cfg(target_os = "linux")]
#[derive(Clone, Copy, Eq, PartialEq)]
enum LinuxRawDirection {
    Read,
    Write,
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
                    next_request_id: 1,
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
        }
    }

    pub fn run(self) -> thread::JoinHandle<()> {
        thread::Builder::new()
            .name("block worker".into())
            .spawn(|| self.work())
            .unwrap()
    }

    #[cfg(not(windows))]
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

    #[cfg(not(windows))]
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
        #[cfg(target_os = "linux")]
        if self.linux_raw.is_some() {
            self.process_linux_raw_queue(queue_index, mem);
            return;
        }

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
    fn process_linux_raw_queue(&mut self, queue_index: usize, mem: &GuestMemoryMmap) {
        let mut pending = HashMap::new();
        while pending.len() < MAX_PENDING_LINUX_RAW_REQUESTS {
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
                let mut buffer = match IoBuffer::new(length, mem_align) {
                    Ok(buffer) => buffer,
                    Err(error) => {
                        error!("failed to allocate aligned io_uring block buffer: {error:?}");
                        self.drain_linux_raw_requests(mem, &mut pending);
                        let (status, len) =
                            match self.process_request(request_header, &mut reader, &mut writer) {
                                Ok(length) => (VIRTIO_BLK_S_OK as u8, length),
                                Err(error) => {
                                    log::error!(
                                        "error processing synchronous block request: {error:?}"
                                    );
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
                        continue;
                    }
                };
                if direction == LinuxRawDirection::Write {
                    if let Err(error) = reader.read_exact(buffer.as_mut().into_slice()) {
                        error!("failed to stage io_uring block write: {error:?}");
                        self.complete_linux_raw_error(queue_index, head.index, mem);
                        continue;
                    }
                }
                let request = PendingLinuxRawRequest {
                    queue_index,
                    head_index: head.index,
                    direction,
                    buffer,
                    offset,
                    completed: 0,
                };
                if let Err(error) = self.submit_linux_raw_request(request, &mut pending) {
                    error!("failed to submit io_uring block request: {error:?}");
                    self.complete_linux_raw_error(queue_index, head.index, mem);
                }
                continue;
            }

            // Flush, discard, write-zeroes, identity, and malformed read/write requests use the
            // mature synchronous path. Draining first is the global write-epoch barrier: every
            // request dequeued before a flush is complete before the durability syscall begins.
            self.drain_linux_raw_requests(mem, &mut pending);
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
        }
        self.drain_linux_raw_requests(mem, &mut pending);
        if self.batch_completions {
            self.signal_linux_raw_queue(queue_index, mem);
        }
    }

    #[cfg(target_os = "linux")]
    fn submit_linux_raw_request(
        &mut self,
        request: PendingLinuxRawRequest,
        pending: &mut HashMap<u64, PendingLinuxRawRequest>,
    ) -> io::Result<()> {
        let backend = self.linux_raw.as_mut().expect("io_uring backend selected");
        let request_id = backend.next_request_id;
        backend.next_request_id = backend.next_request_id.wrapping_add(1).max(1);
        pending.insert(request_id, request);
        let request = pending.get_mut(&request_id).expect("request was inserted");
        if let Err(error) = Self::push_linux_raw_entry(backend, request_id, request) {
            pending.remove(&request_id);
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
        let remaining = request.buffer.len() - request.completed;
        let length = u32::try_from(remaining)
            .map_err(|_| io::Error::other("io_uring block request is too large"))?;
        let offset = request
            .offset
            .checked_add(request.completed as u64)
            .ok_or_else(|| io::Error::other("io_uring block offset overflow"))?;
        let entry = match request.direction {
            LinuxRawDirection::Read => opcode::Read::new(
                types::Fd(backend.file.as_raw_fd()),
                // SAFETY: `completed` never exceeds the buffer length, and the buffer remains
                // owned by the pending request until the matching CQE is reaped.
                unsafe { request.buffer.as_mut().as_ptr().add(request.completed) },
                length,
            )
            .offset(offset)
            .build(),
            LinuxRawDirection::Write => opcode::Write::new(
                types::Fd(backend.file.as_raw_fd()),
                // SAFETY: same bounded offset and lifetime argument as the read buffer above.
                unsafe { request.buffer.as_ref().as_ptr().add(request.completed) },
                length,
            )
            .offset(offset)
            .build(),
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
    fn drain_linux_raw_requests(
        &mut self,
        mem: &GuestMemoryMmap,
        pending: &mut HashMap<u64, PendingLinuxRawRequest>,
    ) {
        while !pending.is_empty() {
            let completions = {
                let backend = self.linux_raw.as_mut().expect("io_uring backend selected");
                if let Err(error) = backend.ring.submit_and_wait(1) {
                    error!("io_uring completion wait failed: {error:?}");
                    // An interrupted or transient submit may still have handed requests to the
                    // kernel. Keep every backing buffer alive and retry the reap rather than
                    // turning an uncertain submission into a use-after-free.
                    thread::yield_now();
                    continue;
                }
                backend
                    .ring
                    .completion()
                    .map(|entry| (entry.user_data(), entry.result()))
                    .collect::<Vec<_>>()
            };
            for (request_id, result) in completions {
                let Some(request) = pending.get_mut(&request_id) else {
                    error!("io_uring returned unknown block request {request_id}");
                    continue;
                };
                let completed = usize::try_from(result).ok();
                let remaining = request.buffer.len() - request.completed;
                if let Some(completed) = completed.filter(|completed| *completed < remaining) {
                    if completed > 0 {
                        request.completed += completed;
                        let can_resubmit = {
                            let backend =
                                self.linux_raw.as_ref().expect("io_uring backend selected");
                            request
                                .offset
                                .checked_add(request.completed as u64)
                                .is_some_and(|offset| {
                                    linux_raw_request_is_aligned(
                                        backend.direct_io,
                                        backend.req_align,
                                        offset,
                                        request.buffer.len() - request.completed,
                                    ) && (!backend.direct_io
                                        || request.completed.is_multiple_of(backend.mem_align))
                                })
                        };
                        let resubmit_result = if can_resubmit {
                            let backend =
                                self.linux_raw.as_mut().expect("io_uring backend selected");
                            Self::push_linux_raw_entry(backend, request_id, request)
                        } else {
                            Err(io::Error::other(
                                "partial direct-I/O completion left an unaligned remainder",
                            ))
                        };
                        if let Err(error) = resubmit_result {
                            error!("failed to resubmit partial io_uring request: {error:?}");
                            let request =
                                pending.remove(&request_id).expect("request still pending");
                            self.complete_linux_raw_request(request, false, result, mem);
                        }
                        continue;
                    }
                }
                let success = completed == Some(remaining);
                let request = pending.remove(&request_id).expect("request still pending");
                self.complete_linux_raw_request(request, success, result, mem);
            }
        }
    }

    #[cfg(target_os = "linux")]
    fn complete_linux_raw_request(
        &mut self,
        request: PendingLinuxRawRequest,
        success: bool,
        result: i32,
        mem: &GuestMemoryMmap,
    ) {
        let mut writer = match self.linux_raw_writer(request.queue_index, request.head_index, mem) {
            Ok(writer) => writer,
            Err(error) => {
                error!("failed to reconstruct io_uring completion descriptors: {error:?}");
                self.complete_linux_raw_error(request.queue_index, request.head_index, mem);
                return;
            }
        };
        let status = if success {
            let data_result = match request.direction {
                LinuxRawDirection::Read => writer.write_all(request.buffer.as_ref().into_slice()),
                LinuxRawDirection::Write => Ok(()),
            };
            if data_result.is_ok() {
                match request.direction {
                    LinuxRawDirection::Read => {
                        self.metrics.add_read_bytes(request.buffer.len() as u64)
                    }
                    LinuxRawDirection::Write => {
                        self.metrics.add_write_bytes(request.buffer.len() as u64)
                    }
                }
                VIRTIO_BLK_S_OK as u8
            } else {
                VIRTIO_BLK_S_IOERR as u8
            }
        } else {
            if result < 0 {
                error!("io_uring block operation failed with errno {}", -result);
            }
            VIRTIO_BLK_S_IOERR as u8
        };
        let _ = writer.write_obj(status);
        let used_len = if status == VIRTIO_BLK_S_OK as u8 {
            request.buffer.len()
        } else {
            0
        };
        self.publish_linux_raw_completion(request.queue_index, request.head_index, used_len, mem);
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
        if self.device_queues[queue_index]
            .queue
            .needs_notification(mem)
            .unwrap_or(false)
        {
            if let Err(error) = self.interrupt.try_signal_used_queue() {
                log::error!("failed to signal io_uring block completion: {error:?}");
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
}
