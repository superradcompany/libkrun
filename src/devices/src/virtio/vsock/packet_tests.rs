use super::*;
use crate::virtio::queue::Descriptor;
use vm_memory::{Bytes, GuestMemoryMmap};

fn memory() -> GuestMemoryMmap {
    GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x20000)]).unwrap()
}

fn descriptor(mem: &GuestMemoryMmap, index: u16, addr: u64, len: u32, flags: u16, next: u16) {
    mem.write_obj(
        Descriptor {
            addr,
            len,
            flags,
            next,
        },
        GuestAddress(0x1000 + u64::from(index) * 16),
    )
    .unwrap();
}

fn header(mem: &GuestMemoryMmap, length: u32) {
    descriptor(mem, 0, 0x2000, VSOCK_PKT_HDR_SIZE as u32, 1, 1);
    let mut bytes = [0; VSOCK_PKT_HDR_SIZE];
    bytes[24..28].copy_from_slice(&length.to_le_bytes());
    mem.write_slice(&bytes, GuestAddress(0x2000)).unwrap();
}

fn packet(mem: &GuestMemoryMmap) -> Result<VsockPacket> {
    let head = DescriptorChain::checked_new(mem, GuestAddress(0x1000), 16, 0).unwrap();
    VsockPacket::from_tx_virtq_head(&head)
}

#[test]
fn fragmented_tx_payload_survives_packet_move_and_excludes_tail() {
    let mem = memory();
    header(&mem, 6);
    descriptor(&mem, 1, 0x3000, 2, 1, 2);
    descriptor(&mem, 2, 0x5000, 12, 0, 0);
    mem.write_slice(b"ab", GuestAddress(0x3000)).unwrap();
    mem.write_slice(b"cdefSECRET!!", GuestAddress(0x5000))
        .unwrap();
    let mut packets = vec![packet(&mem).unwrap()];
    // A move cannot leave a pointer into a former stack-local payload owner.
    let mut pkt = packets.pop().unwrap();
    assert_eq!(pkt.payload().unwrap(), b"abcdef");
    assert_eq!(pkt.buf().unwrap(), b"abcdef");
    pkt.buf_mut().unwrap()[0] = b'X';
    assert_eq!(pkt.payload().unwrap(), b"Xbcdef");
}

#[test]
fn contiguous_tx_keeps_guest_backing() {
    let mem = memory();
    header(&mem, 2);
    descriptor(&mem, 1, 0x3000, 8, 0, 0);
    mem.write_slice(b"abSECRET", GuestAddress(0x3000)).unwrap();
    let pkt = packet(&mem).unwrap();
    assert!(pkt.tx_payload.is_none());
    assert_eq!(pkt.payload().unwrap(), b"ab");
}

#[test]
fn fragmented_tx_handles_empty_descriptor_and_full_64k() {
    let mem = memory();
    header(&mem, 65536);
    descriptor(&mem, 1, 0x3000, 0, 1, 2);
    descriptor(&mem, 2, 0x4000, 32768, 1, 3);
    descriptor(&mem, 3, 0xc000, 32768, 0, 0);
    let data: Vec<u8> = (0..65536).map(|i| (i % 251) as u8).collect();
    mem.write_slice(&data, GuestAddress(0x4000)).unwrap();
    assert_eq!(packet(&mem).unwrap().payload().unwrap(), data);
}

#[test]
fn fragmented_tx_rejects_short_chain() {
    let mem = memory();
    header(&mem, 6);
    descriptor(&mem, 1, 0x3000, 2, 1, 2);
    descriptor(&mem, 2, 0x5000, 2, 0, 0);
    assert!(matches!(packet(&mem), Err(VsockError::BufDescTooSmall)));
}

#[test]
fn fragmented_tx_rejects_writable_later_descriptor() {
    let mem = memory();
    header(&mem, 6);
    descriptor(&mem, 1, 0x3000, 2, 1, 2);
    descriptor(&mem, 2, 0x5000, 4, 2, 0);
    assert!(matches!(
        packet(&mem),
        Err(VsockError::UnreadableDescriptor)
    ));
}

#[test]
fn fragmented_tx_rejects_out_of_bounds_later_descriptor() {
    let mem = memory();
    header(&mem, 6);
    descriptor(&mem, 1, 0x3000, 2, 1, 2);
    descriptor(&mem, 2, 0x1ffff, 4, 0, 0);
    assert!(matches!(packet(&mem), Err(VsockError::GuestMemoryMmap(_))));
}

#[test]
fn fragmented_tx_bounds_cyclic_traversal() {
    let mem = memory();
    header(&mem, 6);
    descriptor(&mem, 1, 0x3000, 0, 1, 1);
    assert!(matches!(packet(&mem), Err(VsockError::BufDescTooSmall)));
}

#[test]
fn tx_preserves_header_and_size_validation() {
    let mem = memory();
    header(&mem, 65537);
    assert!(matches!(
        packet(&mem),
        Err(VsockError::InvalidPktLen(65537))
    ));
    header(&mem, 0);
    assert_eq!(packet(&mem).unwrap().payload().unwrap(), b"");
    descriptor(&mem, 0, 0x2000, VSOCK_PKT_HDR_SIZE as u32, 3, 1);
    assert!(matches!(
        packet(&mem),
        Err(VsockError::UnreadableDescriptor)
    ));
    descriptor(&mem, 0, 0x2000, 1, 1, 1);
    assert!(matches!(packet(&mem), Err(VsockError::HdrDescTooSmall(1))));
}
