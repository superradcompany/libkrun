use std::collections::VecDeque;
use std::io;
use std::num::Wrapping;
use std::sync::{Arc, Mutex};

#[cfg(unix)]
use std::collections::HashMap;

use utils::epoll::{ControlOperation, Epoll, EpollEvent, EventSet};
use vm_memory::GuestMemoryMmap;

use super::super::Queue as VirtQueue;
use super::defs::{self, uapi};
use super::muxer::{push_packet, MuxerRx};
use super::muxer_rxq::MuxerRxQ;
use super::packet::VsockPacket;
#[cfg(unix)]
use super::packet::{TsiAcceptReq, TsiConnectReq, TsiListenReq, TsiSendtoAddr};
use super::proxy::{Proxy, ProxyRemoval, ProxyStatus, ProxyUpdate, RecvPkt};
use super::{VsockConnectState, VsockNotifier, VsockPollable, VsockShutdown, VsockStreamBackend};

//--------------------------------------------------------------------------------------------------
// Types
//--------------------------------------------------------------------------------------------------

/// Platform-neutral proxy around a custom stream backend.
pub struct CustomStreamProxy {
    id: u64,
    cid: u64,
    backend: Box<dyn VsockStreamBackend>,
    notifier: VsockNotifier,
    epoll: Arc<Epoll>,
    waiting_rx: bool,
    status: ProxyStatus,
    mem: GuestMemoryMmap,
    queue: Arc<Mutex<VirtQueue>>,
    rxq: Arc<Mutex<MuxerRxQ>>,
    peer_port: u32,
    local_port: u32,
    peer_fwd_cnt: Wrapping<u32>,
    peer_buf_alloc: u32,
    tx_cnt: Wrapping<u32>,
    last_tx_cnt_sent: Wrapping<u32>,
    rx_cnt: Wrapping<u32>,
    pending_write: VecDeque<u8>,
}

//--------------------------------------------------------------------------------------------------
// Methods
//--------------------------------------------------------------------------------------------------

impl CustomStreamProxy {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: u64,
        cid: u64,
        local_port: u32,
        peer_port: u32,
        backend: Box<dyn VsockStreamBackend>,
        notifier: VsockNotifier,
        epoll: Arc<Epoll>,
        mem: GuestMemoryMmap,
        queue: Arc<Mutex<VirtQueue>>,
        rxq: Arc<Mutex<MuxerRxQ>>,
    ) -> io::Result<Self> {
        let status = match backend.connect_state()? {
            VsockConnectState::Connecting => ProxyStatus::Connecting,
            VsockConnectState::Connected => ProxyStatus::Connected,
        };

        Ok(Self {
            id,
            cid,
            backend,
            notifier,
            epoll,
            waiting_rx: false,
            status,
            mem,
            queue,
            rxq,
            peer_port,
            local_port,
            peer_fwd_cnt: Wrapping(0),
            peer_buf_alloc: 0,
            tx_cnt: Wrapping(0),
            last_tx_cnt_sent: Wrapping(0),
            rx_cnt: Wrapping(0),
            pending_write: VecDeque::new(),
        })
    }

    pub fn is_connecting(&self) -> bool {
        self.status == ProxyStatus::Connecting
    }

    /// Record peer flow-control state while a route connection is pending.
    pub fn prepare_connect(&mut self, pkt: &VsockPacket) {
        self.peer_buf_alloc = pkt.buf_alloc();
        self.peer_fwd_cnt = Wrapping(pkt.fwd_cnt());
        self.local_port = pkt.dst_port();
        self.peer_port = pkt.src_port();
    }

    fn uses_notifier(&self) -> bool {
        self.backend.pollable().is_none()
    }

    fn event_pollable(&self) -> VsockPollable {
        self.backend
            .pollable()
            .unwrap_or_else(|| self.notifier.pollable())
    }

    fn connected_poll_events(&self) -> EventSet {
        if self.uses_notifier() || self.pending_write.is_empty() {
            EventSet::IN
        } else {
            EventSet::IN | EventSet::OUT
        }
    }

    fn connecting_poll_events(&self) -> EventSet {
        if self.uses_notifier() {
            EventSet::IN
        } else {
            EventSet::IN | EventSet::OUT
        }
    }

    fn clear_notification(&self) {
        if let Err(err) = self.notifier.clear() {
            warn!("failed to clear custom vsock notification: {err}");
        }
    }

    // Apply interest changes while the proxy lock still protects its state.
    // Returning a polling update for later application could overwrite a newer
    // pause/resume decision from the guest thread with stale socket interests.
    fn update_polling(&self) {
        let mut events = match self.status {
            ProxyStatus::Connecting => self.connecting_poll_events(),
            ProxyStatus::Connected => self.connected_poll_events(),
            _ => EventSet::empty(),
        };
        if self.waiting_rx && !self.uses_notifier() {
            events.remove(EventSet::IN);
        }
        self.poll(self.event_pollable(), events);

        // A socket's hang-up is level-triggered. Stop watching it when RX is
        // full, but retain the notifier so a guest buffer kick can resume reads.
        if !self.uses_notifier() {
            self.poll(
                self.notifier.pollable(),
                if self.waiting_rx && self.status == ProxyStatus::Connected {
                    EventSet::IN
                } else {
                    EventSet::empty()
                },
            );
        }
    }

    fn poll(&self, fd: VsockPollable, events: EventSet) {
        let _ = self
            .epoll
            .ctl(ControlOperation::Delete, fd, &EpollEvent::default());
        if !events.is_empty() {
            if let Err(err) =
                self.epoll
                    .ctl(ControlOperation::Add, fd, &EpollEvent::new(events, self.id))
            {
                warn!("failed to update custom vsock polling: {err}");
            }
        }
    }

    fn push_connect_response(&self) {
        push_packet(
            self.cid,
            MuxerRx::OpResponse {
                local_port: self.local_port,
                peer_port: self.peer_port,
            },
            &self.rxq,
            &self.queue,
            &self.mem,
        );
    }

    fn push_reset(&self) {
        push_packet(
            self.cid,
            MuxerRx::Reset {
                local_port: self.local_port,
                peer_port: self.peer_port,
            },
            &self.rxq,
            &self.queue,
            &self.mem,
        );
    }

    fn peer_avail_credit(&self) -> usize {
        (Wrapping(self.peer_buf_alloc) - (self.rx_cnt - self.peer_fwd_cnt)).0 as usize
    }

    fn recv_to_pkt(&self, pkt: &mut VsockPacket) -> io::Result<RecvPkt> {
        let Some(buf) = pkt.buf_mut() else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "missing vsock receive buffer",
            ));
        };
        let max_len = buf.len().min(self.peer_avail_credit());
        if max_len == 0 {
            return Ok(RecvPkt::WaitForCredit);
        }

        match self.backend.read(&mut buf[..max_len]) {
            Ok(0) => Ok(RecvPkt::Close),
            Ok(count) if count <= max_len => Ok(RecvPkt::Read(count)),
            Ok(count) => {
                warn!(
                    "vsock backend returned invalid read length: count={count}, capacity={max_len}"
                );
                Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "vsock backend read exceeded buffer",
                ))
            }
            Err(err)
                if matches!(
                    err.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                ) =>
            {
                Ok(RecvPkt::Error)
            }
            Err(err) => Err(err),
        }
    }

    fn recv_pkt(&mut self) -> io::Result<(bool, bool)> {
        let mut have_used = false;
        let mut wait_credit = false;
        let mut queue = self.queue.lock().unwrap();

        self.waiting_rx = false;
        loop {
            let Some(head) = queue.pop(&self.mem) else {
                self.waiting_rx = true;
                break;
            };
            let len = match VsockPacket::from_rx_virtq_head(&head) {
                Ok(mut pkt) => match self.recv_to_pkt(&mut pkt) {
                    Err(err) => {
                        queue.undo_pop();
                        return Err(err);
                    }
                    Ok(RecvPkt::WaitForCredit) => {
                        wait_credit = true;
                        0
                    }
                    Ok(RecvPkt::Read(count)) => {
                        self.rx_cnt += Wrapping(count as u32);
                        self.init_data_pkt(&mut pkt);
                        pkt.set_len(count as u32);
                        pkt.hdr().len() + count
                    }
                    Ok(RecvPkt::Close) => {
                        self.status = ProxyStatus::Closed;
                        0
                    }
                    Ok(RecvPkt::Error) => 0,
                },
                Err(err) => {
                    debug!("custom vsock RX queue error: {err:?}");
                    0
                }
            };

            if len == 0 {
                queue.undo_pop();
                break;
            }
            have_used = true;
            if let Err(err) = queue.add_used(&self.mem, head.index, len as u32) {
                error!("failed to add used elements to the queue: {err:?}");
            }
        }

        Ok((have_used, wait_credit))
    }

    fn flush_pending_write(&mut self) -> io::Result<usize> {
        let mut total_written = 0;
        while !self.pending_write.is_empty() {
            let (front, back) = self.pending_write.as_slices();
            let buf = if front.is_empty() { back } else { front };
            match self.backend.write(buf) {
                Ok(0) => return Err(io::Error::from(io::ErrorKind::WriteZero)),
                Ok(written) if written <= buf.len() => {
                    self.pending_write.drain(..written);
                    self.tx_cnt += Wrapping(written as u32);
                    total_written += written;
                }
                Ok(_) => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "vsock backend returned a write length larger than its input",
                    ));
                }
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => return Ok(total_written),
                Err(err) => return Err(err),
            }
        }
        Ok(total_written)
    }

    /// Return stream credit only after the host backend has consumed bytes
    /// from the bounded proxy queue.
    fn maybe_push_credit_update(&mut self, update: &mut ProxyUpdate) {
        if ((self.tx_cnt - self.last_tx_cnt_sent).0 as usize) < defs::CONN_TX_BUF_SIZE / 2 {
            return;
        }

        self.last_tx_cnt_sent = self.tx_cnt;
        push_packet(
            self.cid,
            MuxerRx::CreditUpdate {
                local_port: self.local_port,
                peer_port: self.peer_port,
                fwd_cnt: self.tx_cnt.0,
            },
            &self.rxq,
            &self.queue,
            &self.mem,
        );
        update.signal_queue = true;
    }

    fn init_data_pkt(&self, pkt: &mut VsockPacket) {
        pkt.set_op(uapi::VSOCK_OP_RW)
            .set_src_cid(uapi::VSOCK_HOST_CID)
            .set_dst_cid(self.cid)
            .set_src_port(self.local_port)
            .set_dst_port(self.peer_port)
            .set_type(uapi::VSOCK_TYPE_STREAM)
            .set_buf_alloc(defs::CONN_TX_BUF_SIZE as u32)
            .set_fwd_cnt(self.tx_cnt.0);
    }

    fn fail(&mut self, update: &mut ProxyUpdate, context: &str, err: &io::Error) {
        warn!("{context}: {err}");
        self.push_reset();
        self.status = ProxyStatus::Closed;
        update.signal_queue = true;
        update.remove_proxy = ProxyRemoval::Deferred;
        self.update_polling();
    }
}

//--------------------------------------------------------------------------------------------------
// Trait Implementations
//--------------------------------------------------------------------------------------------------

impl Proxy for CustomStreamProxy {
    #[cfg(unix)]
    fn id(&self) -> u64 {
        self.id
    }

    fn pollable(&self) -> VsockPollable {
        self.event_pollable()
    }

    fn status(&self) -> ProxyStatus {
        self.status
    }

    #[cfg(unix)]
    fn connect(&mut self, _pkt: &VsockPacket, _req: TsiConnectReq) -> ProxyUpdate {
        unreachable!("custom streams do not implement TSI connect")
    }

    fn confirm_connect(&mut self, pkt: &VsockPacket) -> Option<ProxyUpdate> {
        self.prepare_connect(pkt);
        // A duplicate request can arrive while an asynchronous host connect is
        // still pending. Do not acknowledge it until connect_state reports the
        // backend is actually ready.
        if self.status == ProxyStatus::Connected {
            self.push_connect_response();
        }
        None
    }

    #[cfg(unix)]
    fn getpeername(&mut self, _pkt: &VsockPacket) {
        unreachable!("custom streams do not implement TSI getpeername")
    }

    fn sendmsg(&mut self, pkt: &VsockPacket) -> ProxyUpdate {
        let mut update = ProxyUpdate::default();
        let Some(buf) = pkt.payload() else {
            let err = io::Error::new(
                io::ErrorKind::InvalidData,
                "vsock packet payload does not match its declared length",
            );
            self.fail(&mut update, "invalid custom vsock packet", &err);
            update.remove_proxy = ProxyRemoval::Immediate;
            return update;
        };

        // Flush previously accepted bytes before checking the fixed receive
        // window. This lets a backend that has become writable make room
        // without ever allocating beyond the advertised credit.
        if let Err(err) = self.flush_pending_write() {
            self.fail(&mut update, "custom vsock backend write failed", &err);
            return update;
        }
        if buf.len() > defs::CONN_TX_BUF_SIZE.saturating_sub(self.pending_write.len()) {
            let err = io::Error::new(
                io::ErrorKind::InvalidData,
                "guest exceeded the custom vsock stream receive window",
            );
            self.fail(&mut update, "invalid custom vsock stream credit", &err);
            update.remove_proxy = ProxyRemoval::Immediate;
            return update;
        }

        self.pending_write.extend(buf);
        if let Err(err) = self.flush_pending_write() {
            self.fail(&mut update, "custom vsock backend write failed", &err);
            return update;
        }
        self.update_polling();
        self.maybe_push_credit_update(&mut update);

        update
    }

    #[cfg(unix)]
    fn sendto_addr(&mut self, _req: TsiSendtoAddr) -> ProxyUpdate {
        unreachable!("custom streams do not implement TSI sendto")
    }

    #[cfg(unix)]
    fn listen(
        &mut self,
        _pkt: &VsockPacket,
        _req: TsiListenReq,
        _host_port_map: &Option<HashMap<u16, u16>>,
    ) -> ProxyUpdate {
        unreachable!("custom streams do not implement TSI listen")
    }

    #[cfg(unix)]
    fn accept(&mut self, _req: TsiAcceptReq) -> ProxyUpdate {
        unreachable!("custom streams do not implement TSI accept")
    }

    fn update_peer_credit(&mut self, pkt: &VsockPacket) -> ProxyUpdate {
        self.peer_buf_alloc = pkt.buf_alloc();
        self.peer_fwd_cnt = Wrapping(pkt.fwd_cnt());
        self.status = ProxyStatus::Connected;
        self.kick();

        self.update_polling();
        ProxyUpdate::default()
    }

    fn process_op_response(&mut self, pkt: &VsockPacket) -> ProxyUpdate {
        self.peer_buf_alloc = pkt.buf_alloc();
        self.peer_fwd_cnt = Wrapping(pkt.fwd_cnt());
        self.status = ProxyStatus::Connected;
        self.update_polling();
        ProxyUpdate::default()
    }

    fn shutdown(&mut self, pkt: &VsockPacket) {
        let recv_off = pkt.flags() & uapi::VSOCK_FLAGS_SHUTDOWN_RCV != 0;
        let send_off = pkt.flags() & uapi::VSOCK_FLAGS_SHUTDOWN_SEND != 0;
        let how = match (recv_off, send_off) {
            (true, true) => VsockShutdown::Both,
            (true, false) => VsockShutdown::Read,
            (false, _) => VsockShutdown::Write,
        };
        if let Err(err) = self.backend.shutdown(how) {
            warn!("error shutting down custom vsock backend: {err}");
        }
    }

    fn release(&mut self) -> ProxyUpdate {
        self.status = ProxyStatus::Closed;
        self.update_polling();
        ProxyUpdate {
            remove_proxy: ProxyRemoval::Immediate,
            ..Default::default()
        }
    }

    fn process_event(&mut self, evset: EventSet) -> ProxyUpdate {
        let mut update = ProxyUpdate::default();

        if self.status == ProxyStatus::Connecting {
            self.clear_notification();
            match self.backend.connect_state() {
                Ok(VsockConnectState::Connecting) => {
                    self.update_polling();
                    return update;
                }
                Ok(VsockConnectState::Connected) => {
                    self.status = ProxyStatus::Connected;
                    self.push_connect_response();
                    update.signal_queue = true;
                }
                Err(err) => {
                    self.fail(&mut update, "custom vsock backend connect failed", &err);
                    return update;
                }
            }
        } else if evset.contains(EventSet::IN) {
            self.clear_notification();
        }

        if self.status == ProxyStatus::Connected && !self.pending_write.is_empty() {
            if let Err(err) = self.flush_pending_write() {
                self.fail(&mut update, "custom vsock backend write failed", &err);
                return update;
            }
            self.maybe_push_credit_update(&mut update);
        }

        // A hang-up can arrive with unread host bytes (including macOS EV_EOF).
        // Drain through the normal credit/virtqueue path and close only after
        // read returns EOF. If the guest is blocked, keep the proxy for a retry.
        if self.status == ProxyStatus::Connected
            && evset.intersects(EventSet::IN | EventSet::HANG_UP | EventSet::READ_HANG_UP)
        {
            let (signal_queue, wait_credit) = match self.recv_pkt() {
                Ok(result) => result,
                Err(err) => {
                    self.fail(&mut update, "custom vsock backend read failed", &err);
                    return update;
                }
            };
            update.signal_queue |= signal_queue;
            if wait_credit {
                self.status = ProxyStatus::WaitingCreditUpdate;
                update.push_credit_req = Some(MuxerRx::CreditRequest {
                    local_port: self.local_port,
                    peer_port: self.peer_port,
                    fwd_cnt: self.tx_cnt.0,
                });
            }

            if self.status == ProxyStatus::Closed {
                self.push_reset();
                update.signal_queue = true;
                self.update_polling();
                update.remove_proxy = ProxyRemoval::Immediate;
                return update;
            }
        }

        self.update_polling();
        update
    }

    fn kick(&self) {
        if let Err(err) = self.notifier.notify() {
            warn!("failed to kick custom vsock backend: {err}");
        }
    }
}

//--------------------------------------------------------------------------------------------------
// Tests
//--------------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};

    use vm_memory::{Bytes, GuestAddress};

    use super::super::packet::VSOCK_PKT_HDR_SIZE;
    use super::*;
    use crate::virtio::{Descriptor, DescriptorChain};

    struct TestStreamState {
        blocked: AtomicBool,
        written: Mutex<Vec<u8>>,
    }

    struct TestStream {
        state: Arc<TestStreamState>,
    }

    impl VsockStreamBackend for TestStream {
        fn read(&self, _buf: &mut [u8]) -> io::Result<usize> {
            Err(io::Error::from(io::ErrorKind::WouldBlock))
        }

        fn write(&self, buf: &[u8]) -> io::Result<usize> {
            let mut written = self.state.written.lock().unwrap();
            if self.state.blocked.load(Ordering::Relaxed) && written.len() >= 2 {
                return Err(io::Error::from(io::ErrorKind::WouldBlock));
            }
            let count = if self.state.blocked.load(Ordering::Relaxed) {
                buf.len().min(2)
            } else {
                buf.len()
            };
            written.extend_from_slice(&buf[..count]);
            Ok(count)
        }

        fn shutdown(&self, _how: VsockShutdown) -> io::Result<()> {
            Ok(())
        }
    }

    fn tx_packet(mem: &GuestMemoryMmap, declared_len: u32, descriptor: &[u8]) -> VsockPacket {
        const DESC_TABLE: u64 = 0x1000;
        const HEADER: u64 = 0x2000;
        const PAYLOAD: u64 = 0x3000;

        mem.write_obj(
            Descriptor {
                addr: HEADER,
                len: VSOCK_PKT_HDR_SIZE as u32,
                flags: 1,
                next: 1,
            },
            GuestAddress(DESC_TABLE),
        )
        .unwrap();
        mem.write_obj(
            Descriptor {
                addr: PAYLOAD,
                len: descriptor.len() as u32,
                flags: 0,
                next: 0,
            },
            GuestAddress(DESC_TABLE + 16),
        )
        .unwrap();

        let mut header = [0u8; VSOCK_PKT_HDR_SIZE];
        header[24..28].copy_from_slice(&declared_len.to_le_bytes());
        mem.write_slice(&header, GuestAddress(HEADER)).unwrap();
        mem.write_slice(descriptor, GuestAddress(PAYLOAD)).unwrap();

        let head = DescriptorChain::checked_new(mem, GuestAddress(DESC_TABLE), 2, 0).unwrap();
        VsockPacket::from_tx_virtq_head(&head).unwrap()
    }

    fn test_proxy(state: Arc<TestStreamState>, mem: GuestMemoryMmap) -> CustomStreamProxy {
        CustomStreamProxy::new(
            1,
            3,
            5000,
            4000,
            Box::new(TestStream { state }),
            VsockNotifier::new().unwrap(),
            Arc::new(Epoll::new().unwrap()),
            mem,
            Arc::new(Mutex::new(VirtQueue::new(256))),
            Arc::new(Mutex::new(MuxerRxQ::new())),
        )
        .unwrap()
    }

    #[test]
    fn buffers_partial_writes_without_losing_bytes() {
        let state = Arc::new(TestStreamState {
            blocked: AtomicBool::new(true),
            written: Mutex::new(Vec::new()),
        });
        let mem = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x10000)]).unwrap();
        let mut proxy = test_proxy(Arc::clone(&state), mem);

        proxy.pending_write.extend(b"hello");
        proxy.flush_pending_write().unwrap();
        assert_eq!(&*state.written.lock().unwrap(), b"he");
        assert_eq!(proxy.pending_write.len(), 3);
        assert_eq!(proxy.tx_cnt.0, 2);

        state.blocked.store(false, Ordering::Relaxed);
        proxy.flush_pending_write().unwrap();
        assert_eq!(&*state.written.lock().unwrap(), b"hello");
        assert!(proxy.pending_write.is_empty());
        assert_eq!(proxy.tx_cnt.0, 5);
    }

    #[test]
    fn forwards_only_the_declared_packet_payload() {
        let state = Arc::new(TestStreamState {
            blocked: AtomicBool::new(false),
            written: Mutex::new(Vec::new()),
        });
        let mem = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x10000)]).unwrap();
        let pkt = tx_packet(&mem, 1, b"a-secret-tail");
        let mut proxy = test_proxy(Arc::clone(&state), mem.clone());

        let update = proxy.sendmsg(&pkt);

        assert!(matches!(update.remove_proxy, ProxyRemoval::Keep));
        assert_eq!(&*state.written.lock().unwrap(), b"a");
        assert_eq!(proxy.tx_cnt.0, 1);
    }

    #[test]
    fn rejects_writes_beyond_the_bounded_receive_window() {
        let state = Arc::new(TestStreamState {
            blocked: AtomicBool::new(true),
            // The test backend returns WouldBlock after accepting two bytes.
            written: Mutex::new(vec![0, 0]),
        });
        let mem = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x10000)]).unwrap();
        let pkt = tx_packet(&mem, 1, b"x");
        let mut proxy = test_proxy(state, mem.clone());
        proxy.pending_write.resize(defs::CONN_TX_BUF_SIZE, 0);

        let update = proxy.sendmsg(&pkt);

        assert!(matches!(update.remove_proxy, ProxyRemoval::Immediate));
        assert_eq!(proxy.pending_write.len(), defs::CONN_TX_BUF_SIZE);
    }

    /// Exercise the proxy-to-guest boundary with unread bytes in a real socket.
    /// Neither a hang-up nor temporary guest backpressure may discard the tail.
    #[cfg(unix)]
    #[test]
    fn drains_hung_up_stream_before_reset() {
        use std::io::{Read, Write};
        use std::net::Shutdown;
        use std::os::fd::{AsRawFd, RawFd};
        use std::os::unix::net::UnixStream;

        struct SocketBackend(UnixStream);

        impl VsockStreamBackend for SocketBackend {
            fn read(&self, buf: &mut [u8]) -> io::Result<usize> {
                (&self.0).read(buf)
            }

            fn write(&self, buf: &[u8]) -> io::Result<usize> {
                (&self.0).write(buf)
            }

            fn shutdown(&self, how: VsockShutdown) -> io::Result<()> {
                self.0.shutdown(match how {
                    VsockShutdown::Read => Shutdown::Read,
                    VsockShutdown::Write => Shutdown::Write,
                    VsockShutdown::Both => Shutdown::Both,
                })
            }

            fn pollable(&self) -> Option<RawFd> {
                Some(self.0.as_raw_fd())
            }
        }

        const DESC: u64 = 0x1000;
        const AVAIL: u64 = 0x2000;
        const USED: u64 = 0x3000;
        const DATA: u64 = 0x4000;
        const STRIDE: u64 = 0x2000;
        const CHUNK: usize = 4096;

        for half_close in [false, true] {
            for event in [EventSet::HANG_UP, EventSet::IN | EventSet::HANG_UP] {
                for pause in ["none", "credit", "descriptors"] {
                    let payload: Vec<u8> = (0..CHUNK * 2).map(|i| (i % 251) as u8).collect();
                    let (mut host, backend) = UnixStream::pair().unwrap();
                    backend.set_nonblocking(true).unwrap();
                    host.write_all(&payload).unwrap();
                    let _host = if half_close {
                        host.shutdown(Shutdown::Write).unwrap();
                        Some(host)
                    } else {
                        drop(host);
                        None
                    };

                    let mem = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x10000)]).unwrap();
                    let mut queue = VirtQueue::new(8);
                    queue.size = 8;
                    queue.ready = true;
                    queue.desc_table = GuestAddress(DESC);
                    queue.avail_ring = GuestAddress(AVAIL);
                    queue.used_ring = GuestAddress(USED);
                    // Two data buffers and one buffer for the terminal reset.
                    for i in 0..3u16 {
                        mem.write_obj(
                            Descriptor {
                                addr: DATA + u64::from(i) * STRIDE,
                                len: (VSOCK_PKT_HDR_SIZE + CHUNK) as u32,
                                flags: 2,
                                next: 0,
                            },
                            GuestAddress(DESC + u64::from(i) * 16),
                        )
                        .unwrap();
                        mem.write_obj(i, GuestAddress(AVAIL + 4 + u64::from(i) * 2))
                            .unwrap();
                    }
                    mem.write_obj(
                        if pause == "descriptors" { 1u16 } else { 3u16 },
                        GuestAddress(AVAIL + 2),
                    )
                    .unwrap();
                    let epoll = Arc::new(Epoll::new().unwrap());
                    let mut proxy = CustomStreamProxy::new(
                        1,
                        3,
                        5000,
                        4000,
                        Box::new(SocketBackend(backend)),
                        VsockNotifier::new().unwrap(),
                        epoll.clone(),
                        mem.clone(),
                        Arc::new(Mutex::new(queue)),
                        Arc::new(Mutex::new(MuxerRxQ::new())),
                    )
                    .unwrap();
                    proxy.peer_buf_alloc = if pause == "credit" {
                        CHUNK as u32
                    } else {
                        (CHUNK * 4) as u32
                    };

                    let mut update = proxy.process_event(event);
                    if pause != "none" {
                        assert!(
                            matches!(update.remove_proxy, ProxyRemoval::Keep),
                            "closed before draining: {pause}"
                        );
                        assert_eq!(mem.read_obj::<u16>(GuestAddress(USED + 2)).unwrap(), 1);
                        if pause == "credit" {
                            assert!(matches!(
                                update.push_credit_req,
                                Some(MuxerRx::CreditRequest { .. })
                            ));
                            let credit_mem =
                                GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x10000)])
                                    .unwrap();
                            let mut credit = tx_packet(&credit_mem, 0, &[]);
                            credit.set_buf_alloc(CHUNK as u32).set_fwd_cnt(CHUNK as u32);
                            proxy.update_peer_credit(&credit);
                        } else {
                            let mut ready = vec![EpollEvent::new(EventSet::empty(), 0); 2];
                            assert_eq!(
                                epoll.wait(2, 0, &mut ready).unwrap(),
                                0,
                                "socket must not remain ready while guest RX buffers are exhausted"
                            );
                            // A kick for another connection must not leave this
                            // socket spinning if its RX queue is still empty.
                            proxy.kick();
                            assert_eq!(epoll.wait(2, 1000, &mut ready).unwrap(), 1);
                            proxy.process_event(ready[0].event_set());
                            assert_eq!(epoll.wait(2, 0, &mut ready).unwrap(), 0);

                            mem.write_obj(3u16, GuestAddress(AVAIL + 2)).unwrap();
                            proxy.kick();
                            assert_eq!(
                                epoll.wait(2, 1000, &mut ready).unwrap(),
                                1,
                                "new guest RX buffers must wake the paused proxy"
                            );
                        }
                        update = proxy.process_event(EventSet::IN);
                        // At an exact credit boundary the EOF read needs fresh credit too.
                        if pause == "credit" {
                            let credit_mem =
                                GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x10000)])
                                    .unwrap();
                            let mut credit = tx_packet(&credit_mem, 0, &[]);
                            credit
                                .set_buf_alloc(CHUNK as u32)
                                .set_fwd_cnt((CHUNK * 2) as u32);
                            proxy.update_peer_credit(&credit);
                            update = proxy.process_event(event);
                        }
                    }

                    assert_eq!(
                        mem.read_obj::<u16>(GuestAddress(USED + 2)).unwrap(),
                        3,
                        "expected two data packets followed by reset"
                    );
                    assert!(matches!(update.remove_proxy, ProxyRemoval::Immediate));
                    assert!(update.signal_queue);
                    let mut received = Vec::new();
                    for i in 0..3u16 {
                        let head =
                            DescriptorChain::checked_new(&mem, GuestAddress(DESC), 8, i).unwrap();
                        let pkt = VsockPacket::from_rx_virtq_head(&head).unwrap();
                        if i < 2 {
                            assert_eq!(pkt.op(), uapi::VSOCK_OP_RW);
                            received.extend_from_slice(pkt.payload().unwrap());
                        } else {
                            assert_eq!(pkt.op(), uapi::VSOCK_OP_RST);
                        }
                    }
                    assert_eq!(received, payload);
                    proxy.kick();
                    let mut ready = vec![EpollEvent::new(EventSet::empty(), 0); 2];
                    assert_eq!(
                        epoll.wait(2, 0, &mut ready).unwrap(),
                        0,
                        "closed proxies must unregister the socket and notifier"
                    );
                }
            }
        }
    }
}
