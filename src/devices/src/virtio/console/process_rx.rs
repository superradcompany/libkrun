use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::{io, thread};

use vm_memory::{GuestMemoryBackend, GuestMemoryError, GuestMemoryMmap, GuestMemoryRegion};

use crate::virtio::console::console_control::ConsoleControl;
use crate::virtio::console::port_io::PortInput;
use crate::virtio::{DescriptorChain, InterruptTransport, Queue};

#[allow(clippy::too_many_arguments)]
pub(crate) fn process_rx(
    mem: GuestMemoryMmap,
    mut queue: Queue,
    interrupt: InterruptTransport,
    input: Arc<Mutex<Box<dyn PortInput + Send>>>,
    control: Arc<ConsoleControl>,
    port_id: u32,
    stopfd: utils::eventfd::EventFd,
    stop: Arc<AtomicBool>,
) -> Queue {
    let mem = &mem;
    let mut eof = false;

    let mut input = input.lock().unwrap();
    loop {
        let Some(head) = pop_head_blocking(&mut queue, mem, &interrupt, &stop) else {
            return queue;
        };

        let head_index = head.index;
        let mut bytes_read = 0;
        for chain in head.into_iter().writable() {
            match read_to_desc(chain, input.as_mut(), &mut eof) {
                Ok(0) => {
                    break;
                }
                Ok(len) => {
                    bytes_read += len;
                }
                Err(e) => {
                    log::error!("Failed to read: {e:?}")
                }
            }
        }

        if bytes_read != 0 {
            log::trace!("Rx {bytes_read} bytes queue len{}", queue.len(mem));
            if let Err(e) = queue.add_used(mem, head_index, bytes_read as u32) {
                error!("failed to add used elements to the queue: {e:?}");
            }
        }

        // EOF is terminal for the buffer already consumed from the available ring. Return the
        // descriptor with a zero-length completion before closing the port so a later checkpoint
        // never observes private queue ownership left behind by a departed RX worker.
        if eof {
            if let Err(e) = queue.add_used(mem, head_index, 0) {
                error!("failed to add EOF buffer to the used ring: {e:?}");
            }
            interrupt.signal_used_queue();
            log::trace!("signaling EOF on port {port_id}");
            control.port_open(port_id, false);
            return queue;
        } else if bytes_read == 0 {
            queue.undo_pop();
            interrupt.signal_used_queue();
            input.wait_until_readable(Some(&stopfd));
        }

        if stop.load(Ordering::Acquire) {
            return queue;
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
        if stop.load(Ordering::Acquire) {
            return None;
        }
        match queue.pop(mem) {
            Some(descriptor) => break Some(descriptor),
            None => {
                interrupt.signal_used_queue();
                thread::park();
                log::trace!("rx unparked, queue len {}", queue.len(mem))
            }
        }
    }
}

#[allow(deprecated)]
fn read_to_desc(
    desc: DescriptorChain,
    input: &mut (dyn PortInput + Send),
    eof: &mut bool,
) -> Result<usize, GuestMemoryError> {
    desc.mem
        .try_access(desc.len as usize, desc.addr, |_, len, addr, region| {
            let mut target = region.get_slice(addr, len).unwrap();
            match input.read_volatile(&mut target) {
                Ok(n) => {
                    if n == 0 {
                        *eof = true
                    }
                    Ok(n)
                }
                // We can't return an error otherwise we would not know how many bytes were processed before WouldBlock
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => Ok(0),
                Err(e) => Err(GuestMemoryError::IOError(e)),
            }
        })
}

//--------------------------------------------------------------------------------------------------
// Tests
//--------------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicBool;
    use std::sync::{Arc, Mutex};

    use utils::eventfd::{EventFd, EFD_NONBLOCK};
    use virtio_bindings::virtio_ring::VRING_DESC_F_WRITE;
    use vm_memory::{GuestAddress, GuestMemoryMmap};

    use crate::legacy::DummyIrqChip;
    use crate::virtio::console::console_control::ConsoleControl;
    use crate::virtio::console::port_io::{PortInput, PortInputEmpty};
    use crate::virtio::queue::tests::VirtQueue;

    use super::*;

    #[test]
    fn eof_completes_the_consumed_receive_buffer() {
        let mem = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x1_0000)]).unwrap();
        let virt_queue = VirtQueue::new(GuestAddress(0), &mem, 8);
        virt_queue.dtable[0].set(0x8000, 256, VRING_DESC_F_WRITE as u16, 0);
        virt_queue.avail.ring[0].set(0);
        virt_queue.avail.idx.set(1);

        let input: Arc<Mutex<Box<dyn PortInput + Send>>> =
            Arc::new(Mutex::new(Box::new(PortInputEmpty::new())));
        let interrupt =
            InterruptTransport::new(DummyIrqChip::new().into(), "console-rx".into()).unwrap();
        let queue = process_rx(
            mem.clone(),
            virt_queue.create_queue(),
            interrupt,
            input,
            ConsoleControl::new(),
            0,
            EventFd::new(EFD_NONBLOCK).unwrap(),
            Arc::new(AtomicBool::new(false)),
        );

        assert!(queue.capture_state().is_ok());
        assert_eq!(virt_queue.used.idx.get(), 1);
        assert_eq!(virt_queue.used.ring[0].get().len, 0);
    }
}
