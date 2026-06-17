use std::os::unix::io::AsRawFd;

use polly::event_manager::{EventManager, Subscriber};
use utils::epoll::{EpollEvent, EventSet};

use super::device::{MsbMetrics, RX_INDEX};
use crate::virtio::device::VirtioDevice;

impl MsbMetrics {
    pub(crate) fn handle_rx_event(&mut self, event: &EpollEvent) {
        debug!("msb_metrics: rx queue event");

        let event_set = event.event_set();
        if event_set != EventSet::IN {
            warn!("msb_metrics: rx queue unexpected event {event_set:?}");
            return;
        }

        if let Err(err) = self.queue_event(RX_INDEX).read() {
            error!("msb_metrics: failed to read rx queue event: {err:?}");
        } else if self.process_rx() {
            self.device_state.signal_used_queue();
        }
    }

    fn handle_activate_event(&self, event_manager: &mut EventManager) {
        debug!("msb_metrics: activate event");
        if let Err(err) = self.activate_evt.read() {
            error!("msb_metrics: failed to consume activate event: {err:?}");
        }

        let self_subscriber = event_manager
            .subscriber(self.activate_evt.as_raw_fd())
            .unwrap();

        event_manager
            .register(
                self.queue_event(RX_INDEX).as_raw_fd(),
                EpollEvent::new(EventSet::IN, self.queue_event(RX_INDEX).as_raw_fd() as u64),
                self_subscriber.clone(),
            )
            .unwrap_or_else(|err| {
                error!("msb_metrics: failed to register rx queue: {err:?}");
            });

        event_manager
            .unregister(self.activate_evt.as_raw_fd())
            .unwrap_or_else(|err| {
                error!("msb_metrics: failed to unregister activate event: {err:?}");
            })
    }
}

impl Subscriber for MsbMetrics {
    fn process(&mut self, event: &EpollEvent, event_manager: &mut EventManager) {
        let source = event.fd();
        let activate_evt = self.activate_evt.as_raw_fd();

        if source == activate_evt {
            if !self.is_activated() {
                warn!("msb_metrics: device is not activated; spurious event received: {source:?}");
                return;
            }

            self.handle_activate_event(event_manager);
            return;
        }

        if !self.is_activated() {
            warn!("msb_metrics: device is not activated; spurious event received: {source:?}");
            return;
        }

        let rx = self.queue_event(RX_INDEX).as_raw_fd();
        match source {
            _ if source == rx => self.handle_rx_event(event),
            _ => warn!("msb_metrics: unexpected event received: {source:?}"),
        }
    }

    fn interest_list(&self) -> Vec<EpollEvent> {
        vec![EpollEvent::new(
            EventSet::IN,
            self.activate_evt.as_raw_fd() as u64,
        )]
    }
}
