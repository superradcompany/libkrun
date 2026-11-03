#[cfg(unix)]
use std::os::unix::io::AsRawFd;
#[cfg(windows)]
use std::os::windows::io::AsRawHandle;

use polly::event_manager::{EventManager, Pollable, Subscriber};
use utils::epoll::{EpollEvent, EventSet};

use super::device::{
    Balloon, BalloonStat, DFQ_INDEX, FRQ_INDEX, IFQ_INDEX, MAX_STATS_DESC_LEN, PHQ_INDEX, STQ_INDEX,
};
use super::stats::MemoryStats;
use crate::virtio::descriptor_utils::Reader;
use crate::virtio::device::VirtioDevice;

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
            // One host request authorizes exactly one response, including malformed
            // responses. Extra unsolicited buffers cannot refresh the receipt.
            let request_epoch = self.stats_request_epoch.take();

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
                    if let Some(epoch) = request_epoch {
                        metrics.discard_guest_memory_observation(epoch);
                    }
                    self.stats_desc_index = Some(index);
                    self.arm_stats_timer();
                    continue;
                }
                None => {
                    error!("balloon: stats descriptor length overflow");
                    if let Some(epoch) = request_epoch {
                        metrics.discard_guest_memory_observation(epoch);
                    }
                    self.stats_desc_index = Some(index);
                    self.arm_stats_timer();
                    continue;
                }
            }

            let mut reader = match Reader::new(mem, head) {
                Ok(reader) => reader,
                Err(e) => {
                    error!("balloon: invalid stats descriptor chain: {e:?}");
                    if let Some(epoch) = request_epoch {
                        metrics.discard_guest_memory_observation(epoch);
                    }
                    self.stats_desc_index = Some(index);
                    self.arm_stats_timer();
                    continue;
                }
            };

            let mut stats = MemoryStats::default();
            while reader.available_bytes() >= std::mem::size_of::<BalloonStat>() {
                match reader.read_obj::<BalloonStat>() {
                    Ok(stat) => stats.push(stat.tag.to_native(), stat.val.to_native()),
                    Err(error) => {
                        error!("balloon: failed to read stat: {error:?}");
                        stats.invalidate();
                        break;
                    }
                }
            }
            if reader.available_bytes() != 0 {
                stats.invalidate();
            }
            stats.publish(&metrics, request_epoch, self.stats_polling_interval);

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
        // Timer and queue events can be dispatched in either order. Drain an
        // already available report before authorizing another response: it was
        // produced before this request and must not receive a fresh receipt.
        // process_stq rearms the timer; defer the new request to that next tick
        // so returning stale descriptors cannot overlap a new exchange here.
        if let crate::virtio::DeviceState::Activated(ref mem, _) = self.device_state {
            let queues = self
                .queues
                .as_ref()
                .expect("queues should exist when activated");
            if !queues[STQ_INDEX].queue.is_empty(mem) {
                self.process_stq();
                return;
            }
        }

        self.return_stats_descriptor(Some(self.metrics.guest_memory_epoch()));
    }

    /// Release retained queue ownership. Quiescence passes no request epoch:
    /// returning its buffer must not authorize fresh telemetry after restore.
    pub(crate) fn return_stats_descriptor(&mut self, request_epoch: Option<u64>) {
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

        // Bind the next response to the current lifecycle epoch before waking
        // the guest. An in-flight pre-pause buffer keeps its earlier epoch.
        self.stats_request_epoch = request_epoch;
        if let Err(e) = queues[STQ_INDEX].queue.add_used(mem, index, 0) {
            self.stats_request_epoch = None;
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

//--------------------------------------------------------------------------------------------------
// Tests
//--------------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use utils::eventfd::EventFd;
    use utils::metrics::MetricsWriter;
    use vm_memory::{Bytes, GuestAddress, GuestMemoryMmap};

    use crate::legacy::DummyIrqChip;
    use crate::virtio::queue::tests::VirtQueue;
    use crate::virtio::{DeviceQueue, InterruptTransport, Queue};

    use super::*;

    #[test]
    fn actual_stats_queue_requires_one_request_per_coherent_response() {
        let memory = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x10000)]).unwrap();
        let ring = VirtQueue::new(GuestAddress(0), &memory, 16);
        let metrics = MetricsWriter::default();
        let mut balloon = Balloon::new(metrics.clone(), Some(Duration::from_secs(1))).unwrap();
        let mut queues: Vec<_> = (0..super::super::defs::NUM_QUEUES)
            .map(|_| DeviceQueue::new(Queue::new(16), Arc::new(EventFd::new(0).unwrap())))
            .collect();
        queues[STQ_INDEX].queue = ring.create_queue();
        balloon
            .activate(
                memory.clone(),
                InterruptTransport::new(DummyIrqChip::new().into(), "balloon-test".into()).unwrap(),
                queues,
            )
            .unwrap();
        let submit = |index: u16, available: u64, length: u32| {
            for (offset, tag, value) in [(0, 5u16, 8192u64), (10, 6, available)] {
                memory
                    .write_obj(
                        BalloonStat {
                            tag: tag.into(),
                            val: value.into(),
                        },
                        GuestAddress(0x1000 + offset),
                    )
                    .unwrap();
            }
            ring.dtable[index as usize].set(0x1000, length, 0, 0);
            ring.avail.ring[index as usize].set(index);
            ring.avail.idx.set(index + 1);
        };

        // The first buffer may come from a restored queue and has no host request.
        submit(0, 1000, 20);
        balloon.process_stq();
        assert!(metrics.handle().guest_memory_observation().is_none());
        // The timer may run before a queued unsolicited buffer's event is dispatched.
        // That old buffer must not consume a new request or acquire a fresh receipt.
        submit(1, 1500, 20);
        balloon.trigger_stats_update();
        balloon.process_stq();
        assert!(metrics.handle().guest_memory_observation().is_none());
        assert!(balloon.stats_request_epoch.is_none());
        balloon.trigger_stats_update();
        submit(2, 2000, 20);
        balloon.process_stq();
        let accepted = metrics.handle().guest_memory_observation().unwrap();
        assert_eq!(accepted.available_bytes, 2000);

        // A noncompliant extra buffer must not inherit the previous request.
        submit(3, 3000, 20);
        balloon.process_stq();
        assert_eq!(metrics.handle().guest_memory_observation(), Some(accepted));

        // A malformed response consumes its request too; a later unsolicited
        // valid buffer cannot turn that failed exchange into a fresh report.
        balloon.trigger_stats_update();
        submit(4, 4000, 19);
        balloon.process_stq();
        assert!(metrics.handle().guest_memory_observation().is_none());
        submit(5, 5000, 20);
        balloon.process_stq();
        assert!(metrics.handle().guest_memory_observation().is_none());

        // Crossing a lifecycle boundary rejects a response already in flight.
        balloon.trigger_stats_update();
        metrics.handle().invalidate_guest_memory_observation();
        submit(6, 6000, 20);
        balloon.process_stq();
        assert!(metrics.handle().guest_memory_observation().is_none());
        balloon.trigger_stats_update();
        submit(7, 7000, 20);
        // A response to the already outstanding request remains valid when its
        // queue event loses the dispatch race to the timer.
        balloon.trigger_stats_update();
        assert!(balloon.stats_request_epoch.is_none());
        assert_eq!(
            metrics
                .handle()
                .guest_memory_observation()
                .unwrap()
                .available_bytes,
            7000
        );

        // Quiescence must release the retained descriptor even with an extra
        // report queued. A timer-style deferral would strand consumed ownership.
        let used_before = ring.used.idx.get();
        submit(8, 7500, 20);
        let captured = balloon.quiesce().unwrap();
        assert_eq!(ring.used.idx.get(), used_before + 1);
        assert_eq!(captured[STQ_INDEX].queue.len(&memory), 1);
        assert!(balloon.stats_desc_index.is_none());
        assert!(balloon.stats_request_epoch.is_none());
    }
}
