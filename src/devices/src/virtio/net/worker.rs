use crate::virtio::descriptor_utils::Reader;
use crate::virtio::net::backend::ConnectError;
#[cfg(windows)]
use crate::virtio::net::namedpipe::NamedPipe;
#[cfg(target_os = "linux")]
use crate::virtio::net::tap::Tap;
#[cfg(unix)]
use crate::virtio::net::unixgram::Unixgram;
#[cfg(unix)]
use crate::virtio::net::unixstream::Unixstream;
use crate::virtio::net::{MAX_BUFFER_SIZE, QUEUE_SIZE};
use crate::virtio::{DeviceQueue, InterruptTransport};

use super::backend::{NetBackend, ReadError, WriteError};
use super::device::{FrontendError, RxError, TxError, VirtioNetBackend};
use super::vnet_hdr_len;

use std::io;
#[cfg(unix)]
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
#[cfg(windows)]
use std::os::windows::io::{AsRawHandle, RawHandle};
use std::thread;
use std::{cmp, result};
use utils::epoll::{ControlOperation, Epoll, EpollEvent, EventSet};
use utils::event::{EventSource, RawEventSource};
use utils::eventfd::EventFd;
use virtio_bindings::virtio_net::{
    VIRTIO_NET_CTRL_MQ, VIRTIO_NET_CTRL_MQ_VQ_PAIRS_SET, VIRTIO_NET_ERR, VIRTIO_NET_OK,
};
use vm_memory::{Bytes, GuestAddress, GuestMemoryMmap};

#[cfg(unix)]
type Pollable = std::os::fd::RawFd;
#[cfg(windows)]
type Pollable = RawHandle;

const RX_QUEUE_EVENT_BASE: u64 = 0;
const TX_QUEUE_EVENT_BASE: u64 = 16;
const CONTROL_QUEUE_EVENT: u64 = 32;
const BACKEND_EVENT: u64 = 33;

pub struct NetWorker {
    rx_queues: Vec<DeviceQueue>,
    tx_queues: Vec<DeviceQueue>,
    control_queue: Option<DeviceQueue>,
    active_queue_pairs: usize,
    interrupt: InterruptTransport,

    mem: GuestMemoryMmap,
    backend: Box<dyn NetBackend + Send>,

    rx_frame_buf: [u8; MAX_BUFFER_SIZE],
    rx_frame_buf_len: usize,
    rx_has_deferred_frame: bool,

    tx_iovec: Vec<(GuestAddress, usize)>,
    tx_frame_buf: [u8; MAX_BUFFER_SIZE],
    tx_frame_len: usize,
}

impl NetWorker {
    pub fn new(
        rx_queues: Vec<DeviceQueue>,
        tx_queues: Vec<DeviceQueue>,
        control_queue: Option<DeviceQueue>,
        interrupt: InterruptTransport,
        mem: GuestMemoryMmap,
        _vnet_features: u64,
        cfg_backend: VirtioNetBackend,
    ) -> Result<Self, ConnectError> {
        debug_assert_eq!(rx_queues.len(), tx_queues.len());
        debug_assert!(!rx_queues.is_empty());
        let backend = match cfg_backend {
            #[cfg(unix)]
            VirtioNetBackend::UnixstreamFd(fd) => {
                // SAFETY: we need to trust that the library user has configured
                // the backend with a healthy file descriptor.
                let owned_fd = unsafe { OwnedFd::from_raw_fd(fd) };
                Box::new(Unixstream::new(owned_fd)) as Box<dyn NetBackend + Send>
            }
            #[cfg(unix)]
            VirtioNetBackend::UnixstreamPath(path) => {
                Box::new(Unixstream::open(path)?) as Box<dyn NetBackend + Send>
            }
            #[cfg(unix)]
            VirtioNetBackend::UnixgramFd(fd) => {
                // SAFETY: we need to trust that the library user has configured
                // the backend with a healthy file descriptor.
                let owned_fd = unsafe { OwnedFd::from_raw_fd(fd) };
                Box::new(Unixgram::new(owned_fd)) as Box<dyn NetBackend + Send>
            }
            #[cfg(unix)]
            VirtioNetBackend::UnixgramPath(path, vfkit_magic) => {
                Box::new(Unixgram::open(path, vfkit_magic)?) as Box<dyn NetBackend + Send>
            }
            #[cfg(target_os = "linux")]
            VirtioNetBackend::Tap(tap_name) => {
                Box::new(Tap::new(tap_name, _vnet_features)?) as Box<dyn NetBackend + Send>
            }
            #[cfg(windows)]
            VirtioNetBackend::NamedPipe(name) => {
                Box::new(NamedPipe::open(name)?) as Box<dyn NetBackend + Send>
            }
            VirtioNetBackend::Custom(backend) => backend,
        };

        Ok(Self {
            rx_queues,
            tx_queues,
            control_queue,
            active_queue_pairs: 1,

            mem,
            backend,
            interrupt,

            rx_frame_buf: [0u8; MAX_BUFFER_SIZE],
            rx_frame_buf_len: 0,
            rx_has_deferred_frame: false,

            tx_frame_buf: [0u8; MAX_BUFFER_SIZE],
            tx_frame_len: 0,
            tx_iovec: Vec::with_capacity(QUEUE_SIZE as usize),
        })
    }

    pub fn run(self) {
        thread::Builder::new()
            .name("virtio-net worker".into())
            .spawn(|| self.work())
            .unwrap();
    }

    fn work(mut self) {
        let backend_source = self.backend.event_source(BACKEND_EVENT);
        let backend_pollable = match event_source_pollable(backend_source) {
            Ok(pollable) => pollable,
            Err(err) => {
                log::error!("virtio-net backend event source is unsupported: {err}");
                return;
            }
        };

        let epoll = Epoll::new().unwrap();

        for (index, queue) in self.rx_queues.iter().enumerate() {
            let _ = epoll.ctl(
                ControlOperation::Add,
                eventfd_pollable(&queue.event),
                &EpollEvent::new(EventSet::IN, RX_QUEUE_EVENT_BASE + index as u64),
            );
        }
        for (index, queue) in self.tx_queues.iter().enumerate() {
            let _ = epoll.ctl(
                ControlOperation::Add,
                eventfd_pollable(&queue.event),
                &EpollEvent::new(EventSet::IN, TX_QUEUE_EVENT_BASE + index as u64),
            );
        }
        if let Some(queue) = &self.control_queue {
            let _ = epoll.ctl(
                ControlOperation::Add,
                eventfd_pollable(&queue.event),
                &EpollEvent::new(EventSet::IN, CONTROL_QUEUE_EVENT),
            );
        }
        let _ = epoll.ctl(
            ControlOperation::Add,
            backend_pollable,
            &EpollEvent::new(
                EventSet::IN | EventSet::OUT | EventSet::EDGE_TRIGGERED | EventSet::READ_HANG_UP,
                BACKEND_EVENT,
            ),
        );

        loop {
            let mut epoll_events = vec![EpollEvent::new(EventSet::empty(), 0); 32];
            match epoll.wait(epoll_events.len(), -1, epoll_events.as_mut_slice()) {
                Ok(ev_cnt) => {
                    for event in &epoll_events[0..ev_cnt] {
                        let source = event.data();
                        let event_set = event.event_set();
                        match source {
                            source
                                if is_rx_queue_event(source, self.rx_queues.len())
                                    && event_set.contains(EventSet::IN) =>
                            {
                                self.process_rx_queue_event(source as usize);
                            }
                            source
                                if is_tx_queue_event(source, self.tx_queues.len())
                                    && event_set.contains(EventSet::IN) =>
                            {
                                self.process_tx_queue_event(
                                    (source - TX_QUEUE_EVENT_BASE) as usize,
                                );
                            }
                            CONTROL_QUEUE_EVENT if event_set.contains(EventSet::IN) => {
                                self.process_control_queue_event();
                            }
                            BACKEND_EVENT => {
                                if event_set.contains(EventSet::HANG_UP)
                                    || event_set.contains(EventSet::READ_HANG_UP)
                                {
                                    log::error!("Got {event_set:?} on backend fd, virtio-net will stop working");
                                    eprintln!("LIBKRUN VIRTIO-NET FATAL: Backend process seems to have quit or crashed! Networking is now disabled!");
                                } else {
                                    if event_set.contains(EventSet::IN) {
                                        self.process_backend_socket_readable()
                                    }

                                    if event_set.contains(EventSet::OUT) {
                                        self.process_backend_socket_writeable()
                                    }
                                }
                            }
                            _ => {
                                log::warn!(
                                    "Received unknown virtio-net event: {event_set:?} token={source}"
                                );
                            }
                        }
                    }
                }
                Err(e) => {
                    debug!("vsock: failed to consume muxer epoll event: {e}");
                }
            }
        }
    }

    pub(crate) fn process_rx_queue_event(&mut self, queue_index: usize) {
        if let Err(e) = self.rx_queues[queue_index].event.read() {
            log::error!("Failed to get rx event from queue: {e:?}");
        }
        if let Err(e) = self.rx_queues[queue_index]
            .queue
            .disable_notification(&self.mem)
        {
            error!("error disabling queue notifications: {e:?}");
        }
        if let Err(e) = self.process_rx() {
            log::error!("Failed to process rx: {e:?} (triggered by queue event)")
        };
        if let Err(e) = self.rx_queues[queue_index]
            .queue
            .enable_notification(&self.mem)
        {
            error!("error disabling queue notifications: {e:?}");
        }
    }

    pub(crate) fn process_tx_queue_event(&mut self, queue_index: usize) {
        match self.tx_queues[queue_index].event.read() {
            Ok(_) => {
                log::debug!("virtio-net tx queue event: {queue_index}");
                self.process_tx_loop(queue_index)
            }
            Err(e) => {
                log::error!("Failed to get tx queue event from queue: {e:?}");
            }
        }
    }

    pub(crate) fn process_control_queue_event(&mut self) {
        let max_queue_pairs = self.rx_queues.len();
        let Some(control_queue) = self.control_queue.as_mut() else {
            return;
        };
        if let Err(error) = control_queue.event.read() {
            log::error!("failed to read virtio-net control queue event: {error:?}");
            return;
        }

        let mut completed = false;
        while let Some(head) = control_queue.queue.pop(&self.mem) {
            let mut used_len = 0;
            let status = match Reader::new_pair(&self.mem, head.clone()) {
                Ok((mut reader, mut writer)) => {
                    let requested_pairs = match (
                        reader.read_obj::<u8>(),
                        reader.read_obj::<u8>(),
                        reader.read_obj::<u16>(),
                    ) {
                        (Ok(class), Ok(command), Ok(pairs))
                            if u32::from(class) == VIRTIO_NET_CTRL_MQ
                                && u32::from(command) == VIRTIO_NET_CTRL_MQ_VQ_PAIRS_SET
                                && pairs > 0
                                && usize::from(pairs) <= max_queue_pairs =>
                        {
                            Some(usize::from(pairs))
                        }
                        _ => None,
                    };
                    let status = if let Some(requested_pairs) = requested_pairs {
                        self.active_queue_pairs = requested_pairs;
                        VIRTIO_NET_OK as u8
                    } else {
                        VIRTIO_NET_ERR as u8
                    };
                    if let Err(error) = writer.write_obj(status) {
                        log::error!("failed to write virtio-net control status: {error:?}");
                        VIRTIO_NET_ERR as u8
                    } else {
                        used_len = 1;
                        status
                    }
                }
                Err(error) => {
                    log::error!("invalid virtio-net control descriptor chain: {error:?}");
                    VIRTIO_NET_ERR as u8
                }
            };

            if let Err(error) = control_queue
                .queue
                .add_used(&self.mem, head.index, used_len)
            {
                log::error!("failed to complete virtio-net control request: {error:?}");
            } else {
                completed = true;
            }
            if status != VIRTIO_NET_OK as u8 {
                log::debug!("virtio-net rejected control request");
            }
        }
        if completed
            && control_queue
                .queue
                .needs_notification(&self.mem)
                .unwrap_or(false)
        {
            if let Err(error) = self.interrupt.try_signal_used_queue() {
                log::error!("failed to signal virtio-net control completion: {error:?}");
            }
        }
    }

    pub(crate) fn process_backend_socket_readable(&mut self) {
        for queue in self.rx_queues.iter_mut().take(self.active_queue_pairs) {
            if let Err(e) = queue.queue.enable_notification(&self.mem) {
                error!("error enabling queue notifications: {e:?}");
            }
        }
        if let Err(e) = self.process_rx() {
            log::error!("Failed to process rx: {e:?} (triggered by backend socket readable)");
        };
        for queue in self.rx_queues.iter_mut().take(self.active_queue_pairs) {
            if let Err(e) = queue.queue.disable_notification(&self.mem) {
                error!("error disabling queue notifications: {e:?}");
            }
        }
    }

    pub(crate) fn process_backend_socket_writeable(&mut self) {
        match self
            .backend
            .try_finish_write(vnet_hdr_len(), &self.tx_frame_buf[..self.tx_frame_len])
        {
            Ok(()) => {
                for queue_index in 0..self.active_queue_pairs {
                    self.process_tx_loop(queue_index);
                }
            }
            Err(WriteError::PartialWrite | WriteError::NothingWritten) => {}
            Err(e @ WriteError::Internal(_)) => {
                log::error!("Failed to finish write: {e:?}");
            }
            Err(e @ WriteError::ProcessNotRunning) => {
                log::debug!("Failed to finish write: {e:?}");
            }
        }
    }

    fn process_rx(&mut self) -> result::Result<(), RxError> {
        // if we have a deferred frame we try to process it first,
        // if that is not possible, we don't continue processing other frames
        if self.rx_has_deferred_frame {
            if self.write_frame_to_guest() {
                self.rx_has_deferred_frame = false;
            } else {
                return Ok(());
            }
        }

        let mut signal_queue = false;

        // Read as many frames as possible.
        let result = loop {
            match self.read_into_rx_frame_buf_from_backend() {
                Ok(()) => {
                    if self.write_frame_to_guest() {
                        signal_queue = true;
                    } else {
                        self.rx_has_deferred_frame = true;
                        break Ok(());
                    }
                }
                Err(ReadError::NothingRead) => break Ok(()),
                Err(e @ ReadError::Internal(_)) => break Err(RxError::Backend(e)),
            }
        };

        // At this point we processed as many Rx frames as possible.
        // We have to wake the guest if at least one descriptor chain has been used.
        if signal_queue {
            self.interrupt
                .try_signal_used_queue()
                .map_err(RxError::DeviceError)?;
        }

        result
    }

    fn process_tx_loop(&mut self, queue_index: usize) {
        if queue_index >= self.active_queue_pairs {
            return;
        }
        loop {
            self.tx_queues[queue_index]
                .queue
                .disable_notification(&self.mem)
                .unwrap();

            if let Err(e) = self.process_tx(queue_index) {
                log::error!("Failed to process rx: {e:?} (triggered by backend socket readable)");
            };

            if !self.tx_queues[queue_index]
                .queue
                .enable_notification(&self.mem)
                .unwrap()
            {
                break;
            }
        }
    }

    fn process_tx(&mut self, queue_index: usize) -> result::Result<(), TxError> {
        let tx_queue = &mut self.tx_queues[queue_index].queue;

        if self.backend.has_unfinished_write()
            && self
                .backend
                .try_finish_write(vnet_hdr_len(), &self.tx_frame_buf[..self.tx_frame_len])
                .is_err()
        {
            log::trace!("Cannot process tx because of unfinished partial write!");
            return Ok(());
        }

        let mut raise_irq = false;

        while let Some(head) = tx_queue.pop(&self.mem) {
            let head_index = head.index;
            let mut next_desc = Some(head);

            self.tx_iovec.clear();
            while let Some(desc) = next_desc {
                if desc.is_write_only() {
                    self.tx_iovec.clear();
                    break;
                }
                self.tx_iovec.push((desc.addr, desc.len as usize));
                next_desc = desc.next_descriptor();
            }

            // Copy buffer from across multiple descriptors.
            let mut read_count = 0;
            for (desc_addr, desc_len) in self.tx_iovec.drain(..) {
                let limit = cmp::min(read_count + desc_len, self.tx_frame_buf.len());

                let read_result = self
                    .mem
                    .read_slice(&mut self.tx_frame_buf[read_count..limit], desc_addr);
                match read_result {
                    Ok(()) => {
                        read_count += limit - read_count;
                    }
                    Err(e) => {
                        log::error!("Failed to read slice: {e:?}");
                        read_count = 0;
                        break;
                    }
                }
            }

            self.tx_frame_len = read_count;
            log::debug!("virtio-net tx descriptor: head={head_index}, bytes={read_count}");
            match self
                .backend
                .write_frame(vnet_hdr_len(), &mut self.tx_frame_buf[..read_count])
            {
                Ok(()) => {
                    self.tx_frame_len = 0;
                    tx_queue
                        .add_used(&self.mem, head_index, 0)
                        .map_err(TxError::QueueError)?;
                    raise_irq = true;
                }
                Err(WriteError::NothingWritten) => {
                    tx_queue.undo_pop();
                    break;
                }
                Err(WriteError::PartialWrite) => {
                    log::trace!("process_tx: partial write");
                    /*
                    This situation should be pretty rare, assuming reasonably sized socket buffers.
                    We have written only a part of a frame to the backend socket (the socket is full).

                    The frame we have read from the guest remains in tx_frame_buf, and will be sent
                    later.

                    Note that we cannot wait for the backend to process our sending frames, because
                    the backend could be blocked on sending a remainder of a frame to us - us waiting
                    for backend would cause a deadlock.
                     */
                    tx_queue
                        .add_used(&self.mem, head_index, 0)
                        .map_err(TxError::QueueError)?;
                    raise_irq = true;
                    break;
                }
                Err(e @ WriteError::Internal(_) | e @ WriteError::ProcessNotRunning) => {
                    return Err(TxError::Backend(e))
                }
            }
        }

        if raise_irq && tx_queue.needs_notification(&self.mem).unwrap() {
            self.interrupt
                .try_signal_used_queue()
                .map_err(TxError::DeviceError)?;
        }

        Ok(())
    }

    // Copies a single frame from `self.rx_frame_buf` into the guest.
    fn write_frame_to_guest_impl(
        &mut self,
        queue_index: usize,
    ) -> result::Result<(), FrontendError> {
        let mut result: std::result::Result<(), FrontendError> = Ok(());

        let queue = &mut self.rx_queues[queue_index].queue;
        let head_descriptor = queue.pop(&self.mem).ok_or(FrontendError::EmptyQueue)?;
        let head_index = head_descriptor.index;

        let mut frame_slice = &self.rx_frame_buf[..self.rx_frame_buf_len];

        let frame_len = frame_slice.len();
        let mut maybe_next_descriptor = Some(head_descriptor);
        while let Some(descriptor) = &maybe_next_descriptor {
            if frame_slice.is_empty() {
                break;
            }

            if !descriptor.is_write_only() {
                result = Err(FrontendError::ReadOnlyDescriptor);
                break;
            }

            let len = std::cmp::min(frame_slice.len(), descriptor.len as usize);
            match self.mem.write_slice(&frame_slice[..len], descriptor.addr) {
                Ok(()) => {
                    frame_slice = &frame_slice[len..];
                }
                Err(e) => {
                    log::error!("Failed to write slice: {e:?}");
                    result = Err(FrontendError::GuestMemory(e));
                    break;
                }
            };

            maybe_next_descriptor = descriptor.next_descriptor();
        }
        if result.is_ok() && !frame_slice.is_empty() {
            log::warn!("Receiving buffer is too small to hold frame of current size");
            result = Err(FrontendError::DescriptorChainTooSmall);
        }

        // Mark the descriptor chain as used. If an error occurred, skip the descriptor chain.
        let used_len = if result.is_err() { 0 } else { frame_len as u32 };
        queue
            .add_used(&self.mem, head_index, used_len)
            .map_err(FrontendError::QueueError)?;
        result
    }

    // Copies a single frame from `self.rx_frame_buf` into the guest. In case of an error retries
    // the operation if possible. Returns true if the operation was successfull.
    fn write_frame_to_guest(&mut self) -> bool {
        let ethernet_frame = &self.rx_frame_buf[vnet_hdr_len()..self.rx_frame_buf_len];
        let queue_index = flow_queue_index(ethernet_frame, self.active_queue_pairs);
        let max_iterations = self.rx_queues[queue_index].queue.actual_size();
        for _ in 0..max_iterations {
            match self.write_frame_to_guest_impl(queue_index) {
                Ok(()) => return true,
                Err(FrontendError::EmptyQueue) => break,
                Err(_) => continue,
            }
        }

        false
    }

    /// Fills self.rx_frame_buf with an ethernet frame from backend and prepends virtio_net_hdr to it
    fn read_into_rx_frame_buf_from_backend(&mut self) -> result::Result<(), ReadError> {
        self.rx_frame_buf_len = self.backend.read_frame(&mut self.rx_frame_buf)?;
        Ok(())
    }
}

fn is_rx_queue_event(source: u64, queue_count: usize) -> bool {
    source < RX_QUEUE_EVENT_BASE + queue_count as u64
}

fn is_tx_queue_event(source: u64, queue_count: usize) -> bool {
    source >= TX_QUEUE_EVENT_BASE && source < TX_QUEUE_EVENT_BASE + queue_count as u64
}

/// Choose a stable receive queue from immutable flow fields so packets in one flow never reorder.
fn flow_queue_index(frame: &[u8], queue_pairs: usize) -> usize {
    if queue_pairs <= 1 {
        return 0;
    }

    let mut hash = 0xcbf29ce484222325u64;
    let mut hash_bytes = |bytes: &[u8]| {
        for byte in bytes {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x100000001b3);
        }
    };
    if frame.len() < 14 {
        hash_bytes(frame);
        return hash as usize % queue_pairs;
    }

    hash_bytes(&frame[..12]);
    let mut network_start = 14usize;
    let mut ethertype = u16::from_be_bytes([frame[12], frame[13]]);
    while matches!(ethertype, 0x8100 | 0x88a8) && network_start + 4 <= frame.len() {
        hash_bytes(&frame[network_start..network_start + 2]);
        ethertype = u16::from_be_bytes([frame[network_start + 2], frame[network_start + 3]]);
        network_start += 4;
    }
    match ethertype {
        0x0800 if network_start + 20 <= frame.len() => {
            let protocol = frame[network_start + 9];
            hash_bytes(&frame[network_start + 12..network_start + 20]);
            hash_bytes(&[protocol]);
            let transport = network_start + usize::from(frame[network_start] & 0x0f) * 4;
            if matches!(protocol, 6 | 17) && transport + 4 <= frame.len() {
                hash_bytes(&frame[transport..transport + 4]);
            }
        }
        0x86dd if network_start + 40 <= frame.len() => {
            let protocol = frame[network_start + 6];
            hash_bytes(&frame[network_start + 8..network_start + 40]);
            hash_bytes(&[protocol]);
            let transport = network_start + 40;
            if matches!(protocol, 6 | 17) && transport + 4 <= frame.len() {
                hash_bytes(&frame[transport..transport + 4]);
            }
        }
        _ => hash_bytes(&ethertype.to_be_bytes()),
    }
    hash as usize % queue_pairs
}

#[cfg(unix)]
fn eventfd_pollable(event: &EventFd) -> Pollable {
    event.as_raw_fd()
}

#[cfg(windows)]
fn eventfd_pollable(event: &EventFd) -> Pollable {
    event.as_raw_handle()
}

#[cfg(unix)]
fn event_source_pollable(source: EventSource) -> io::Result<Pollable> {
    match source.raw() {
        RawEventSource::Fd(fd) => Ok(fd),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flow_queue_hash_ignores_tcp_payload_changes() {
        let mut first = ipv4_tcp_frame(1234, 443, &[1, 2, 3]);
        let mut second = ipv4_tcp_frame(1234, 443, &[9, 8, 7, 6]);

        assert_eq!(flow_queue_index(&first, 2), flow_queue_index(&second, 2));
        first[34 + 4..34 + 8].copy_from_slice(&100u32.to_be_bytes());
        second[34 + 4..34 + 8].copy_from_slice(&200u32.to_be_bytes());
        assert_eq!(flow_queue_index(&first, 2), flow_queue_index(&second, 2));
    }

    #[test]
    fn event_tokens_do_not_overlap_queue_classes() {
        assert!(is_rx_queue_event(0, 2));
        assert!(is_rx_queue_event(1, 2));
        assert!(!is_rx_queue_event(TX_QUEUE_EVENT_BASE, 2));
        assert!(is_tx_queue_event(TX_QUEUE_EVENT_BASE + 1, 2));
        assert!(!is_tx_queue_event(CONTROL_QUEUE_EVENT, 2));
    }

    fn ipv4_tcp_frame(source_port: u16, destination_port: u16, payload: &[u8]) -> Vec<u8> {
        let mut frame = vec![0u8; 14 + 20 + 20];
        frame.extend_from_slice(payload);
        frame[12..14].copy_from_slice(&0x0800u16.to_be_bytes());
        frame[14] = 0x45;
        frame[23] = 6;
        frame[26..30].copy_from_slice(&[192, 0, 2, 1]);
        frame[30..34].copy_from_slice(&[198, 51, 100, 2]);
        frame[34..36].copy_from_slice(&source_port.to_be_bytes());
        frame[36..38].copy_from_slice(&destination_port.to_be_bytes());
        frame
    }
}

#[cfg(windows)]
fn event_source_pollable(source: EventSource) -> io::Result<Pollable> {
    match source.raw() {
        RawEventSource::WaitableHandle(handle) => Ok(handle),
        RawEventSource::CompletionHandle(_) => Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "virtio-net does not support IOCP completion sources yet",
        )),
    }
}
