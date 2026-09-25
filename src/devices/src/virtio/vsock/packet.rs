// Copyright 2018 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0
//

/// `VsockPacket` wraps a virtqueue packet header and its optional payload.
/// Single-descriptor payloads reference guest memory directly; fragmented TX
/// payloads are gathered into a bounded buffer for the vsock backend.
use std::convert::TryInto;
use std::ffi::CStr;
#[cfg(unix)]
use std::net::{Ipv4Addr, SocketAddrV4};
#[cfg(target_os = "macos")]
use std::net::{Ipv6Addr, SocketAddrV6};
use std::os::raw::c_char;
use std::result;

#[cfg(target_os = "linux")]
use nix::sys::socket::{sockaddr, AddressFamily};
#[cfg(unix)]
use nix::sys::socket::{SockaddrLike, SockaddrStorage};
use utils::byte_order;
use vm_memory::{self, Address, GuestAddress, GuestMemory, GuestMemoryError};

use super::super::DescriptorChain;
use super::defs;
use super::{Result, VsockError};

// The vsock packet header is defined by the C struct:
//
// ```C
// struct virtio_vsock_hdr {
//     le64 src_cid;
//     le64 dst_cid;
//     le32 src_port;
//     le32 dst_port;
//     le32 len;
//     le16 type;
//     le16 op;
//     le32 flags;
//     le32 buf_alloc;
//     le32 fwd_cnt;
// };
// ```
//
// This structed will occupy the buffer pointed to by the head descriptor. We'll be accessing it
// as a byte slice. To that end, we define below the offsets for each field struct, as well as the
// packed struct size, as a bunch of `usize` consts.
// Note that these offsets are only used privately by the `VsockPacket` struct, the public interface
// consisting of getter and setter methods, for each struct field, that will also handle the correct
// endianess.

/// The vsock packet header struct size (when packed).
pub const VSOCK_PKT_HDR_SIZE: usize = 44;

// Source CID.
const HDROFF_SRC_CID: usize = 0;

// Destination CID.
const HDROFF_DST_CID: usize = 8;

// Source port.
const HDROFF_SRC_PORT: usize = 16;

// Destination port.
const HDROFF_DST_PORT: usize = 20;

// Data length (in bytes) - may be 0, if there is no data buffer.
const HDROFF_LEN: usize = 24;

// Socket type. Currently, only connection-oriented streams are defined by the vsock protocol.
const HDROFF_TYPE: usize = 28;

// Operation ID - one of the VSOCK_OP_* values; e.g.
// - VSOCK_OP_RW: a data packet;
// - VSOCK_OP_REQUEST: connection request;
// - VSOCK_OP_RST: forcefull connection termination;
// etc (see `super::defs::uapi` for the full list).
const HDROFF_OP: usize = 30;

// Additional options (flags) associated with the current operation (`op`).
// Currently, only used with shutdown requests (VSOCK_OP_SHUTDOWN).
const HDROFF_FLAGS: usize = 32;

// Size (in bytes) of the packet sender receive buffer (for the connection to which this packet
// belongs).
const HDROFF_BUF_ALLOC: usize = 36;

// Number of bytes the sender has received and consumed (for the connection to which this packet
// belongs). For instance, for our Unix backend, this counter would be the total number of bytes
// we have successfully written to a backing Unix socket.
const HDROFF_FWD_CNT: usize = 40;

#[cfg(unix)]
#[repr(C)]
pub struct TsiProxyCreate {
    pub peer_port: u32,
    pub family: u16,
    pub _type: u16,
}

#[cfg(unix)]
#[repr(C)]
pub struct TsiConnectReq {
    pub peer_port: u32,
    pub addr: SockaddrStorage,
}

#[cfg(unix)]
#[repr(C)]
pub struct TsiConnectRsp {
    pub result: i32,
}

#[cfg(unix)]
#[repr(C)]
pub struct TsiGetnameReq {
    pub peer_port: u32,
    pub local_port: u32,
    pub peer: u32,
}

#[cfg(unix)]
#[repr(C)]
#[derive(Debug)]
pub struct TsiGetnameRsp {
    pub result: i32,
    pub addr_len: u32,
    pub addr: SockaddrStorage,
}

#[cfg(unix)]
impl Default for TsiGetnameRsp {
    fn default() -> Self {
        let addr: SockaddrStorage = SocketAddrV4::new(Ipv4Addr::new(0, 0, 0, 0), 0).into();
        TsiGetnameRsp {
            result: -1,
            // It's fine to unwrap here sice we've just created the SocketAddrV4 above.
            addr_len: addr.as_sockaddr_in().unwrap().len(),
            addr,
        }
    }
}

#[cfg(unix)]
#[repr(C)]
#[derive(Debug)]
pub struct TsiSendtoAddr {
    pub peer_port: u32,
    pub addr: SockaddrStorage,
}

#[cfg(unix)]
#[repr(C)]
#[derive(Debug)]
pub struct TsiListenReq {
    pub peer_port: u32,
    pub vm_port: u32,
    pub backlog: i32,
    pub addr: SockaddrStorage,
}

#[cfg(unix)]
#[repr(C)]
#[derive(Debug)]
pub struct TsiListenRsp {
    pub result: i32,
}

#[cfg(unix)]
#[repr(C)]
#[derive(Debug)]
pub struct TsiAcceptReq {
    pub peer_port: u32,
    pub flags: u32,
}

#[cfg(unix)]
#[repr(C)]
#[derive(Debug)]
pub struct TsiAcceptRsp {
    pub result: i32,
}

#[cfg(unix)]
#[repr(C)]
pub struct TsiReleaseReq {
    pub peer_port: u32,
    pub local_port: u32,
}

/// The vsock packet wraps a virtq descriptor chain.
/// Fragmented TX payloads are gathered; single-descriptor payloads stay zero-copy.
pub struct VsockPacket {
    hdr: *mut u8,
    buf: Option<*mut u8>,
    buf_size: usize,
    // Keeps the backing allocation alive when TX data spans descriptors.
    owned_buf: Option<Vec<u8>>,
}

fn get_host_address<T: GuestMemory + vm_memory::GuestMemoryBackend>(
    mem: &T,
    guest_addr: GuestAddress,
    size: usize,
) -> result::Result<*mut u8, GuestMemoryError> {
    Ok(mem.get_slice(guest_addr, size)?.ptr_guard_mut().as_ptr())
}

impl VsockPacket {
    /// Create the packet wrapper from a TX virtq chain head.
    ///
    /// The chain head holds the header, followed by readable payload descriptors.
    /// Bounds and pointer checks are performed when creating the wrapper.
    pub fn from_tx_virtq_head(head: &DescriptorChain) -> Result<Self> {
        // All buffers in the TX queue must be readable.
        //
        if head.is_write_only() {
            return Err(VsockError::UnreadableDescriptor);
        }

        // The packet header should fit inside the head descriptor.
        if head.len < VSOCK_PKT_HDR_SIZE as u32 {
            return Err(VsockError::HdrDescTooSmall(head.len));
        }

        let mut pkt = Self {
            hdr: get_host_address(head.mem, head.addr, VSOCK_PKT_HDR_SIZE)
                .map_err(VsockError::GuestMemoryMmap)?,
            buf: None,
            buf_size: 0,
            owned_buf: None,
        };

        let payload_len = pkt.len() as usize;
        // No point looking for a data/buffer descriptor, if the packet is zero-lengthed.
        if payload_len == 0 {
            return Ok(pkt);
        }

        // Reject weirdly-sized packets.
        //
        if payload_len > defs::MAX_PKT_BUF_SIZE {
            return Err(VsockError::InvalidPktLen(payload_len as u32));
        }

        // If the packet header showed a non-zero length, there should be a data descriptor here.
        let buf_desc = head.next_descriptor().ok_or(VsockError::BufDescMissing)?;

        // TX data should be read-only.
        if buf_desc.is_write_only() {
            return Err(VsockError::UnreadableDescriptor);
        }

        // Linux can scatter a large TX payload across multiple descriptors.
        if (buf_desc.len as usize) < payload_len {
            let mut data = Vec::with_capacity(payload_len);
            let mut desc = buf_desc;
            loop {
                if desc.is_write_only() {
                    return Err(VsockError::UnreadableDescriptor);
                }
                let count = (desc.len as usize).min(payload_len - data.len());
                let ptr = get_host_address(desc.mem, desc.addr, count)
                    .map_err(VsockError::GuestMemoryMmap)?;
                // The guest range was checked above; copy only declared payload bytes.
                data.extend_from_slice(unsafe { std::slice::from_raw_parts(ptr, count) });
                if data.len() == payload_len {
                    break;
                }
                desc = desc.next_descriptor().ok_or(VsockError::BufDescTooSmall)?;
            }
            pkt.buf_size = data.len();
            pkt.buf = Some(data.as_mut_ptr());
            pkt.owned_buf = Some(data);
            return Ok(pkt);
        }

        pkt.buf_size = buf_desc.len as usize;
        pkt.buf = Some(
            get_host_address(buf_desc.mem, buf_desc.addr, pkt.buf_size)
                .map_err(VsockError::GuestMemoryMmap)?,
        );

        Ok(pkt)
    }

    /// Create the packet wrapper from an RX virtq chain head.
    ///
    /// There must be two descriptors in the chain, both writable: a header descriptor and a data
    /// descriptor. Bounds and pointer checks are performed when creating the wrapper.
    pub fn from_rx_virtq_head(head: &DescriptorChain) -> Result<Self> {
        // All RX buffers must be writable.
        //
        if !head.is_write_only() {
            return Err(VsockError::UnwritableDescriptor);
        }

        // The packet header should fit inside the head descriptor.
        if head.len < VSOCK_PKT_HDR_SIZE as u32 {
            return Err(VsockError::HdrDescTooSmall(head.len));
        }

        let mut pkt = Self {
            hdr: get_host_address(head.mem, head.addr, VSOCK_PKT_HDR_SIZE)
                .map_err(VsockError::GuestMemoryMmap)?,
            buf: None,
            buf_size: 0,
            owned_buf: None,
        };

        // Starting from Linux 6.2 the virtio-vsock driver can use a single descriptor for both
        // header and data.
        if !head.has_next() && head.len > VSOCK_PKT_HDR_SIZE as u32 {
            let buf_addr = head
                .addr
                .checked_add(VSOCK_PKT_HDR_SIZE as u64)
                .ok_or(VsockError::GuestMemoryBounds)?;

            pkt.buf_size = head.len as usize - VSOCK_PKT_HDR_SIZE;
            pkt.buf = Some(
                get_host_address(head.mem, buf_addr, pkt.buf_size)
                    .map_err(VsockError::GuestMemoryMmap)?,
            );
        } else {
            let buf_desc = head.next_descriptor().ok_or(VsockError::BufDescMissing)?;

            pkt.buf_size = buf_desc.len as usize;
            pkt.buf = Some(
                get_host_address(buf_desc.mem, buf_desc.addr, pkt.buf_size)
                    .map_err(VsockError::GuestMemoryMmap)?,
            );
        }

        Ok(pkt)
    }

    /// Provides in-place, byte-slice, access to the vsock packet header.
    pub fn hdr(&self) -> &[u8] {
        // This is safe since bound checks have already been performed when creating the packet
        // from the virtq descriptor.
        unsafe { std::slice::from_raw_parts(self.hdr as *const u8, VSOCK_PKT_HDR_SIZE) }
    }

    /// Provides in-place, byte-slice, mutable access to the vsock packet header.
    pub fn hdr_mut(&mut self) -> &mut [u8] {
        // This is safe since bound checks have already been performed when creating the packet
        // from the virtq descriptor.
        unsafe { std::slice::from_raw_parts_mut(self.hdr, VSOCK_PKT_HDR_SIZE) }
    }

    /// Provides in-place, byte-slice access to the vsock packet data buffer.
    ///
    /// Note: control packets (e.g. connection request or reset) have no data buffer associated.
    ///       For those packets, this method will return `None`.
    /// Also note: calling `len()` on the returned slice will yield the buffer size, which may be
    ///            (and often is) larger than the length of the packet data. The packet data length
    ///            is stored in the packet header, and accessible via `VsockPacket::len()`.
    pub fn buf(&self) -> Option<&[u8]> {
        self.buf.map(|ptr| {
            // This is safe since bound checks have already been performed when creating the packet
            // from the virtq descriptor.
            unsafe { std::slice::from_raw_parts(ptr as *const u8, self.buf_size) }
        })
    }

    /// Return exactly the data length declared in the packet header rather
    /// than the potentially larger backing descriptor.
    pub(crate) fn payload(&self) -> Option<&[u8]> {
        let len = self.len() as usize;
        if len == 0 {
            return Some(&[]);
        }
        self.buf()?.get(..len)
    }

    /// Provides in-place, byte-slice, mutable access to the vsock packet data buffer.
    ///
    /// Note: control packets (e.g. connection request or reset) have no data buffer associated.
    ///       For those packets, this method will return `None`.
    /// Also note: calling `len()` on the returned slice will yield the buffer size, which may be
    ///            (and often is) larger than the length of the packet data. The packet data length
    ///            is stored in the packet header, and accessible via `VsockPacket::len()`.
    pub fn buf_mut(&mut self) -> Option<&mut [u8]> {
        self.buf.map(|ptr| {
            // This is safe since bound checks have already been performed when creating the packet
            // from the virtq descriptor.
            unsafe { std::slice::from_raw_parts_mut(ptr, self.buf_size) }
        })
    }

    pub fn src_cid(&self) -> u64 {
        byte_order::read_le_u64(&self.hdr()[HDROFF_SRC_CID..])
    }

    pub fn set_src_cid(&mut self, cid: u64) -> &mut Self {
        byte_order::write_le_u64(&mut self.hdr_mut()[HDROFF_SRC_CID..], cid);
        self
    }

    pub fn dst_cid(&self) -> u64 {
        byte_order::read_le_u64(&self.hdr()[HDROFF_DST_CID..])
    }

    pub fn set_dst_cid(&mut self, cid: u64) -> &mut Self {
        byte_order::write_le_u64(&mut self.hdr_mut()[HDROFF_DST_CID..], cid);
        self
    }

    pub fn src_port(&self) -> u32 {
        byte_order::read_le_u32(&self.hdr()[HDROFF_SRC_PORT..])
    }

    pub fn set_src_port(&mut self, port: u32) -> &mut Self {
        byte_order::write_le_u32(&mut self.hdr_mut()[HDROFF_SRC_PORT..], port);
        self
    }

    pub fn dst_port(&self) -> u32 {
        byte_order::read_le_u32(&self.hdr()[HDROFF_DST_PORT..])
    }

    pub fn set_dst_port(&mut self, port: u32) -> &mut Self {
        byte_order::write_le_u32(&mut self.hdr_mut()[HDROFF_DST_PORT..], port);
        self
    }

    pub fn len(&self) -> u32 {
        byte_order::read_le_u32(&self.hdr()[HDROFF_LEN..])
    }

    pub fn set_len(&mut self, len: u32) -> &mut Self {
        byte_order::write_le_u32(&mut self.hdr_mut()[HDROFF_LEN..], len);
        self
    }

    pub fn type_(&self) -> u16 {
        byte_order::read_le_u16(&self.hdr()[HDROFF_TYPE..])
    }

    pub fn set_type(&mut self, type_: u16) -> &mut Self {
        byte_order::write_le_u16(&mut self.hdr_mut()[HDROFF_TYPE..], type_);
        self
    }

    pub fn op(&self) -> u16 {
        byte_order::read_le_u16(&self.hdr()[HDROFF_OP..])
    }

    pub fn set_op(&mut self, op: u16) -> &mut Self {
        byte_order::write_le_u16(&mut self.hdr_mut()[HDROFF_OP..], op);
        self
    }

    pub fn flags(&self) -> u32 {
        byte_order::read_le_u32(&self.hdr()[HDROFF_FLAGS..])
    }

    pub fn set_flags(&mut self, flags: u32) -> &mut Self {
        byte_order::write_le_u32(&mut self.hdr_mut()[HDROFF_FLAGS..], flags);
        self
    }

    pub fn set_flag(&mut self, flag: u32) -> &mut Self {
        self.set_flags(self.flags() | flag);
        self
    }

    pub fn buf_alloc(&self) -> u32 {
        byte_order::read_le_u32(&self.hdr()[HDROFF_BUF_ALLOC..])
    }

    pub fn set_buf_alloc(&mut self, buf_alloc: u32) -> &mut Self {
        byte_order::write_le_u32(&mut self.hdr_mut()[HDROFF_BUF_ALLOC..], buf_alloc);
        self
    }

    pub fn fwd_cnt(&self) -> u32 {
        byte_order::read_le_u32(&self.hdr()[HDROFF_FWD_CNT..])
    }

    pub fn set_fwd_cnt(&mut self, fwd_cnt: u32) -> &mut Self {
        byte_order::write_le_u32(&mut self.hdr_mut()[HDROFF_FWD_CNT..], fwd_cnt);
        self
    }

    pub fn sa_family(&self) -> Option<u16> {
        if self.buf_size >= 2 {
            Some(byte_order::read_le_u16(&self.buf().unwrap()[0..]))
        } else {
            None
        }
    }

    pub fn inet_port(&self) -> Option<u16> {
        if self.buf_size >= 4 {
            Some(byte_order::read_be_u16(&self.buf().unwrap()[2..]))
        } else {
            None
        }
    }

    pub fn inet_addr(&self) -> Option<[u8; 4]> {
        if self.buf_size >= 8 {
            let ptr = &self.buf().unwrap()[4];
            let slice = unsafe { std::slice::from_raw_parts(ptr as *const u8, 4) };
            slice[0..4].try_into().ok()
        } else {
            None
        }
    }

    pub fn unix_path(&self) -> Option<&str> {
        if self.buf_size >= 108 {
            let cstr =
                unsafe { CStr::from_ptr(&self.buf().unwrap()[2] as *const _ as *const c_char) };
            cstr.to_str().ok()
        } else {
            None
        }
    }

    #[cfg(target_os = "linux")]
    fn parse_address(buf: &[u8], addr_len: u32) -> Option<SockaddrStorage> {
        let sockaddr: SockaddrStorage = unsafe {
            SockaddrStorage::from_raw(&buf[0] as *const _ as *const sockaddr, Some(addr_len))?
        };

        match sockaddr.family() {
            Some(AddressFamily::Inet) => debug!("parse_address: AF_INET"),
            Some(AddressFamily::Inet6) => debug!("parse_address: AF_INET6"),
            Some(AddressFamily::Unix) => debug!("parse_address: AF_UNIX"),
            _ => {
                if let Some(family) = sockaddr.family() {
                    warn!("parse_address: unsupported family {family:?}");
                } else {
                    warn!("parse_address: error parsing family");
                }
                return None;
            }
        }

        Some(sockaddr)
    }

    #[cfg(target_os = "macos")]
    fn parse_address(buf: &[u8], _addr_len: u32) -> Option<SockaddrStorage> {
        let family: u16 = byte_order::read_le_u16(&buf[0..2]);

        match family {
            defs::LINUX_AF_INET => {
                debug!("parse_address: AF_INET");
                let in_port: u16 = byte_order::read_be_u16(&buf[2..4]);
                let in_addr = Ipv4Addr::new(buf[4], buf[5], buf[6], buf[7]);
                Some(SocketAddrV4::new(in_addr, in_port).into())
            }
            defs::LINUX_AF_INET6 => {
                debug!("parse_address: AF_INET6");
                let in_port: u16 = byte_order::read_be_u16(&buf[2..4]);
                let flowinfo: u32 = byte_order::read_be_u32(&buf[4..8]);
                let in6_addr = Ipv6Addr::new(
                    byte_order::read_be_u16(&buf[8..10]),
                    byte_order::read_be_u16(&buf[10..12]),
                    byte_order::read_be_u16(&buf[12..14]),
                    byte_order::read_be_u16(&buf[14..16]),
                    byte_order::read_be_u16(&buf[16..18]),
                    byte_order::read_be_u16(&buf[18..20]),
                    byte_order::read_be_u16(&buf[20..22]),
                    byte_order::read_be_u16(&buf[22..24]),
                );
                let scope_id: u32 = byte_order::read_be_u32(&buf[24..28]);
                Some(SocketAddrV6::new(in6_addr, in_port, flowinfo, scope_id).into())
            }
            defs::LINUX_AF_UNIX => {
                // On macOS, SockaddrStorage doesn't implement `from_raw` for
                // Unix sockets, nor a way to cast an UnixPath to it.
                error!("AF_UNIX sockets aren't yet supported on macOS");
                None
            }
            _ => None,
        }
    }

    #[cfg(unix)]
    pub fn read_proxy_create(&self) -> Option<TsiProxyCreate> {
        if self.buf_size >= 6 {
            let peer_port: u32 = byte_order::read_le_u32(&self.buf().unwrap()[0..]);
            let family: u16 = byte_order::read_le_u16(&self.buf().unwrap()[4..]);
            let _type: u16 = byte_order::read_le_u16(&self.buf().unwrap()[6..]);

            Some(TsiProxyCreate {
                peer_port,
                family,
                _type,
            })
        } else {
            None
        }
    }

    #[cfg(unix)]
    pub fn read_connect_req(&self) -> Option<TsiConnectReq> {
        if self.buf_size >= 4 {
            let buf = self.buf().unwrap();
            let peer_port: u32 = byte_order::read_le_u32(&buf[0..]);
            let addr_len: u32 = byte_order::read_le_u32(&buf[4..]);
            let addr = Self::parse_address(&buf[8..], addr_len)?;

            Some(TsiConnectReq { peer_port, addr })
        } else {
            None
        }
    }

    #[cfg(unix)]
    pub fn write_connect_rsp(&mut self, rsp: TsiConnectRsp) {
        if self.buf_size >= 4 {
            if let Some(buf) = self.buf_mut() {
                byte_order::write_le_u32(&mut buf[0..], rsp.result as u32);
            }
        }
    }

    #[cfg(unix)]
    pub fn read_getname_req(&self) -> Option<TsiGetnameReq> {
        if self.buf_size >= 12 {
            let peer_port: u32 = byte_order::read_le_u32(&self.buf().unwrap()[0..]);
            let local_port: u32 = byte_order::read_le_u32(&self.buf().unwrap()[4..]);
            let peer: u32 = byte_order::read_le_u32(&self.buf().unwrap()[8..]);
            Some(TsiGetnameReq {
                peer_port,
                local_port,
                peer,
            })
        } else {
            None
        }
    }

    #[cfg(unix)]
    pub fn write_getname_rsp(&mut self, rsp: TsiGetnameRsp) {
        if self.buf_size >= 132 {
            if let Some(buf) = self.buf_mut() {
                byte_order::write_le_u32(&mut buf[0..], rsp.result as u32);
                byte_order::write_le_u32(&mut buf[4..], rsp.addr_len);
                let addr_ptr = rsp.addr.as_ptr();
                let slice = unsafe {
                    std::slice::from_raw_parts(addr_ptr as *const u8, rsp.addr.len() as usize)
                };
                buf[8..(rsp.addr.len() + 8) as usize].copy_from_slice(slice);

                // On macOS, convert BSD sockaddr (u8 sa_len + u8 sa_family) to
                // Linux wire format (u16 sa_family). Also translate macOS AF_*
                // values to their Linux equivalents (e.g. AF_INET6: 30 → 10).
                #[cfg(target_os = "macos")]
                {
                    let bsd_family = buf[9];
                    let linux_family: u16 = match bsd_family as i32 {
                        libc::AF_INET => defs::LINUX_AF_INET,
                        libc::AF_INET6 => defs::LINUX_AF_INET6,
                        _ => 0, // AF_UNSPEC
                    };
                    byte_order::write_le_u16(&mut buf[8..], linux_family);
                }
            }
        }
    }

    #[cfg(unix)]
    pub fn read_sendto_addr(&self) -> Option<TsiSendtoAddr> {
        if self.buf_size >= 4 {
            let buf = self.buf().unwrap();
            let peer_port: u32 = byte_order::read_le_u32(&buf[0..]);
            let addr_len: u32 = byte_order::read_le_u32(&buf[4..]);
            let addr = Self::parse_address(&buf[8..], addr_len)?;

            Some(TsiSendtoAddr { peer_port, addr })
        } else {
            None
        }
    }

    #[cfg(unix)]
    pub fn read_listen_req(&self) -> Option<TsiListenReq> {
        if self.buf_size >= 12 {
            let buf = self.buf().unwrap();
            let peer_port: u32 = byte_order::read_le_u32(&buf[0..]);
            let vm_port: u32 = byte_order::read_le_u32(&buf[4..]);
            let backlog: u32 = byte_order::read_le_u32(&buf[8..]);
            let addr_len: u32 = byte_order::read_le_u32(&buf[12..]);
            let addr = Self::parse_address(&buf[16..], addr_len)?;

            Some(TsiListenReq {
                peer_port,
                vm_port,
                backlog: backlog as i32,
                addr,
            })
        } else {
            None
        }
    }

    #[cfg(unix)]
    pub fn write_listen_rsp(&mut self, rsp: TsiListenRsp) {
        if self.buf_size >= 4 {
            if let Some(buf) = self.buf_mut() {
                byte_order::write_le_u32(&mut buf[0..], rsp.result as u32);
            }
        }
    }

    #[cfg(unix)]
    pub fn read_accept_req(&self) -> Option<TsiAcceptReq> {
        if self.buf_size >= 8 {
            let peer_port: u32 = byte_order::read_le_u32(&self.buf().unwrap()[0..]);
            let flags: u32 = byte_order::read_le_u32(&self.buf().unwrap()[4..]);

            Some(TsiAcceptReq { peer_port, flags })
        } else {
            None
        }
    }

    #[cfg(unix)]
    pub fn write_accept_rsp(&mut self, rsp: TsiAcceptRsp) {
        if self.buf_size >= 4 {
            if let Some(buf) = self.buf_mut() {
                byte_order::write_le_u32(&mut buf[0..], rsp.result as u32);
            }
        }
    }

    #[cfg(unix)]
    pub fn read_release_req(&self) -> Option<TsiReleaseReq> {
        if self.buf_size >= 8 {
            let peer_port: u32 = byte_order::read_le_u32(&self.buf().unwrap()[0..]);
            let local_port: u32 = byte_order::read_le_u32(&self.buf().unwrap()[4..]);
            Some(TsiReleaseReq {
                peer_port,
                local_port,
            })
        } else {
            None
        }
    }

    pub fn write_time_sync(&mut self, time: u64) {
        if self.buf_size >= 8 {
            if let Some(buf) = self.buf_mut() {
                byte_order::write_le_u64(&mut buf[0..], time);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::virtio::Descriptor;
    use vm_memory::{Bytes, GuestMemoryMmap};

    fn fragmented_packet(
        mem: &GuestMemoryMmap,
        declared_len: u32,
        tail_flags: u16,
    ) -> Result<VsockPacket> {
        for (index, descriptor) in [
            Descriptor {
                addr: 0x2000,
                len: VSOCK_PKT_HDR_SIZE as u32,
                flags: 1,
                next: 1,
            },
            Descriptor {
                addr: 0x3000,
                len: 32768,
                flags: 1,
                next: 2,
            },
            Descriptor {
                addr: 0x13000,
                len: 32768,
                flags: tail_flags,
                next: 0,
            },
        ]
        .into_iter()
        .enumerate()
        {
            mem.write_obj(descriptor, GuestAddress(0x1000 + index as u64 * 16))
                .unwrap();
        }
        let mut header = [0u8; VSOCK_PKT_HDR_SIZE];
        header[24..28].copy_from_slice(&declared_len.to_le_bytes());
        mem.write_slice(&header, GuestAddress(0x2000)).unwrap();
        mem.write_slice(&vec![b'a'; 32768], GuestAddress(0x3000))
            .unwrap();
        mem.write_slice(&vec![b'b'; 32768], GuestAddress(0x13000))
            .unwrap();
        let head = DescriptorChain::checked_new(mem, GuestAddress(0x1000), 3, 0).unwrap();
        VsockPacket::from_tx_virtq_head(&head)
    }

    #[test]
    fn gathers_fragmented_tx_without_forwarding_descriptor_padding() {
        let mem = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x20000)]).unwrap();
        let packet = fragmented_packet(&mem, 65535, 0).unwrap();
        let mut expected = vec![b'a'; 32768];
        expected.extend_from_slice(&vec![b'b'; 32767]);
        assert_eq!(packet.payload().unwrap(), expected);
    }

    #[test]
    fn rejects_writable_fragment() {
        let mem = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x20000)]).unwrap();
        assert!(matches!(
            fragmented_packet(&mem, 65535, 2),
            Err(VsockError::UnreadableDescriptor)
        ));
    }

    #[test]
    fn rejects_truncated_fragment_chain() {
        let mem = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x20000)]).unwrap();
        let _ = fragmented_packet(&mem, 65535, 0).unwrap();
        mem.write_obj(
            Descriptor {
                addr: 0x13000,
                len: 1,
                flags: 0,
                next: 0,
            },
            GuestAddress(0x1020),
        )
        .unwrap();
        let head = DescriptorChain::checked_new(&mem, GuestAddress(0x1000), 3, 0).unwrap();
        assert!(matches!(
            VsockPacket::from_tx_virtq_head(&head),
            Err(VsockError::BufDescTooSmall)
        ));
    }
}
