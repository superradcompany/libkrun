#[cfg(unix)]
use std::os::unix::io::AsRawFd;
#[cfg(windows)]
use std::os::windows::io::AsRawHandle;
use std::sync::atomic::Ordering;

use polly::event_manager::{EventManager, Pollable, Subscriber};
use utils::epoll::{EpollEvent, EventSet};
use vm_memory::{Address, Bytes};

use super::device::{
    Balloon, BalloonStat, DFQ_INDEX, FRQ_INDEX, IFQ_INDEX, MAX_STATS_DESC_LEN, PHQ_INDEX, STQ_INDEX,
};
use super::stats::MemoryStats;
use crate::virtio::descriptor_utils::Reader;
use crate::virtio::device::VirtioDevice;
use crate::virtio::queue::Error as QueueError;

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
        let crate::virtio::DeviceState::Activated(mem, _) = &self.device_state else {
            return false;
        };
        // Unsupported queues have not had their guest-controlled ring addresses
        // validated. Queue::is_empty unwraps read errors, so use a checked atomic
        // read of virtq_avail->idx instead, preserving its acquire ordering.
        let available = queue
            .avail_ring
            .checked_add(2)
            .ok_or(QueueError::AddressOverflow)
            .and_then(|addr| {
                mem.load::<u16>(addr, Ordering::Acquire)
                    .map_err(QueueError::GuestMemory)
            });
        match available {
            Ok(available) => available != queue.next_avail.0,
            Err(err) => {
                error!("Failed to read balloon queue {index} available index: {err:?}");
                false
            }
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
mod observation_tests {
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

            // Unsupported rings are guest-controlled and need not be valid,
            // even when marked ready. Invalid notifications must be drained
            // and diagnosed without panicking or changing queue ownership.
            for (ready, address) in [
                (false, u64::MAX - 1),
                (true, u64::MAX - 1), // Adding the index offset overflows.
                (true, 0x10000),      // The ring lies outside guest memory.
                (true, 0xfffd),       // The index crosses the memory boundary.
                (true, 0x1001),       // Atomic ring-index reads need alignment.
            ] {
                let queue = &mut balloon.queues.as_mut().unwrap()[index].queue;
                queue.ready = ready;
                queue.avail_ring = GuestAddress(address);
                queue.next_avail = Wrapping(7);
                LOGS.with(|logs| logs.borrow_mut().clear());
                balloon.queue_event(index).write(1).unwrap();
                handler(&mut balloon, &EpollEvent::new(EventSet::IN, 0));
                let logs = LOGS.with(|logs| logs.borrow().clone());
                if ready {
                    assert_eq!(logs.len(), 1);
                    assert!(logs[0].starts_with(&format!(
                        "Failed to read balloon queue {index} available index:"
                    )));
                } else {
                    assert!(logs.is_empty());
                }
                assert!(balloon.queue_event(index).read().is_err());
                assert_eq!(
                    balloon.queues.as_ref().unwrap()[index].queue.next_avail.0,
                    7
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
