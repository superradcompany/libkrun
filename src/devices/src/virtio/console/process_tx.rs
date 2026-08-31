use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::{io, thread};

use vm_memory::{GuestMemoryBackend, GuestMemoryError, GuestMemoryMmap, GuestMemoryRegion};

use crate::virtio::console::port_io::PortOutput;
use crate::virtio::{DescriptorChain, InterruptTransport, Queue};

pub(crate) fn process_tx(
    mem: GuestMemoryMmap,
    mut queue: Queue,
    interrupt: InterruptTransport,
    output: Arc<Mutex<Box<dyn PortOutput + Send>>>,
    stop: Arc<AtomicBool>,
) -> Queue {
    loop {
        let Some(head) = pop_head_blocking(&mut queue, &mem, &interrupt, &stop) else {
            return queue;
        };

        let head_index = head.index;
        let mut bytes_written = 0;

        for desc in head.into_iter().readable() {
            let desc_len = desc.len as usize;
            match write_desc_to_output(desc, output.lock().unwrap().as_mut(), &interrupt) {
                Ok(0) => {
                    break;
                }
                Ok(n) => {
                    assert_eq!(n, desc_len);
                    bytes_written += n;
                }
                Err(e) => {
                    log::error!("Failed to write output: {e}");
                }
            }
        }

        if bytes_written == 0 {
            log::trace!("Tx Add used {bytes_written}");
            queue.undo_pop();
        } else {
            log::trace!("Tx add used {bytes_written}");
            if let Err(e) = queue.add_used(&mem, head_index, bytes_written as u32) {
                error!("failed to add used elements to the queue: {e:?}");
            }
        }
    }
}

fn pop_head_blocking<'mem>(
    queue: &mut Queue,
    mem: &'mem GuestMemoryMmap,
    interrupt: &InterruptTransport,
    stop: &AtomicBool,
) -> Option<DescriptorChain<'mem>> {
    loop {
        // Once quiescence is requested, work already owned by the caller of this helper may
        // finish, but no additional descriptor may cross into worker ownership.
        if stop.load(Ordering::Acquire) {
            return None;
        }
        match queue.pop(mem) {
            Some(descriptor) => break Some(descriptor),
            None => {
                interrupt.signal_used_queue();
                thread::park();
                if stop.load(Ordering::Acquire) {
                    break None;
                }
                log::trace!("tx unparked, queue len {}", queue.len(mem))
            }
        }
    }
}

#[allow(deprecated)]
fn write_desc_to_output(
    desc: DescriptorChain,
    output: &mut (dyn PortOutput + Send),
    interrupt: &InterruptTransport,
) -> Result<usize, GuestMemoryError> {
    desc.mem
        .try_access(desc.len as usize, desc.addr, |_, len, addr, region| {
            let src = region.get_slice(addr, len).unwrap();
            loop {
                log::trace!("Tx {src:?}, write_volatile {len} bytes");
                match output.write_volatile(&src) {
                    // try_access seem to handle partial write for us (we will be invoked again with an offset)
                    Ok(n) => break Ok(n),
                    // We can't return an error otherwise we would not know how many bytes were processed before WouldBlock
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                        log::trace!("Tx wait for output (would block)");
                        interrupt.signal_used_queue();
                        output.wait_until_writable();
                    }
                    Err(e) => break Err(GuestMemoryError::IOError(e)),
                }
            }
        })
}

//--------------------------------------------------------------------------------------------------
// Tests
//--------------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicBool;

    use vm_memory::{GuestAddress, GuestMemoryMmap};

    use crate::legacy::DummyIrqChip;
    use crate::virtio::queue::tests::VirtQueue;

    use super::*;

    #[test]
    fn stop_priority_leaves_available_transmit_buffers_unconsumed() {
        let mem = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x1_0000)]).unwrap();
        let virt_queue = VirtQueue::new(GuestAddress(0), &mem, 8);
        virt_queue.dtable[0].set(0x8000, 16, 0, 0);
        virt_queue.avail.ring[0].set(0);
        virt_queue.avail.idx.set(1);
        let mut queue = virt_queue.create_queue();
        let interrupt =
            InterruptTransport::new(DummyIrqChip::new().into(), "console-tx".into()).unwrap();

        assert!(pop_head_blocking(&mut queue, &mem, &interrupt, &AtomicBool::new(true)).is_none());
        assert_eq!(queue.len(&mem), 1);
        assert!(queue.capture_state().is_ok());
    }
}
