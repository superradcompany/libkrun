// Copyright 2026 Super Rad Company.
// SPDX-License-Identifier: Apache-2.0

//! Request-scoped host access epochs for guest memory.

use std::fmt::{Display, Formatter};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, Weak};
use std::time::{Duration, Instant};

const MODE_MASK: u64 = 0b11;
const MODE_RESIDENT: u64 = 0;
const MODE_TRACKING: u64 = 1;
const MODE_FROZEN: u64 = 2;
const GENERATION_SHIFT: u32 = 2;

/// One non-empty guest-physical range written by a host worker.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HostMemoryRange {
    /// First guest-physical byte.
    pub start: u64,
    /// Number of bytes in the range.
    pub length: u64,
}

/// Access mode selected once at a request or bounded worker-batch boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MemoryAccessMode {
    /// Fully resident direct access without dirty-range construction.
    Resident,
    /// Fully resident direct access with mark-before-expose host writes.
    Tracking { generation: u64 },
    /// New requests are denied while existing requests drain.
    Frozen { generation: u64 },
}

/// Errors while changing the shared guest-memory access epoch.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    /// A participant did not finish its admitted requests before the deadline.
    DrainTimeout,
    /// The access generation counter is exhausted.
    GenerationExhausted,
}

impl Display for Error {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DrainTimeout => write!(f, "guest-memory access requests did not drain"),
            Self::GenerationExhausted => write!(f, "guest-memory access generation is exhausted"),
        }
    }
}

impl std::error::Error for Error {}

/// Shared access epoch used by every virtqueue attached to one VM.
#[derive(Clone)]
pub struct MemoryAccessDomain {
    inner: Arc<MemoryAccessDomainInner>,
}

struct MemoryAccessDomainInner {
    state: AtomicU64,
    dirty: Mutex<Vec<HostMemoryRange>>,
    participants: Mutex<Vec<Weak<MemoryAccessParticipantInner>>>,
    mode_lock: Mutex<()>,
    mode_changed: Condvar,
}

/// Queue-local participant. Its active count is not globally contended by unrelated queues.
#[derive(Clone)]
pub struct MemoryAccessParticipant {
    domain: MemoryAccessDomain,
    inner: Arc<MemoryAccessParticipantInner>,
}

impl std::fmt::Debug for MemoryAccessParticipant {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemoryAccessParticipant")
            .field("mode", &self.domain.mode())
            .finish_non_exhaustive()
    }
}

impl PartialEq for MemoryAccessParticipant {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }
}

impl Eq for MemoryAccessParticipant {}

struct MemoryAccessParticipantInner {
    active: AtomicUsize,
    drained_lock: Mutex<()>,
    drained: Condvar,
}

impl MemoryAccessDomain {
    /// Creates a fully resident access domain with tracking disabled.
    pub fn new() -> Self {
        Self {
            inner: Arc::new(MemoryAccessDomainInner {
                state: AtomicU64::new(encode_state(MODE_RESIDENT, 0)),
                dirty: Mutex::new(Vec::new()),
                participants: Mutex::new(Vec::new()),
                mode_lock: Mutex::new(()),
                mode_changed: Condvar::new(),
            }),
        }
    }

    /// Registers one independently draining worker queue.
    pub fn register_participant(&self) -> MemoryAccessParticipant {
        let inner = Arc::new(MemoryAccessParticipantInner {
            active: AtomicUsize::new(0),
            drained_lock: Mutex::new(()),
            drained: Condvar::new(),
        });
        self.inner
            .participants
            .lock()
            .expect("memory-access participant mutex poisoned")
            .push(Arc::downgrade(&inner));
        MemoryAccessParticipant {
            domain: self.clone(),
            inner,
        }
    }

    /// Returns the current immutable mode selection.
    pub fn mode(&self) -> MemoryAccessMode {
        decode_state(self.inner.state.load(Ordering::Acquire))
    }

    /// Denies new requests and waits for every previously admitted request to finish.
    pub fn freeze(&self, timeout: Duration) -> Result<MemoryAccessMode, Error> {
        let previous_state = self.inner.state.load(Ordering::Acquire);
        let previous = decode_state(previous_state);
        let generation = next_generation(previous_state)?;
        self.inner
            .state
            .store(encode_state(MODE_FROZEN, generation), Ordering::Release);

        let deadline = Instant::now() + timeout;
        let participants = {
            let mut registrations = self
                .inner
                .participants
                .lock()
                .expect("memory-access participant mutex poisoned");
            registrations.retain(|participant| participant.strong_count() != 0);
            registrations
                .iter()
                .filter_map(Weak::upgrade)
                .collect::<Vec<_>>()
        };
        for participant in participants {
            let mut guard = participant
                .drained_lock
                .lock()
                .expect("memory-access drain mutex poisoned");
            while participant.active.load(Ordering::Acquire) != 0 {
                let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                    self.resume_mode(previous);
                    return Err(Error::DrainTimeout);
                };
                let (next, timeout) = participant
                    .drained
                    .wait_timeout(guard, remaining)
                    .expect("memory-access drain mutex poisoned");
                guard = next;
                if timeout.timed_out() && participant.active.load(Ordering::Acquire) != 0 {
                    drop(guard);
                    self.resume_mode(previous);
                    return Err(Error::DrainTimeout);
                }
            }
        }
        Ok(previous)
    }

    /// Opens a new tracked request generation after a frozen boundary.
    pub fn begin_tracking(&self) -> Result<u64, Error> {
        let generation = next_generation(self.inner.state.load(Ordering::Acquire))?;
        let _guard = self
            .inner
            .mode_lock
            .lock()
            .expect("memory-access mode mutex poisoned");
        self.inner
            .dirty
            .lock()
            .expect("host dirty mutex poisoned")
            .clear();
        self.inner
            .state
            .store(encode_state(MODE_TRACKING, generation), Ordering::Release);
        self.inner.mode_changed.notify_all();
        Ok(generation)
    }

    /// Returns to direct resident access after a frozen boundary.
    pub fn resume_resident(&self) -> Result<u64, Error> {
        let generation = next_generation(self.inner.state.load(Ordering::Acquire))?;
        let _guard = self
            .inner
            .mode_lock
            .lock()
            .expect("memory-access mode mutex poisoned");
        self.inner
            .dirty
            .lock()
            .expect("host dirty mutex poisoned")
            .clear();
        self.inner
            .state
            .store(encode_state(MODE_RESIDENT, generation), Ordering::Release);
        self.inner.mode_changed.notify_all();
        Ok(generation)
    }

    /// Reopens the preceding mode after an abandoned frozen operation.
    pub fn resume_mode(&self, mode: MemoryAccessMode) {
        let kind = match mode {
            MemoryAccessMode::Resident => MODE_RESIDENT,
            MemoryAccessMode::Tracking { .. } => MODE_TRACKING,
            MemoryAccessMode::Frozen { .. } => MODE_FROZEN,
        };
        let _guard = self
            .inner
            .mode_lock
            .lock()
            .expect("memory-access mode mutex poisoned");
        let current_generation = self.inner.state.load(Ordering::Acquire) >> GENERATION_SHIFT;
        self.inner
            .state
            .store(encode_state(kind, current_generation), Ordering::Release);
        self.inner.mode_changed.notify_all();
    }

    /// Drains the coalescible host-dirty inventory at a frozen boundary.
    pub fn take_dirty_ranges(&self) -> Vec<HostMemoryRange> {
        std::mem::take(&mut *self.inner.dirty.lock().expect("host dirty mutex poisoned"))
    }
}

impl Default for MemoryAccessDomain {
    fn default() -> Self {
        Self::new()
    }
}

impl MemoryAccessParticipant {
    /// Admits one request and marks its complete writable inventory before memory is exposed.
    ///
    /// `writable_ranges` is invoked only in tracking mode, keeping the resident path free of range
    /// collection and dirty-map locking.
    pub fn begin_request<F>(&self, writable_ranges: F) -> bool
    where
        F: FnOnce() -> Vec<HostMemoryRange>,
    {
        let state = self.domain.inner.state.load(Ordering::Acquire);
        if state & MODE_MASK == MODE_FROZEN {
            return false;
        }
        self.inner.active.fetch_add(1, Ordering::AcqRel);
        if self.domain.inner.state.load(Ordering::Acquire) != state {
            self.end_request();
            return false;
        }
        if state & MODE_MASK == MODE_TRACKING {
            self.domain
                .inner
                .dirty
                .lock()
                .expect("host dirty mutex poisoned")
                .extend(writable_ranges());
        }
        true
    }

    /// Completes one previously admitted request.
    pub fn end_request(&self) {
        if self.inner.active.fetch_sub(1, Ordering::AcqRel) == 1 {
            let _guard = self
                .inner
                .drained_lock
                .lock()
                .expect("memory-access drain mutex poisoned");
            self.inner.drained.notify_all();
        }
    }

    /// Marks an additional writable range discovered after request decoding but before exposure.
    ///
    /// This covers semantic mutations such as balloon or virtio-mem page disposition changes that
    /// are not represented by writable descriptors. The caller must hold an admitted request.
    pub fn mark_write(&self, range: HostMemoryRange) -> bool {
        if range.length == 0 || self.inner.active.load(Ordering::Acquire) == 0 {
            return false;
        }
        if !matches!(self.domain.mode(), MemoryAccessMode::Resident) {
            self.domain
                .inner
                .dirty
                .lock()
                .expect("host dirty mutex poisoned")
                .push(range);
        }
        true
    }

    /// Whether new request admission is currently frozen.
    pub fn is_frozen(&self) -> bool {
        matches!(self.domain.mode(), MemoryAccessMode::Frozen { .. })
    }

    /// Waits until a frozen snapshot boundary reopens host access.
    ///
    /// Queue workers use this after consuming a guest kick so thaw cannot strand the already
    /// available descriptor or leave queue notifications disabled without another wakeup.
    pub fn wait_until_thawed(&self) {
        let mut guard = self
            .domain
            .inner
            .mode_lock
            .lock()
            .expect("memory-access mode mutex poisoned");
        while self.is_frozen() {
            guard = self
                .domain
                .inner
                .mode_changed
                .wait(guard)
                .expect("memory-access mode mutex poisoned");
        }
    }
}

fn encode_state(mode: u64, generation: u64) -> u64 {
    (generation << GENERATION_SHIFT) | mode
}

fn next_generation(state: u64) -> Result<u64, Error> {
    (state >> GENERATION_SHIFT)
        .checked_add(1)
        .filter(|generation| *generation <= u64::MAX >> GENERATION_SHIFT)
        .ok_or(Error::GenerationExhausted)
}

fn decode_state(state: u64) -> MemoryAccessMode {
    let generation = state >> GENERATION_SHIFT;
    match state & MODE_MASK {
        MODE_RESIDENT => MemoryAccessMode::Resident,
        MODE_TRACKING => MemoryAccessMode::Tracking { generation },
        MODE_FROZEN => MemoryAccessMode::Frozen { generation },
        _ => unreachable!("two-bit access mode is exhaustive"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tracking_marks_once_per_admitted_request() {
        let domain = MemoryAccessDomain::new();
        let participant = domain.register_participant();
        let previous = domain.freeze(Duration::from_millis(10)).unwrap();
        assert_eq!(previous, MemoryAccessMode::Resident);
        domain.begin_tracking().unwrap();

        assert!(participant.begin_request(|| {
            vec![HostMemoryRange {
                start: 0x1000,
                length: 0x2000,
            }]
        }));
        participant.end_request();
        domain.freeze(Duration::from_millis(10)).unwrap();

        assert_eq!(
            domain.take_dirty_ranges(),
            vec![HostMemoryRange {
                start: 0x1000,
                length: 0x2000,
            }]
        );
    }

    #[test]
    fn freeze_denies_new_requests_and_waits_for_completion() {
        let domain = MemoryAccessDomain::new();
        let participant = domain.register_participant();
        assert!(participant.begin_request(Vec::new));
        assert_eq!(
            domain.freeze(Duration::from_millis(1)),
            Err(Error::DrainTimeout)
        );
        // A failed freeze rolls back to the preceding mode because no boundary was established.
        assert!(participant.begin_request(Vec::new));
        participant.end_request();
        participant.end_request();
        assert!(domain.freeze(Duration::from_millis(10)).is_ok());
    }

    #[test]
    fn frozen_worker_is_released_when_access_reopens() {
        let domain = MemoryAccessDomain::new();
        let participant = domain.register_participant();
        domain.freeze(Duration::from_millis(10)).unwrap();
        let (ready_sender, ready_receiver) = std::sync::mpsc::channel();

        let worker = std::thread::spawn(move || {
            ready_sender.send(()).unwrap();
            participant.wait_until_thawed();
            assert!(participant.begin_request(Vec::new));
            participant.end_request();
        });
        ready_receiver.recv().unwrap();

        domain.begin_tracking().unwrap();
        worker.join().unwrap();
    }

    #[test]
    fn resident_mode_does_not_construct_dirty_inventory() {
        let domain = MemoryAccessDomain::new();
        let participant = domain.register_participant();
        assert!(participant.begin_request(|| panic!("resident mode built dirty ranges")));
        participant.end_request();
        assert!(domain.take_dirty_ranges().is_empty());
    }

    #[test]
    fn decoded_semantic_write_is_marked_before_request_completion() {
        let domain = MemoryAccessDomain::new();
        let participant = domain.register_participant();
        domain.freeze(Duration::from_millis(10)).unwrap();
        domain.begin_tracking().unwrap();

        assert!(participant.begin_request(Vec::new));
        assert!(participant.mark_write(HostMemoryRange {
            start: 0x8000,
            length: 0x1000,
        }));
        participant.end_request();
        domain.freeze(Duration::from_millis(10)).unwrap();

        assert_eq!(
            domain.take_dirty_ranges(),
            vec![HostMemoryRange {
                start: 0x8000,
                length: 0x1000,
            }]
        );
    }

    #[test]
    fn generation_exhaustion_does_not_wrap_the_access_epoch() {
        let domain = MemoryAccessDomain::new();
        domain.inner.state.store(
            encode_state(MODE_RESIDENT, u64::MAX >> GENERATION_SHIFT),
            Ordering::Release,
        );
        assert_eq!(
            domain.freeze(Duration::from_millis(10)),
            Err(Error::GenerationExhausted)
        );
    }
}
