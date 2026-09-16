#[cfg(unix)]
use std::os::unix::io::AsRawFd;
#[cfg(windows)]
use std::os::windows::io::AsRawHandle;

use polly::event_manager::{EventManager, Pollable, Subscriber};
use utils::epoll::{EpollEvent, EventSet};

use super::device::{
    Balloon, BalloonStat, DFQ_INDEX, FRQ_INDEX, IFQ_INDEX, MAX_STATS_DESC_LEN, PHQ_INDEX,
    STQ_INDEX, VIRTIO_BALLOON_S_AVAIL,
};
use crate::virtio::descriptor_utils::Reader;
use crate::virtio::device::VirtioDevice;

impl Balloon {
    fn queue_event(&self, idx: usize) -> &std::sync::Arc<utils::eventfd::EventFd> {
        &self.queues.as_ref().expect("queues should exist")[idx].event
    }

    pub(crate) fn handle_ifq_event(&mut self, event: &EpollEvent) {
        let event_set = event.event_set();
        if event_set != EventSet::IN {
            warn!("balloon: inflate unexpected event {event_set:?}");
            return;
        }

        if let Err(e) = self.queue_event(IFQ_INDEX).read() {
            error!("Failed to read balloon inflate queue event: {e:?}");
        } else if self.unsupported_queue_has_work(IFQ_INDEX) {
            error!("balloon: unsupported inflate queue event");
        }
    }

    pub(crate) fn handle_dfq_event(&mut self, event: &EpollEvent) {
        let event_set = event.event_set();
        if event_set != EventSet::IN {
            warn!("balloon: deflate unexpected event {event_set:?}");
            return;
        }

        if let Err(e) = self.queue_event(DFQ_INDEX).read() {
            error!("Failed to read balloon deflate queue event: {e:?}");
        } else if self.unsupported_queue_has_work(DFQ_INDEX) {
            error!("balloon: unsupported deflate queue event");
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
        } else {
            self.process_stq();
        }
    }

    pub(crate) fn handle_stats_timer_event(&mut self, event: &EpollEvent) {
        debug!("balloon: stats timer event");

        let event_set = event.event_set();
        if event_set != EventSet::IN {
            warn!("balloon: stats timer unexpected event {event_set:?}");
            return;
        }

        if let Err(e) = self.stats_timer.read() {
            error!("Failed to read balloon stats timer event: {e:?}");
        } else {
            self.trigger_stats_update();
        }
    }

    pub(crate) fn handle_phq_event(&mut self, event: &EpollEvent) {
        let event_set = event.event_set();
        if event_set != EventSet::IN {
            warn!("balloon: page-hinting unexpected event {event_set:?}");
            return;
        }

        if let Err(e) = self.queue_event(PHQ_INDEX).read() {
            error!("Failed to read balloon page-hinting queue event: {e:?}");
        } else if self.unsupported_queue_has_work(PHQ_INDEX) {
            error!("balloon: unsupported page-hinting queue event");
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

    fn unsupported_queue_has_work(&self, index: usize) -> bool {
        let queue = &self.queues.as_ref().expect("queues should exist")[index].queue;
        // Transport resume signals every queue to rescan pending work. A wakeup
        // alone is not a guest request; unconfigured queues have no valid ring
        // to inspect. Do not consume descriptors just to classify the event.
        if !queue.ready {
            return false;
        }
        match &self.device_state {
            crate::virtio::DeviceState::Activated(mem, _) => !queue.is_empty(mem),
            crate::virtio::DeviceState::Inactive => false,
        }
    }

    fn process_stq(&mut self) {
        let mem = match self.device_state {
            crate::virtio::DeviceState::Activated(ref mem, _) => mem,
            crate::virtio::DeviceState::Inactive => unreachable!(),
        };
        let metrics = self.metrics.clone();
        while let Some(head) = {
            let queues = self
                .queues
                .as_mut()
                .expect("queues should exist when activated");
            queues[STQ_INDEX].queue.pop(mem)
        } {
            let index = head.index;

            if let Some(prev_index) = self.stats_desc_index.take() {
                error!("balloon: driver is not compliant, more than one stats buffer received");
                let add_result = {
                    let queues = self
                        .queues
                        .as_mut()
                        .expect("queues should exist when activated");
                    queues[STQ_INDEX].queue.add_used(mem, prev_index, 0)
                };
                if let Err(e) = add_result {
                    error!("balloon: failed to add stale used stats element: {e:?}");
                } else {
                    self.device_state.signal_used_queue();
                }
            }

            match stats_descriptor_len(&head) {
                Some(len) if len <= MAX_STATS_DESC_LEN => {}
                Some(len) => {
                    warn!(
                        "balloon: stats descriptor too large: {} > {}, skipping",
                        len, MAX_STATS_DESC_LEN
                    );
                    self.stats_desc_index = Some(index);
                    self.arm_stats_timer();
                    continue;
                }
                None => {
                    error!("balloon: stats descriptor length overflow");
                    self.stats_desc_index = Some(index);
                    self.arm_stats_timer();
                    continue;
                }
            }

            let mut reader = match Reader::new(mem, head) {
                Ok(reader) => reader,
                Err(e) => {
                    error!("balloon: invalid stats descriptor chain: {e:?}");
                    self.stats_desc_index = Some(index);
                    self.arm_stats_timer();
                    continue;
                }
            };

            while reader.available_bytes() >= std::mem::size_of::<BalloonStat>() {
                match reader.read_obj::<BalloonStat>() {
                    Ok(stat) => {
                        if stat.tag.to_native() == VIRTIO_BALLOON_S_AVAIL {
                            metrics.set_memory_available_bytes(stat.val.to_native());
                        }
                    }
                    Err(e) => {
                        error!("balloon: failed to read stat: {e:?}");
                        break;
                    }
                }
            }

            self.stats_desc_index = Some(index);
            self.arm_stats_timer();
        }
    }

    fn arm_stats_timer(&self) {
        let Some(interval) = self.stats_polling_interval else {
            return;
        };

        if let Err(e) = self.stats_timer.arm_oneshot(interval) {
            error!("balloon: failed to arm stats timer: {e:?}");
        }
    }

    pub(crate) fn trigger_stats_update(&mut self) {
        let Some(index) = self.stats_desc_index.take() else {
            debug!("balloon: stats timer fired without a retained descriptor");
            return;
        };

        let mem = match self.device_state {
            crate::virtio::DeviceState::Activated(ref mem, _) => mem,
            crate::virtio::DeviceState::Inactive => unreachable!(),
        };
        let queues = self
            .queues
            .as_mut()
            .expect("queues should exist when activated");

        if let Err(e) = queues[STQ_INDEX].queue.add_used(mem, index, 0) {
            error!("balloon: failed to add used stats element: {e:?}");
        } else {
            self.device_state.signal_used_queue();
        }
    }

    fn handle_activate_event(&mut self, event_manager: &mut EventManager) {
        debug!("balloon: activate event");
        if let Err(e) = self.activate_evt.read() {
            error!("Failed to consume balloon activate event: {e:?}");
        }

        // The subscriber must exist as we previously registered activate_evt via
        // `interest_list()`.
        let activate_evt = eventfd_pollable(&self.activate_evt);
        let self_subscriber = event_manager.subscriber(activate_evt).unwrap();

        let ifq = eventfd_pollable(self.queue_event(IFQ_INDEX));
        let dfq = eventfd_pollable(self.queue_event(DFQ_INDEX));
        let stq = eventfd_pollable(self.queue_event(STQ_INDEX));
        let phq = eventfd_pollable(self.queue_event(PHQ_INDEX));
        let frq = eventfd_pollable(self.queue_event(FRQ_INDEX));
        let stats_timer = timerfd_pollable(&self.stats_timer);

        event_manager
            .register(ifq, pollable_event(ifq), self_subscriber.clone())
            .unwrap_or_else(|e| {
                error!("Failed to register balloon ifq with event manager: {e:?}");
            });

        event_manager
            .register(dfq, pollable_event(dfq), self_subscriber.clone())
            .unwrap_or_else(|e| {
                error!("Failed to register balloon dfq with event manager: {e:?}");
            });

        if self.stats_enabled() {
            event_manager
                .register(stq, pollable_event(stq), self_subscriber.clone())
                .unwrap_or_else(|e| {
                    error!("Failed to register balloon stq with event manager: {e:?}");
                });

            event_manager
                .register(
                    stats_timer,
                    pollable_event(stats_timer),
                    self_subscriber.clone(),
                )
                .unwrap_or_else(|e| {
                    error!("Failed to register balloon stats timer with event manager: {e:?}");
                });
        }

        event_manager
            .register(phq, pollable_event(phq), self_subscriber.clone())
            .unwrap_or_else(|e| {
                error!("Failed to register balloon dfq with event manager: {e:?}");
            });

        event_manager
            .register(frq, pollable_event(frq), self_subscriber.clone())
            .unwrap_or_else(|e| {
                error!("Failed to register balloon frq with event manager: {e:?}");
            });

        event_manager.unregister(activate_evt).unwrap_or_else(|e| {
            error!("Failed to unregister balloon activate evt: {e:?}");
        })
    }
}

impl Subscriber for Balloon {
    fn process(&mut self, event: &EpollEvent, event_manager: &mut EventManager) {
        let source = event.fd();
        let ifq = eventfd_pollable(self.queue_event(IFQ_INDEX));
        let dfq = eventfd_pollable(self.queue_event(DFQ_INDEX));
        let stq = eventfd_pollable(self.queue_event(STQ_INDEX));
        let phq = eventfd_pollable(self.queue_event(PHQ_INDEX));
        let frq = eventfd_pollable(self.queue_event(FRQ_INDEX));
        let activate_evt = eventfd_pollable(&self.activate_evt);
        let stats_timer = timerfd_pollable(&self.stats_timer);

        if self.is_activated() {
            match source {
                _ if source == ifq => self.handle_ifq_event(event),
                _ if source == dfq => self.handle_dfq_event(event),
                _ if source == stq => self.handle_stq_event(event),
                _ if self.stats_enabled() && source == stats_timer => {
                    self.handle_stats_timer_event(event)
                }
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
        vec![pollable_event(eventfd_pollable(&self.activate_evt))]
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
fn timerfd_pollable(timer: &utils::timerfd::TimerFd) -> Pollable {
    timer.as_raw_fd()
}

#[cfg(windows)]
fn timerfd_pollable(timer: &utils::timerfd::TimerFd) -> Pollable {
    timer.as_raw_handle()
}

fn pollable_event(pollable: Pollable) -> EpollEvent {
    EpollEvent::new(EventSet::IN, pollable_token(pollable))
}

#[cfg(unix)]
fn pollable_token(pollable: Pollable) -> u64 {
    pollable as u64
}

#[cfg(windows)]
fn pollable_token(pollable: Pollable) -> u64 {
    pollable as usize as u64
}

fn stats_descriptor_len(head: &crate::virtio::DescriptorChain<'_>) -> Option<u32> {
    let mut len = 0u32;
    for desc in head.clone().into_iter().readable() {
        len = len.checked_add(desc.len)?;
    }
    Some(len)
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::num::Wrapping;
    use std::sync::{Arc, Once};

    use utils::eventfd::{EventFd, EFD_NONBLOCK};
    use utils::metrics::MetricsWriter;
    use vm_memory::{Bytes, GuestAddress, GuestMemoryMmap};

    use super::*;
    use crate::legacy::DummyIrqChip;
    use crate::virtio::device::DeviceQueue;
    use crate::virtio::{InterruptTransport, Queue};

    struct TestLogger;
    static LOGGER: TestLogger = TestLogger;
    static INIT: Once = Once::new();
    thread_local! {
        // Keep parallel device tests from mixing their diagnostics into ours.
        static LOGS: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
    }

    impl log::Log for TestLogger {
        fn enabled(&self, _: &log::Metadata<'_>) -> bool {
            true
        }
        fn log(&self, record: &log::Record<'_>) {
            LOGS.with(|logs| logs.borrow_mut().push(record.args().to_string()));
        }
        fn flush(&self) {}
    }

    #[test]
    fn unsupported_queues_ignore_empty_wakeups_but_report_real_work() {
        INIT.call_once(|| {
            log::set_logger(&LOGGER).unwrap();
            log::set_max_level(log::LevelFilter::Warn);
        });
        let mem = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x10000)]).unwrap();
        let mut balloon = Balloon::new(MetricsWriter::default(), None).unwrap();
        let interrupt =
            InterruptTransport::new(DummyIrqChip::new().into(), "balloon-test".into()).unwrap();
        let queues = balloon
            .queue_config()
            .iter()
            .map(|config| {
                DeviceQueue::new(
                    Queue::new(config.size),
                    Arc::new(EventFd::new(EFD_NONBLOCK).unwrap()),
                )
            })
            .collect();
        balloon.activate(mem.clone(), interrupt, queues).unwrap();

        type Handler = fn(&mut Balloon, &EpollEvent);
        for (index, name, handler) in [
            (IFQ_INDEX, "inflate", Balloon::handle_ifq_event as Handler),
            (DFQ_INDEX, "deflate", Balloon::handle_dfq_event as Handler),
            (
                PHQ_INDEX,
                "page-hinting",
                Balloon::handle_phq_event as Handler,
            ),
        ] {
            // Include wrapping ring indices: equality, not a nonzero index,
            // determines whether the guest has submitted new work.
            for (ready, consumed, available, expected) in [
                (false, 0, 1, false),
                (true, 0, 0, false),
                (true, u16::MAX, u16::MAX, false),
                (true, u16::MAX, 0, true),
                (true, 0, 1, true),
            ] {
                let queue = &mut balloon.queues.as_mut().unwrap()[index].queue;
                queue.ready = ready;
                queue.avail_ring = GuestAddress(0x1000);
                queue.next_avail = Wrapping(consumed);
                mem.write_obj(available, GuestAddress(0x1002)).unwrap();
                LOGS.with(|logs| logs.borrow_mut().clear());
                balloon.queue_event(index).write(1).unwrap();
                handler(&mut balloon, &EpollEvent::new(EventSet::IN, 0));
                let logs = LOGS.with(|logs| logs.borrow().clone());
                assert_eq!(
                    logs,
                    if expected {
                        vec![format!("balloon: unsupported {name} queue event")]
                    } else {
                        vec![]
                    }
                );
                assert!(
                    balloon.queue_event(index).read().is_err(),
                    "wakeup must be drained"
                );
                assert_eq!(
                    balloon.queues.as_ref().unwrap()[index].queue.next_avail.0,
                    consumed
                );
            }

            LOGS.with(|logs| logs.borrow_mut().clear());
            handler(&mut balloon, &EpollEvent::new(EventSet::IN, 0));
            assert!(LOGS.with(|logs| logs.borrow().iter().any(
                |line| line.starts_with(&format!("Failed to read balloon {name} queue event:"))
            )));
        }
    }
}
