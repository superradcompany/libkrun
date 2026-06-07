use std::os::unix::io::AsRawFd;

use polly::event_manager::{EventManager, Subscriber};
use utils::epoll::{EpollEvent, EventSet};
use vm_memory::{ByteValued, Le16, Le64};

use super::device::{
    Balloon, DFQ_INDEX, FRQ_INDEX, IFQ_INDEX, PHQ_INDEX, STQ_INDEX, VIRTIO_BALLOON_S_AVAIL,
};
use crate::virtio::descriptor_utils::Reader;
use crate::virtio::device::VirtioDevice;

#[derive(Clone, Copy, Default)]
#[repr(C, packed)]
struct BalloonStat {
    tag: Le16,
    val: Le64,
}

unsafe impl ByteValued for BalloonStat {}

impl Balloon {
    fn queue_event(&self, idx: usize) -> &std::sync::Arc<utils::eventfd::EventFd> {
        &self.queues.as_ref().expect("queues should exist")[idx].event
    }

    pub(crate) fn handle_ifq_event(&mut self, event: &EpollEvent) {
        error!("balloon: unsupported inflate queue event");

        let event_set = event.event_set();
        if event_set != EventSet::IN {
            warn!("balloon: inflate unexpected event {event_set:?}");
            return;
        }

        if let Err(e) = self.queue_event(IFQ_INDEX).read() {
            error!("Failed to read balloon inflate queue event: {e:?}");
        }
    }

    pub(crate) fn handle_dfq_event(&mut self, event: &EpollEvent) {
        error!("balloon: unsupported deflate queue event");

        let event_set = event.event_set();
        if event_set != EventSet::IN {
            warn!("balloon: deflate unexpected event {event_set:?}");
            return;
        }

        if let Err(e) = self.queue_event(DFQ_INDEX).read() {
            error!("Failed to read balloon inflate queue event: {e:?}");
        }
    }

    pub(crate) fn handle_stq_event(&mut self, event: &EpollEvent) {
        debug!("balloon: stats queue event");

        let event_set = event.event_set();
        if event_set != EventSet::IN {
            warn!("balloon: stats unexpected event {event_set:?}");
            return;
        }

        if let Err(e) = self.queue_event(STQ_INDEX).read() {
            error!("Failed to read balloon stats queue event: {e:?}");
        } else if self.process_stq() {
            self.device_state.signal_used_queue();
        }
    }

    pub(crate) fn handle_phq_event(&mut self, event: &EpollEvent) {
        error!("balloon: unsupported page-hinting queue event");

        let event_set = event.event_set();
        if event_set != EventSet::IN {
            warn!("balloon: page-hinting unexpected event {event_set:?}");
            return;
        }

        if let Err(e) = self.queue_event(PHQ_INDEX).read() {
            error!("Failed to read balloon page-hinting queue event: {e:?}");
        }
    }

    pub(crate) fn handle_frq_event(&mut self, event: &EpollEvent) {
        debug!("balloon: free-page reporting queue event");

        let event_set = event.event_set();
        if event_set != EventSet::IN {
            warn!("balloon: free-page reporting unexpected event {event_set:?}");
            return;
        }

        if let Err(e) = self.queue_event(FRQ_INDEX).read() {
            error!("Failed to read balloon free-page reporting queue event: {e:?}");
        } else if self.process_frq() {
            self.device_state.signal_used_queue();
        }
    }

    fn process_stq(&mut self) -> bool {
        let mem = match self.device_state {
            crate::virtio::DeviceState::Activated(ref mem, _) => mem,
            crate::virtio::DeviceState::Inactive => unreachable!(),
        };
        let metrics = self.metrics.clone();
        let queues = self
            .queues
            .as_mut()
            .expect("queues should exist when activated");
        let mut have_used = false;

        while let Some(head) = queues[STQ_INDEX].queue.pop(mem) {
            let index = head.index;
            let mut reader = match Reader::new(mem, head) {
                Ok(reader) => reader,
                Err(e) => {
                    error!("balloon: invalid stats descriptor chain: {e:?}");
                    have_used = true;
                    if let Err(e) = queues[STQ_INDEX].queue.add_used(mem, index, 0) {
                        error!("balloon: failed to add used stats element: {e:?}");
                    }
                    continue;
                }
            };

            while reader.available_bytes() >= std::mem::size_of::<BalloonStat>() {
                match reader.read_obj::<BalloonStat>() {
                    Ok(stat) => match stat.tag.to_native() {
                        VIRTIO_BALLOON_S_AVAIL => {
                            metrics.set_memory_available_bytes(stat.val.to_native())
                        }
                        _ => {}
                    },
                    Err(e) => {
                        error!("balloon: failed to read stat: {e:?}");
                        break;
                    }
                }
            }

            have_used = true;
            if let Err(e) = queues[STQ_INDEX].queue.add_used(mem, index, 0) {
                error!("balloon: failed to add used stats element: {e:?}");
            }
        }

        have_used
    }

    fn handle_activate_event(&self, event_manager: &mut EventManager) {
        debug!("balloon: activate event");
        if let Err(e) = self.activate_evt.read() {
            error!("Failed to consume balloon activate event: {e:?}");
        }

        // The subscriber must exist as we previously registered activate_evt via
        // `interest_list()`.
        let self_subscriber = event_manager
            .subscriber(self.activate_evt.as_raw_fd())
            .unwrap();

        event_manager
            .register(
                self.queue_event(IFQ_INDEX).as_raw_fd(),
                EpollEvent::new(EventSet::IN, self.queue_event(IFQ_INDEX).as_raw_fd() as u64),
                self_subscriber.clone(),
            )
            .unwrap_or_else(|e| {
                error!("Failed to register balloon ifq with event manager: {e:?}");
            });

        event_manager
            .register(
                self.queue_event(DFQ_INDEX).as_raw_fd(),
                EpollEvent::new(EventSet::IN, self.queue_event(DFQ_INDEX).as_raw_fd() as u64),
                self_subscriber.clone(),
            )
            .unwrap_or_else(|e| {
                error!("Failed to register balloon dfq with event manager: {e:?}");
            });

        event_manager
            .register(
                self.queue_event(STQ_INDEX).as_raw_fd(),
                EpollEvent::new(EventSet::IN, self.queue_event(STQ_INDEX).as_raw_fd() as u64),
                self_subscriber.clone(),
            )
            .unwrap_or_else(|e| {
                error!("Failed to register balloon stq with event manager: {e:?}");
            });

        event_manager
            .register(
                self.queue_event(PHQ_INDEX).as_raw_fd(),
                EpollEvent::new(EventSet::IN, self.queue_event(PHQ_INDEX).as_raw_fd() as u64),
                self_subscriber.clone(),
            )
            .unwrap_or_else(|e| {
                error!("Failed to register balloon dfq with event manager: {e:?}");
            });

        event_manager
            .register(
                self.queue_event(FRQ_INDEX).as_raw_fd(),
                EpollEvent::new(EventSet::IN, self.queue_event(FRQ_INDEX).as_raw_fd() as u64),
                self_subscriber.clone(),
            )
            .unwrap_or_else(|e| {
                error!("Failed to register balloon frq with event manager: {e:?}");
            });

        event_manager
            .unregister(self.activate_evt.as_raw_fd())
            .unwrap_or_else(|e| {
                error!("Failed to unregister balloon activate evt: {e:?}");
            })
    }
}

impl Subscriber for Balloon {
    fn process(&mut self, event: &EpollEvent, event_manager: &mut EventManager) {
        let source = event.fd();
        let ifq = self.queue_event(IFQ_INDEX).as_raw_fd();
        let dfq = self.queue_event(DFQ_INDEX).as_raw_fd();
        let stq = self.queue_event(STQ_INDEX).as_raw_fd();
        let phq = self.queue_event(PHQ_INDEX).as_raw_fd();
        let frq = self.queue_event(FRQ_INDEX).as_raw_fd();
        let activate_evt = self.activate_evt.as_raw_fd();

        if self.is_activated() {
            match source {
                _ if source == ifq => self.handle_ifq_event(event),
                _ if source == dfq => self.handle_dfq_event(event),
                _ if source == stq => self.handle_stq_event(event),
                _ if source == phq => self.handle_phq_event(event),
                _ if source == frq => self.handle_frq_event(event),
                _ if source == activate_evt => {
                    self.handle_activate_event(event_manager);
                }
                _ => warn!("Unexpected balloon event received: {source:?}"),
            }
        } else {
            warn!("balloon: The device is not yet activated. Spurious event received: {source:?}");
        }
    }

    fn interest_list(&self) -> Vec<EpollEvent> {
        vec![EpollEvent::new(
            EventSet::IN,
            self.activate_evt.as_raw_fd() as u64,
        )]
    }
}
