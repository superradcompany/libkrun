// Copyright 2018 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0
//

/// `MuxerRxQ` implements a helper object that `VsockMuxer` can use for queuing RX (host -> guest)
/// packets (or rather instructions on how to build said packets).
///
/// Under ideal operation, every connection, that has pending RX data, will be present in the muxer
/// RX queue. However, since the RX queue is smaller than the connection pool, it may, under some
/// conditions, become full, meaning that it can no longer account for all the connections that can
/// yield RX data.  When that happens, we say that it is no longer "synchronized" (i.e. with the
/// connection pool).  A desynchronized RX queue still holds valid data, and the muxer will
/// continue to pop packets from it. However, when a desynchronized queue is drained, additional
/// data may still be available, so the muxer will have to perform a more costly walk of the entire
/// connection pool to find it.  This walk is performed here, as part of building an RX queue from
/// the connection pool. When an out-of-sync is drained, the muxer will discard it, and attempt to
/// rebuild a synced one.
use std::collections::VecDeque;

use super::defs;
use super::defs::uapi;
use super::muxer::MuxerRx;
use super::packet::VsockPacket;
#[cfg(unix)]
use super::packet::{TsiAcceptRsp, TsiConnectRsp, TsiListenRsp};

/// The muxer RX queue.
pub struct MuxerRxQ {
    /// The RX queue data.
    q: VecDeque<MuxerRx>,
    /// The RX queue sync status.
    synced: bool,
}

impl MuxerRxQ {
    const SIZE: usize = defs::MUXER_RXQ_SIZE;

    /// Trivial RX queue constructor.
    pub fn new() -> Self {
        Self {
            q: VecDeque::with_capacity(Self::SIZE),
            synced: true,
        }
    }

    /// Push a new RX item to the queue.
    ///
    /// A push will fail when:
    /// - trying to push a connection key onto an out-of-sync, or full queue; or
    /// - trying to push an RST onto a queue already full of RSTs.
    ///
    /// RSTs take precedence over connections, because connections can always be queried for
    /// pending RX data later. Aside from this queue, there is no other storage for RSTs, so
    /// failing to push one means that we have to drop the packet.
    ///
    /// Returns:
    /// - `true` if the new item has been successfully queued; or
    /// - `false` if there was no room left in the queue.
    pub fn push(&mut self, rx: MuxerRx) -> bool {
        // Pushing to a non-full, synchronized queue will always succeed.
        if self.is_synced() && !self.is_full() {
            self.q.push_back(rx);
            return true;
        }

        false
    }

    /// Queue retryable custom-stream credit without crowding out other controls.
    /// The caller must retain and retry progress when this returns false.
    pub fn push_credit_update(&mut self, local_port: u32, peer_port: u32, fwd_cnt: u32) -> bool {
        let position = self.q.iter().position(|pending| {
            matches!(pending,
            MuxerRx::CreditUpdate { local_port: local, peer_port: peer, .. }
                if *local == local_port && *peer == peer_port)
        });
        if let Some(position) = position {
            // Move the newest count to the tail: a later same-stream credit
            // request may still carry an older count. Updating in place would
            // make the guest observe progress backwards. Counts also wrap, so
            // max(old, new) is not a valid way to choose the latest value.
            self.q.remove(position);
            self.q.push_back(MuxerRx::CreditUpdate {
                local_port,
                peer_port,
                fwd_cnt,
            });
            return true;
        }
        let credit_count = self
            .q
            .iter()
            .filter(|pending| matches!(pending, MuxerRx::CreditUpdate { .. }))
            .count();
        // Reserve half the queue for replies, resets, and credit requests.
        // Apply this only to callers that retain rejected credit: legacy
        // producers using push() do not implement this retry contract.
        if credit_count >= Self::SIZE / 2 {
            return false;
        }
        self.push(MuxerRx::CreditUpdate {
            local_port,
            peer_port,
            fwd_cnt,
        })
    }

    /// Pop an RX item from the front of the queue.
    pub fn pop(&mut self) -> Option<MuxerRx> {
        self.q.pop_front()
    }

    /// Check if the RX queue is synchronized with the connection pool.
    pub fn is_synced(&self) -> bool {
        self.synced
    }

    /// Get the total number of items in the queue.
    pub fn len(&self) -> usize {
        self.q.len()
    }

    /// Check if the queue is empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Check if the queue is full.
    pub fn is_full(&self) -> bool {
        self.len() == Self::SIZE
    }

    /// Drops connection-local responses after the owning muxer has stopped.
    pub(crate) fn clear(&mut self) {
        self.q.clear();
        self.synced = true;
    }
}

pub fn rx_to_pkt(cid: u64, rx: MuxerRx, pkt: &mut VsockPacket) -> bool {
    match rx {
        MuxerRx::Reset {
            local_port,
            peer_port,
        } => {
            pkt.set_op(uapi::VSOCK_OP_RST)
                .set_src_cid(uapi::VSOCK_HOST_CID)
                .set_dst_cid(cid)
                .set_src_port(local_port)
                .set_dst_port(peer_port)
                .set_len(0)
                .set_type(uapi::VSOCK_TYPE_STREAM)
                .set_flags(0)
                .set_buf_alloc(0)
                .set_fwd_cnt(0);
        }
        #[cfg(unix)]
        MuxerRx::ConnResponse {
            local_port,
            peer_port,
            result,
        } => {
            pkt.set_op(uapi::VSOCK_OP_RW)
                .set_src_cid(uapi::VSOCK_HOST_CID)
                .set_dst_cid(cid)
                .set_src_port(local_port)
                .set_dst_port(peer_port)
                .set_type(uapi::VSOCK_TYPE_DGRAM);

            pkt.write_connect_rsp(TsiConnectRsp { result });
            pkt.set_len(pkt.buf().unwrap().len() as u32);
        }
        MuxerRx::OpRequest {
            local_port,
            peer_port,
        } => {
            pkt.set_op(uapi::VSOCK_OP_REQUEST)
                .set_src_cid(uapi::VSOCK_HOST_CID)
                .set_dst_cid(cid)
                .set_src_port(local_port)
                .set_dst_port(peer_port)
                .set_type(uapi::VSOCK_TYPE_STREAM)
                .set_buf_alloc(defs::CONN_TX_BUF_SIZE as u32);

            pkt.set_len(0);
        }
        MuxerRx::OpResponse {
            local_port,
            peer_port,
        } => {
            pkt.set_op(uapi::VSOCK_OP_RESPONSE)
                .set_src_cid(uapi::VSOCK_HOST_CID)
                .set_dst_cid(cid)
                .set_src_port(local_port)
                .set_dst_port(peer_port)
                .set_type(uapi::VSOCK_TYPE_STREAM)
                .set_buf_alloc(defs::CONN_TX_BUF_SIZE as u32);

            pkt.set_len(0);
        }
        #[cfg(unix)]
        MuxerRx::GetnameResponse {
            local_port,
            peer_port,
            data,
        } => {
            pkt.set_op(uapi::VSOCK_OP_RW)
                .set_src_cid(uapi::VSOCK_HOST_CID)
                .set_dst_cid(cid)
                .set_src_port(local_port)
                .set_dst_port(peer_port)
                .set_type(uapi::VSOCK_TYPE_DGRAM);

            pkt.write_getname_rsp(data);
            pkt.set_len(pkt.buf().unwrap().len() as u32);
        }
        MuxerRx::CreditRequest {
            local_port,
            peer_port,
            fwd_cnt,
        } => {
            pkt.set_op(uapi::VSOCK_OP_CREDIT_REQUEST)
                .set_len(0)
                .set_flags(0)
                .set_src_cid(uapi::VSOCK_HOST_CID)
                .set_dst_cid(cid)
                .set_src_port(local_port)
                .set_dst_port(peer_port)
                .set_type(uapi::VSOCK_TYPE_STREAM)
                .set_buf_alloc(defs::CONN_TX_BUF_SIZE as u32)
                .set_fwd_cnt(fwd_cnt);
        }
        MuxerRx::CreditUpdate {
            local_port,
            peer_port,
            fwd_cnt,
        } => {
            pkt.set_op(uapi::VSOCK_OP_CREDIT_UPDATE)
                .set_len(0)
                .set_flags(0)
                .set_src_cid(uapi::VSOCK_HOST_CID)
                .set_dst_cid(cid)
                .set_src_port(local_port)
                .set_dst_port(peer_port)
                .set_type(uapi::VSOCK_TYPE_STREAM)
                .set_buf_alloc(defs::CONN_TX_BUF_SIZE as u32)
                .set_fwd_cnt(fwd_cnt);
        }
        #[cfg(unix)]
        MuxerRx::ListenResponse {
            local_port,
            peer_port,
            result,
        } => {
            pkt.set_op(uapi::VSOCK_OP_RW)
                .set_src_cid(uapi::VSOCK_HOST_CID)
                .set_dst_cid(cid)
                .set_src_port(local_port)
                .set_dst_port(peer_port)
                .set_type(uapi::VSOCK_TYPE_DGRAM);

            pkt.write_listen_rsp(TsiListenRsp { result });
            pkt.set_len(pkt.buf().unwrap().len() as u32);
        }
        #[cfg(unix)]
        MuxerRx::AcceptResponse {
            local_port,
            peer_port,
            result,
        } => {
            pkt.set_op(uapi::VSOCK_OP_RW)
                .set_src_cid(uapi::VSOCK_HOST_CID)
                .set_dst_cid(cid)
                .set_src_port(local_port)
                .set_dst_port(peer_port)
                .set_type(uapi::VSOCK_TYPE_DGRAM);

            pkt.write_accept_rsp(TsiAcceptRsp { result });
            pkt.set_len(pkt.buf().unwrap().len() as u32);
        }
        #[cfg(unix)]
        MuxerRx::Datagram {
            local_port,
            peer_port,
            data,
        } => {
            let Some(capacity) = pkt.buf().map(|buf| buf.len()) else {
                return false;
            };
            if data.len() > capacity {
                // Virtio-vsock datagrams are atomic. A short guest buffer must
                // drop the message rather than manufacture a truncated one.
                return false;
            }

            pkt.set_op(uapi::VSOCK_OP_RW)
                .set_src_cid(uapi::VSOCK_HOST_CID)
                .set_dst_cid(cid)
                .set_src_port(local_port)
                .set_dst_port(peer_port)
                .set_type(uapi::VSOCK_TYPE_DGRAM)
                .set_flags(0)
                .set_buf_alloc(0)
                .set_fwd_cnt(0);

            let buf = pkt.buf_mut().expect("datagram buffer was validated above");
            buf[..data.len()].copy_from_slice(&data);
            pkt.set_len(data.len() as u32);
        }
    }

    true
}

//--------------------------------------------------------------------------------------------------
// Tests
//--------------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::virtio::{Descriptor, DescriptorChain};
    use vm_memory::{Bytes, GuestAddress, GuestMemoryMmap};

    #[test]
    fn credit_budget_reserves_control_capacity_and_full_queue_still_coalesces() {
        let mut queue = MuxerRxQ::new();
        for port in 0..MuxerRxQ::SIZE / 2 {
            assert!(queue.push_credit_update(port as u32, 1, u32::MAX));
        }
        assert!(
            !queue.push_credit_update(0, 2, 1),
            "both ports identify a stream"
        );
        for port in 0..MuxerRxQ::SIZE / 2 {
            assert!(queue.push(MuxerRx::Reset {
                local_port: port as u32,
                peer_port: 2
            }));
        }
        assert!(queue.is_full());
        assert!(
            queue.push_credit_update(0, 1, 0),
            "coalescing needs no extra slot, even after wrap"
        );
        assert_eq!(queue.len(), MuxerRxQ::SIZE);
        let mut last = None;
        while let Some(packet) = queue.pop() {
            last = Some(packet);
        }
        assert!(matches!(
            last,
            Some(MuxerRx::CreditUpdate {
                local_port: 0,
                peer_port: 1,
                fwd_cnt: 0
            })
        ));
    }

    #[test]
    fn coalesced_credit_follows_older_requests_and_preserves_other_streams() {
        let mut queue = MuxerRxQ::new();
        assert!(queue.push_credit_update(5, 6, u32::MAX));
        assert!(queue.push(MuxerRx::CreditRequest {
            local_port: 5,
            peer_port: 6,
            fwd_cnt: u32::MAX
        }));
        assert!(queue.push_credit_update(5, 7, 8));
        assert!(queue.push_credit_update(7, 6, 9));
        assert!(queue.push_credit_update(5, 6, 0));
        assert_eq!(queue.len(), 4);
        assert!(matches!(
            queue.pop(),
            Some(MuxerRx::CreditRequest {
                fwd_cnt: u32::MAX,
                ..
            })
        ));
        assert!(matches!(
            queue.pop(),
            Some(MuxerRx::CreditUpdate {
                local_port: 5,
                peer_port: 7,
                fwd_cnt: 8
            })
        ));
        assert!(matches!(
            queue.pop(),
            Some(MuxerRx::CreditUpdate {
                local_port: 7,
                peer_port: 6,
                fwd_cnt: 9
            })
        ));
        assert!(matches!(
            queue.pop(),
            Some(MuxerRx::CreditUpdate {
                local_port: 5,
                peer_port: 6,
                fwd_cnt: 0
            })
        ));
    }

    #[test]
    fn credit_controls_clear_recycled_rx_header_length_and_flags() {
        let mem = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x4000)]).unwrap();
        mem.write_obj(
            Descriptor {
                addr: 0x2000,
                len: 128,
                flags: 2,
                next: 0,
            },
            GuestAddress(0x1000),
        )
        .unwrap();
        for request in [false, true] {
            mem.write_slice(&[0xff; 128], GuestAddress(0x2000)).unwrap();
            let head = DescriptorChain::checked_new(&mem, GuestAddress(0x1000), 1, 0).unwrap();
            let mut packet = VsockPacket::from_rx_virtq_head(&head).unwrap();
            let control = if request {
                MuxerRx::CreditRequest {
                    local_port: 5,
                    peer_port: 6,
                    fwd_cnt: 7,
                }
            } else {
                MuxerRx::CreditUpdate {
                    local_port: 5,
                    peer_port: 6,
                    fwd_cnt: 7,
                }
            };
            assert!(rx_to_pkt(3, control, &mut packet));
            assert_eq!(packet.len(), 0);
            assert_eq!(packet.flags(), 0);
            assert_eq!(
                (packet.src_port(), packet.dst_port(), packet.fwd_cnt()),
                (5, 6, 7)
            );
            assert_eq!(packet.buf_alloc(), defs::CONN_TX_BUF_SIZE as u32);
        }
    }
}
