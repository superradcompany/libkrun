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

// How often the host re-requests guest memory stats. One wakeup per second per
// VM is negligible, versus the ~18k wakeups/second an unthrottled stats queue
// caused. Linux-only (paired with the stats TimerFd).
#[cfg(target_os = "linux")]
const STATS_POLL_INTERVAL_SECS: u64 = 1;

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
            return;
        }

        // Read the freshly-reported stats and stash the descriptor index.
        self.process_stats_queue();

        // The buffer must be returned (add_used + signal) to request the next
        // sample. Doing that *here*, on every guest kick, is exactly what caused
        // the idle-vCPU busy-spin: the stats queue is host-paced, so returning
        // the buffer makes the Linux guest's `stats_request` refill and kick
        // again immediately — an unthrottled host<->guest ping-pong that keeps
        // the vCPU out of HLT and pins the `fc_vcpu` thread at ~100% on every
        // running microVM, even idle ones (the 0.1.15 regression). On Linux we
        // instead return the buffer on a periodic timer (handle_stats_timer_event),
        // pacing requests to STATS_POLL_INTERVAL_SECS so memory_available_bytes
        // stays live while the vCPU sleeps between samples. Without a timerfd
        // (non-Linux) we fall back to returning it inline.
        #[cfg(not(target_os = "linux"))]
        {
            if self.return_stats_buffer() {
                self.device_state.signal_used_queue();
            }
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

    // Pop the guest's stats descriptor, publish `memory_available_bytes`, and
    // stash the descriptor index for `return_stats_buffer` to hand back later.
    // Deliberately does NOT add_used — returning the buffer is the throttled
    // step that requests the next sample.
    fn process_stats_queue(&mut self) {
        let mem = match self.device_state {
            crate::virtio::DeviceState::Activated(ref mem, _) => mem,
            crate::virtio::DeviceState::Inactive => return,
        };
        let metrics = self.metrics.clone();
        let queues = self
            .queues
            .as_mut()
            .expect("queues should exist when activated");

        let mut latest_index: Option<u16> = None;
        while let Some(head) = queues[STQ_INDEX].queue.pop(mem) {
            let index = head.index;
            match Reader::new(mem, head) {
                Ok(mut reader) => {
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
                }
                Err(e) => error!("balloon: invalid stats descriptor chain: {e:?}"),
            }
            latest_index = Some(index);
        }

        if latest_index.is_some() {
            self.stats_desc_index = latest_index;
        }
    }

    // Hand the stashed stats buffer back to the guest (add_used), which requests
    // a fresh sample. Returns true if the caller should signal the used queue.
    fn return_stats_buffer(&mut self) -> bool {
        let index = match self.stats_desc_index.take() {
            Some(index) => index,
            None => return false,
        };
        let mem = match self.device_state {
            crate::virtio::DeviceState::Activated(ref mem, _) => mem,
            crate::virtio::DeviceState::Inactive => return false,
        };
        let queues = self
            .queues
            .as_mut()
            .expect("queues should exist when activated");
        if let Err(e) = queues[STQ_INDEX].queue.add_used(mem, index, 0) {
            error!("balloon: failed to add used stats element: {e:?}");
            return false;
        }
        true
    }

    // Timer tick: return the stashed stats buffer to request the next sample.
    // Throttling the return here — instead of on every guest kick — is what
    // stops the idle-vCPU busy-spin while keeping `memory_available_bytes` live.
    #[cfg(target_os = "linux")]
    pub(crate) fn handle_stats_timer_event(&mut self) {
        if let Err(e) = self.stats_timer.wait() {
            error!("balloon: failed to read stats timer: {e:?}");
        }
        if self.return_stats_buffer() {
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

        // Arm + register the periodic stats timer (Linux only) when the guest
        // negotiated the stats queue. The timer paces stats requests so the vCPU
        // sleeps between samples instead of busy-spinning (see handle_stq_event).
        #[cfg(target_os = "linux")]
        if self.acked_features & (1u64 << super::defs::uapi::VIRTIO_BALLOON_F_STATS_VQ as u64) != 0
        {
            let interval = std::time::Duration::from_secs(STATS_POLL_INTERVAL_SECS);
            if let Err(e) = self.stats_timer.reset(interval, Some(interval)) {
                error!("balloon: failed to arm stats timer: {e:?}");
            }
            let timer_fd = self.stats_timer.as_raw_fd();
            event_manager
                .register(
                    timer_fd,
                    EpollEvent::new(EventSet::IN, timer_fd as u64),
                    self_subscriber.clone(),
                )
                .unwrap_or_else(|e| {
                    error!("Failed to register balloon stats timer with event manager: {e:?}");
                });
        }

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
        #[cfg(target_os = "linux")]
        let stats_timer = self.stats_timer.as_raw_fd();

        if self.is_activated() {
            match source {
                _ if source == ifq => self.handle_ifq_event(event),
                _ if source == dfq => self.handle_dfq_event(event),
                _ if source == stq => self.handle_stq_event(event),
                _ if source == phq => self.handle_phq_event(event),
                _ if source == frq => self.handle_frq_event(event),
                #[cfg(target_os = "linux")]
                _ if source == stats_timer => self.handle_stats_timer_event(),
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
