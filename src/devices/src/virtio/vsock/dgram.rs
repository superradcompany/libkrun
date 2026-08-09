use std::collections::HashMap;
use std::io;
use std::os::fd::{AsRawFd, RawFd};
use std::sync::{Arc, Mutex};

use vm_memory::GuestMemoryMmap;

use super::super::Queue as VirtQueue;
use super::backend::{VsockDatagramBackend, VsockNotifier};
use super::defs;
use super::muxer::{push_packet, MuxerRx};
use super::muxer_rxq::MuxerRxQ;
use super::packet::{TsiAcceptReq, TsiConnectReq, TsiListenReq, TsiSendtoAddr, VsockPacket};
use super::proxy::{Proxy, ProxyRemoval, ProxyStatus, ProxyUpdate};
use utils::epoll::EventSet;

//--------------------------------------------------------------------------------------------------
// Constants
//--------------------------------------------------------------------------------------------------

/// Bound the work performed for one readiness event so a busy datagram service
/// cannot monopolize the shared vsock muxer thread.
const MAX_RECEIVE_BATCH: usize = 32;

//--------------------------------------------------------------------------------------------------
// Types
//--------------------------------------------------------------------------------------------------

/// One host endpoint associated with a guest datagram source port.
pub struct DatagramProxy {
    id: u64,
    cid: u64,
    local_port: u32,
    peer_port: u32,
    backend: Box<dyn VsockDatagramBackend>,
    notifier: VsockNotifier,
    mem: GuestMemoryMmap,
    queue: Arc<Mutex<VirtQueue>>,
    rxq: Arc<Mutex<MuxerRxQ>>,
}

//--------------------------------------------------------------------------------------------------
// Methods
//--------------------------------------------------------------------------------------------------

impl DatagramProxy {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: u64,
        cid: u64,
        local_port: u32,
        peer_port: u32,
        backend: Box<dyn VsockDatagramBackend>,
        notifier: VsockNotifier,
        mem: GuestMemoryMmap,
        queue: Arc<Mutex<VirtQueue>>,
        rxq: Arc<Mutex<MuxerRxQ>>,
    ) -> Self {
        Self {
            id,
            cid,
            local_port,
            peer_port,
            backend,
            notifier,
            mem,
            queue,
            rxq,
        }
    }

    fn uses_notifier(&self) -> bool {
        self.backend.pollable().is_none()
    }

    fn receive_batch(&self) -> io::Result<bool> {
        if self.uses_notifier() {
            self.notifier.clear()?;
        }

        let mut delivered = false;
        for _ in 0..MAX_RECEIVE_BATCH {
            let mut data = vec![0; defs::MAX_PKT_BUF_SIZE];
            let read = match self.backend.receive(&mut data) {
                Ok(read) => read,
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => break,
                Err(err) => return Err(err),
            };

            if read.len > data.len() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "vsock datagram backend returned a length larger than its buffer",
                ));
            }
            if read.truncated {
                warn!(
                    "dropping oversized host datagram for vsock port {}",
                    self.local_port
                );
                continue;
            }

            data.truncate(read.len);
            push_packet(
                self.cid,
                MuxerRx::Datagram {
                    local_port: self.local_port,
                    peer_port: self.peer_port,
                    data,
                },
                &self.rxq,
                &self.queue,
                &self.mem,
            );
            delivered = true;
        }

        Ok(delivered)
    }
}

//--------------------------------------------------------------------------------------------------
// Trait Implementations
//--------------------------------------------------------------------------------------------------

impl AsRawFd for DatagramProxy {
    fn as_raw_fd(&self) -> RawFd {
        self.backend
            .pollable()
            .unwrap_or_else(|| self.notifier.event().as_raw_fd())
    }
}

impl Proxy for DatagramProxy {
    fn id(&self) -> u64 {
        self.id
    }

    fn pollable(&self) -> RawFd {
        self.as_raw_fd()
    }

    fn status(&self) -> ProxyStatus {
        ProxyStatus::Connected
    }

    fn connect(&mut self, _pkt: &VsockPacket, _req: TsiConnectReq) -> ProxyUpdate {
        ProxyUpdate::default()
    }

    fn getpeername(&mut self, _pkt: &VsockPacket) {}

    fn sendmsg(&mut self, pkt: &VsockPacket) -> ProxyUpdate {
        let payload = pkt.buf().unwrap_or_default();
        match self.backend.send(payload) {
            Ok(()) => ProxyUpdate::default(),
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                debug!(
                    "dropping guest datagram for busy host port {}",
                    self.local_port
                );
                ProxyUpdate::default()
            }
            Err(err) => {
                warn!(
                    "vsock datagram backend failed for host port {}: {err}",
                    self.local_port
                );
                ProxyUpdate {
                    polling: Some((self.id, self.as_raw_fd(), EventSet::empty())),
                    remove_proxy: ProxyRemoval::Immediate,
                    ..Default::default()
                }
            }
        }
    }

    fn sendto_addr(&mut self, _req: TsiSendtoAddr) -> ProxyUpdate {
        ProxyUpdate::default()
    }

    fn listen(
        &mut self,
        _pkt: &VsockPacket,
        _req: TsiListenReq,
        _host_port_map: &Option<HashMap<u16, u16>>,
    ) -> ProxyUpdate {
        ProxyUpdate::default()
    }

    fn accept(&mut self, _req: TsiAcceptReq) -> ProxyUpdate {
        ProxyUpdate::default()
    }

    fn update_peer_credit(&mut self, _pkt: &VsockPacket) -> ProxyUpdate {
        // Datagram packets deliberately do not participate in stream credit accounting.
        ProxyUpdate::default()
    }

    fn process_op_response(&mut self, _pkt: &VsockPacket) -> ProxyUpdate {
        ProxyUpdate::default()
    }

    fn release(&mut self) -> ProxyUpdate {
        ProxyUpdate {
            polling: Some((self.id, self.as_raw_fd(), EventSet::empty())),
            remove_proxy: ProxyRemoval::Immediate,
            ..Default::default()
        }
    }

    fn process_event(&mut self, evset: EventSet) -> ProxyUpdate {
        if evset.contains(EventSet::HANG_UP) {
            return self.release();
        }

        if !evset.contains(EventSet::IN) {
            return ProxyUpdate::default();
        }

        match self.receive_batch() {
            Ok(delivered) => ProxyUpdate {
                signal_queue: delivered,
                polling: Some((self.id, self.as_raw_fd(), EventSet::IN)),
                ..Default::default()
            },
            Err(err) => {
                warn!(
                    "failed to receive host datagram for vsock port {}: {err}",
                    self.local_port
                );
                self.release()
            }
        }
    }

    fn kick(&self) {
        if self.uses_notifier() {
            if let Err(err) = self.notifier.notify() {
                warn!("failed to kick custom vsock datagram backend: {err}");
            }
        }
    }
}
