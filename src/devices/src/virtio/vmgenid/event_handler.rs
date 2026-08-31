#[cfg(unix)]
use std::os::unix::io::AsRawFd;
#[cfg(windows)]
use std::os::windows::io::AsRawHandle;

use polly::event_manager::{EventManager, Pollable, Subscriber};
use utils::epoll::{EpollEvent, EventSet};

use super::device::Generation;

impl Generation {
    fn queue_event(&self, index: usize) -> &std::sync::Arc<utils::eventfd::EventFd> {
        &self.queues.as_ref().expect("queues should exist")[index].event
    }

    fn handle_activate_event(&mut self, event_manager: &mut EventManager) {
        debug!("virtio-msb-vmgenid: activate event");
        if let Err(error) = self.activate_evt.read() {
            error!("Failed to consume virtio-msb-vmgenid activate event: {error:?}");
        }

        // Generation traffic uses config space. The otherwise-unused queue only causes the
        // virtio-mmio transport to allocate the interrupt used for config-change notifications.
        let activate_evt = eventfd_pollable(&self.activate_evt);
        event_manager
            .unregister(activate_evt)
            .unwrap_or_else(|error| {
                error!("Failed to unregister virtio-msb-vmgenid activate event: {error:?}");
            });
    }
}

impl Subscriber for Generation {
    fn process(&mut self, event: &EpollEvent, event_manager: &mut EventManager) {
        let source = event.fd();
        let activate_evt = eventfd_pollable(&self.activate_evt);
        let unused_queue = eventfd_pollable(self.queue_event(0));

        if source == activate_evt {
            self.handle_activate_event(event_manager);
        } else if source == unused_queue {
            let _ = self.queue_event(0).read();
        } else {
            warn!("Unexpected virtio-msb-vmgenid event received: {source:?}");
        }
    }

    fn interest_list(&self) -> Vec<EpollEvent> {
        vec![EpollEvent::new(
            EventSet::IN,
            pollable_token(eventfd_pollable(&self.activate_evt)),
        )]
    }
}

#[cfg(unix)]
fn eventfd_pollable(event: &utils::eventfd::EventFd) -> Pollable {
    event.as_raw_fd()
}

#[cfg(windows)]
fn eventfd_pollable(event: &utils::eventfd::EventFd) -> Pollable {
    event.as_raw_handle()
}

#[cfg(unix)]
fn pollable_token(pollable: Pollable) -> u64 {
    pollable as u64
}

#[cfg(windows)]
fn pollable_token(pollable: Pollable) -> u64 {
    pollable as usize as u64
}
