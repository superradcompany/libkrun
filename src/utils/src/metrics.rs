// Copyright 2026 Super Rad Company
// SPDX-License-Identifier: Apache-2.0

use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

//--------------------------------------------------------------------------------------------------
// Types
//--------------------------------------------------------------------------------------------------

type HostResidentMemorySampler = dyn Fn() -> Option<u64> + Send + Sync + 'static;
type GuestMemoryInvalidator = dyn Fn() + Send + Sync + 'static;

/// Cloneable handle for point-in-time VM metrics.
#[derive(Clone, Debug, Default)]
pub struct MetricsHandle {
    state: Arc<MetricsState>,
}

/// Cloneable writer for VMM and device metrics.
#[derive(Clone, Debug, Default)]
pub struct MetricsWriter {
    state: Arc<MetricsState>,
}

/// Cloneable writer for one block device's metrics.
#[derive(Clone, Debug)]
pub struct BlockMetricsWriter {
    state: Arc<MetricsState>,
    device: Arc<BlockDeviceState>,
}

/// Point-in-time VM metrics.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct VmMetrics {
    /// CPU metrics.
    pub cpu: CpuMetrics,
    /// Memory metrics.
    pub memory: MemoryMetrics,
    /// Block device metrics.
    pub block: BlockMetrics,
    /// Filesystem metrics.
    pub filesystem: FilesystemMetrics,
}

/// CPU metrics.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CpuMetrics {
    /// Cumulative guest vCPU execution time across all vCPUs when available.
    pub vcpu_time_ns: Option<u64>,
}

/// Memory metrics.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct MemoryMetrics {
    /// Configured guest physical memory.
    pub total_bytes: u64,
    /// Guest-available memory when reported by virtio-balloon stats.
    pub available_bytes: Option<u64>,
    /// Derived guest-used memory when available.
    pub used_bytes: Option<u64>,
    /// Host-resident guest memory pages when the VMM can report them.
    pub host_resident_bytes: Option<u64>,
}

/// One coherent guest report; configured allocation remains `MemoryMetrics::total_bytes`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GuestMemoryObservation {
    /// Guest usable total, including only capacity the guest reports usable.
    pub total_bytes: u64,
    /// Guest available memory from the same report.
    pub available_bytes: u64,
    /// Host-wide monotonic receipt time, not the host sampler's read time.
    ///
    /// The clock is `CLOCK_BOOTTIME` on Linux, `CLOCK_MONOTONIC` on other Unix
    /// hosts, and `GetTickCount64` (milliseconds) on Windows. Compute age using
    /// that same host clock; neither an arbitrary `Instant` nor wall time has
    /// a comparable origin. Linux/Windows receipts include host suspend time;
    /// other Unix hosts follow their `CLOCK_MONOTONIC` suspend semantics.
    pub received_at: Duration,
    /// Guest reporting interval, independent of host sampling cadence.
    pub reporting_interval: Duration,
    /// Observation epoch; invalidation rejects responses to earlier requests.
    pub epoch: u64,
}

#[derive(Default)]
struct GuestMemoryState {
    epoch: u64,
    observation: Option<GuestMemoryObservation>,
    invalidators: Vec<Box<GuestMemoryInvalidator>>,
}

/// Aggregate block metrics.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct BlockMetrics {
    /// Successful guest logical bytes read from block devices.
    pub read_bytes: u64,
    /// Successful guest logical bytes written to block devices.
    pub write_bytes: u64,
    /// Per-device block metrics.
    pub devices: Vec<BlockDeviceMetrics>,
}

/// Per-device block metrics.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BlockDeviceMetrics {
    /// VMM block device id.
    pub id: String,
    /// Successful guest logical bytes read from this block device.
    pub read_bytes: u64,
    /// Successful guest logical bytes written to this block device.
    pub write_bytes: u64,
}

/// Filesystem metrics.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct FilesystemMetrics {
    /// Guest-visible used bytes on the microsandbox OCI upper filesystem.
    pub upper_used_bytes: Option<u64>,
    /// Guest-visible bytes available to ordinary allocation on the microsandbox OCI upper filesystem.
    pub upper_free_bytes: Option<u64>,
    /// Unix milliseconds when the upper filesystem sample was received by the host.
    pub upper_sampled_at_unix_ms: Option<u64>,
}

struct MetricsState {
    vcpu_time_ns: AtomicU64,
    vcpu_time_valid: AtomicU64,
    memory_total_bytes: AtomicU64,
    guest_memory: Mutex<GuestMemoryState>,
    memory_available_bytes: AtomicU64,
    memory_available_valid: AtomicU64,
    memory_host_resident_bytes: AtomicU64,
    memory_host_resident_valid: AtomicU64,
    memory_host_resident_sampler: Mutex<Option<Arc<HostResidentMemorySampler>>>,
    block_read_bytes: AtomicU64,
    block_write_bytes: AtomicU64,
    block_devices: Mutex<Vec<Arc<BlockDeviceState>>>,
    filesystem_upper_used_bytes: AtomicU64,
    filesystem_upper_used_valid: AtomicU64,
    filesystem_upper_free_bytes: AtomicU64,
    filesystem_upper_free_valid: AtomicU64,
    filesystem_upper_sampled_at_unix_ms: AtomicU64,
}

#[derive(Debug)]
struct BlockDeviceState {
    id: String,
    read_bytes: AtomicU64,
    write_bytes: AtomicU64,
}

//--------------------------------------------------------------------------------------------------
// Methods
//--------------------------------------------------------------------------------------------------

impl MetricsHandle {
    /// Return the last coherent report without refreshing its receipt time.
    /// Callers apply freshness using the guest cadence and the platform clock
    /// documented on [`GuestMemoryObservation::received_at`].
    pub fn guest_memory_observation(&self) -> Option<GuestMemoryObservation> {
        self.state.guest_memory.lock().unwrap().observation
    }

    /// Publish a report while holding the same lock as lifecycle invalidation.
    ///
    /// Pair this with [`Self::register_guest_memory_invalidator`] for an external
    /// publication. The closure must only perform a short, nonblocking write: it
    /// must not call this metrics handle, VM control, or other code that can acquire
    /// a VMM/device lock. Obtain independent counters and perform I/O beforehand.
    /// Copying a report out of this closure and publishing it later loses the fence.
    pub fn with_guest_memory_observation<R>(
        &self,
        publish: impl FnOnce(Option<GuestMemoryObservation>) -> R,
    ) -> R {
        let state = self.state.guest_memory.lock().unwrap();
        publish(state.observation)
    }

    /// Register a synchronous invalidator for this VM's lifetime.
    ///
    /// Invoked once during registration and whenever observations are invalidated,
    /// before that invalidation returns. Together with
    /// [`Self::with_guest_memory_observation`], this prevents an older publication
    /// from surviving a completed lifecycle boundary. The callback must obey the
    /// same short, nonblocking, non-reentrant rules as the publication closure.
    /// Capture only the destination, not this handle, to avoid an ownership cycle.
    pub fn register_guest_memory_invalidator(&self, invalidate: impl Fn() + Send + Sync + 'static) {
        let mut state = self.state.guest_memory.lock().unwrap();
        invalidate();
        state.invalidators.push(Box::new(invalidate));
    }

    /// Invalidate reports across pause/restore/replacement. Responses to requests
    /// from the prior epoch cannot make the old observation fresh again.
    pub fn invalidate_guest_memory_observation(&self) {
        let mut state = self.state.guest_memory.lock().unwrap();
        state.epoch = state.epoch.wrapping_add(1);
        state.observation = None;
        for invalidate in &state.invalidators {
            invalidate();
        }
    }

    /// Return a coherent-enough snapshot of current VM metrics.
    ///
    /// Counters are monotonic atomics. Callers that need rates should compute
    /// deltas between snapshots.
    pub fn snapshot(&self) -> VmMetrics {
        self.snapshot_inner(true, true)
    }

    /// Return aggregate VM metrics without per-device block details.
    ///
    /// This avoids cloning the per-device vector for high-frequency samplers
    /// that only publish aggregate counters.
    pub fn aggregate_snapshot(&self) -> VmMetrics {
        self.snapshot_inner(false, true)
    }

    /// Return current aggregate counters without querying host page residency.
    ///
    /// `memory.host_resident_bytes` is `None` in this snapshot. Host residency can
    /// change even while a guest is paused, so returning a cached value would
    /// misrepresent it as a current observation. Other counters are read normally.
    pub fn aggregate_snapshot_without_host_residency(&self) -> VmMetrics {
        self.snapshot_inner(false, false)
    }

    fn snapshot_inner(
        &self,
        include_block_devices: bool,
        include_host_residency: bool,
    ) -> VmMetrics {
        let total_bytes = self.state.memory_total_bytes.load(Ordering::Relaxed);
        let available_bytes = valid_value(
            &self.state.memory_available_valid,
            &self.state.memory_available_bytes,
        );
        let host_resident_bytes = if include_host_residency {
            self.host_resident_bytes()
        } else {
            None
        };
        VmMetrics {
            cpu: CpuMetrics {
                vcpu_time_ns: valid_value(&self.state.vcpu_time_valid, &self.state.vcpu_time_ns),
            },
            memory: MemoryMetrics {
                total_bytes,
                available_bytes,
                used_bytes: available_bytes
                    .and_then(|available| total_bytes.checked_sub(available)),
                host_resident_bytes,
            },
            block: BlockMetrics {
                read_bytes: self.state.block_read_bytes.load(Ordering::Relaxed),
                write_bytes: self.state.block_write_bytes.load(Ordering::Relaxed),
                devices: if include_block_devices {
                    self.state
                        .block_devices
                        .lock()
                        .unwrap()
                        .iter()
                        .map(|device| BlockDeviceMetrics {
                            id: device.id.clone(),
                            read_bytes: device.read_bytes.load(Ordering::Relaxed),
                            write_bytes: device.write_bytes.load(Ordering::Relaxed),
                        })
                        .collect()
                } else {
                    Vec::new()
                },
            },
            filesystem: FilesystemMetrics {
                upper_used_bytes: valid_value(
                    &self.state.filesystem_upper_used_valid,
                    &self.state.filesystem_upper_used_bytes,
                ),
                upper_free_bytes: valid_value(
                    &self.state.filesystem_upper_free_valid,
                    &self.state.filesystem_upper_free_bytes,
                ),
                upper_sampled_at_unix_ms: valid_value(
                    &self.state.filesystem_upper_used_valid,
                    &self.state.filesystem_upper_sampled_at_unix_ms,
                ),
            },
        }
    }

    fn host_resident_bytes(&self) -> Option<u64> {
        let sampler = self
            .state
            .memory_host_resident_sampler
            .lock()
            .unwrap()
            .clone();
        if let Some(sampler) = sampler {
            match sampler() {
                Some(bytes) => {
                    self.state
                        .memory_host_resident_bytes
                        .store(bytes, Ordering::Relaxed);
                    self.state
                        .memory_host_resident_valid
                        .store(1, Ordering::Release);
                }
                None => {
                    self.state
                        .memory_host_resident_valid
                        .store(0, Ordering::Release);
                    return None;
                }
            }
        }

        valid_value(
            &self.state.memory_host_resident_valid,
            &self.state.memory_host_resident_bytes,
        )
    }
}

impl MetricsWriter {
    /// Epoch to bind to a guest statistics request before returning its buffer.
    pub fn guest_memory_epoch(&self) -> u64 {
        self.state.guest_memory.lock().unwrap().epoch
    }

    /// Publish a complete guest report with receipt time captured exactly once.
    /// A malformed report invalidates the pair; an earlier epoch cannot overwrite
    /// a later report. Legacy configured/available accounting remains independent.
    pub fn record_guest_memory_observation(
        &self,
        epoch: u64,
        total_bytes: u64,
        available_bytes: u64,
        reporting_interval: Duration,
    ) -> bool {
        let mut state = self.state.guest_memory.lock().unwrap();
        if state.epoch != epoch {
            return false;
        }
        state.observation = host_monotonic_time()
            .filter(|_| {
                total_bytes != 0 && available_bytes <= total_bytes && !reporting_interval.is_zero()
            })
            .map(|received_at| GuestMemoryObservation {
                total_bytes,
                available_bytes,
                received_at,
                reporting_interval,
                epoch,
            });
        if state.observation.is_none() {
            for invalidate in &state.invalidators {
                invalidate();
            }
        }
        state.observation.is_some()
    }

    /// Discard an incomplete or malformed response without changing request identity.
    pub fn discard_guest_memory_observation(&self, epoch: u64) {
        let mut state = self.state.guest_memory.lock().unwrap();
        if state.epoch == epoch {
            state.observation = None;
            for invalidate in &state.invalidators {
                invalidate();
            }
        }
    }

    /// Return a public read handle for this metrics state.
    pub fn handle(&self) -> MetricsHandle {
        MetricsHandle {
            state: Arc::clone(&self.state),
        }
    }

    /// Set configured guest memory.
    pub fn set_memory_total_bytes(&self, bytes: u64) {
        self.state
            .memory_total_bytes
            .store(bytes, Ordering::Relaxed);
    }

    /// Set guest-available memory.
    pub fn set_memory_available_bytes(&self, bytes: u64) {
        self.state
            .memory_available_bytes
            .store(bytes, Ordering::Relaxed);
        self.state
            .memory_available_valid
            .store(1, Ordering::Release);
    }

    /// Set host-resident guest memory.
    pub fn set_memory_host_resident_bytes(&self, bytes: u64) {
        self.state
            .memory_host_resident_bytes
            .store(bytes, Ordering::Relaxed);
        self.state
            .memory_host_resident_valid
            .store(1, Ordering::Release);
    }

    /// Register a sampler that refreshes host-resident guest memory during snapshots.
    pub fn set_memory_host_resident_sampler<F>(&self, sampler: F)
    where
        F: Fn() -> Option<u64> + Send + Sync + 'static,
    {
        *self.state.memory_host_resident_sampler.lock().unwrap() = Some(Arc::new(sampler));
    }

    /// Set guest-visible OCI upper filesystem used/free bytes.
    pub fn set_upper_filesystem_bytes(&self, used_bytes: u64, free_bytes: u64) {
        self.state
            .filesystem_upper_used_bytes
            .store(used_bytes, Ordering::Relaxed);
        self.state
            .filesystem_upper_free_bytes
            .store(free_bytes, Ordering::Relaxed);
        self.state
            .filesystem_upper_sampled_at_unix_ms
            .store(unix_timestamp_ms(), Ordering::Relaxed);
        self.state
            .filesystem_upper_used_valid
            .store(1, Ordering::Release);
        self.state
            .filesystem_upper_free_valid
            .store(1, Ordering::Release);
    }

    /// Clear guest-visible OCI upper filesystem metrics.
    pub fn clear_upper_filesystem_bytes(&self) {
        self.state
            .filesystem_upper_used_valid
            .store(0, Ordering::Release);
        self.state
            .filesystem_upper_free_valid
            .store(0, Ordering::Release);
    }

    /// Add guest vCPU execution time.
    pub fn add_vcpu_time_ns(&self, ns: u64) {
        self.state.vcpu_time_ns.fetch_add(ns, Ordering::Relaxed);
        self.state.vcpu_time_valid.store(1, Ordering::Release);
    }

    /// Register one block device and return a device-scoped metrics writer.
    pub fn register_block_device(&self, id: String) -> BlockMetricsWriter {
        let device = Arc::new(BlockDeviceState {
            id,
            read_bytes: AtomicU64::new(0),
            write_bytes: AtomicU64::new(0),
        });
        self.state
            .block_devices
            .lock()
            .unwrap()
            .push(Arc::clone(&device));
        BlockMetricsWriter {
            state: Arc::clone(&self.state),
            device,
        }
    }
}

impl BlockMetricsWriter {
    /// Add successful guest logical block read bytes.
    pub fn add_read_bytes(&self, bytes: u64) {
        self.state
            .block_read_bytes
            .fetch_add(bytes, Ordering::Relaxed);
        self.device.read_bytes.fetch_add(bytes, Ordering::Relaxed);
    }

    /// Add successful guest logical block write bytes.
    pub fn add_write_bytes(&self, bytes: u64) {
        self.state
            .block_write_bytes
            .fetch_add(bytes, Ordering::Relaxed);
        self.device.write_bytes.fetch_add(bytes, Ordering::Relaxed);
    }
}

//--------------------------------------------------------------------------------------------------
// Trait Implementations
//--------------------------------------------------------------------------------------------------

impl Default for MetricsState {
    fn default() -> Self {
        Self {
            vcpu_time_ns: AtomicU64::new(0),
            vcpu_time_valid: AtomicU64::new(0),
            memory_total_bytes: AtomicU64::new(0),
            guest_memory: Mutex::new(GuestMemoryState::default()),
            memory_available_bytes: AtomicU64::new(0),
            memory_available_valid: AtomicU64::new(0),
            memory_host_resident_bytes: AtomicU64::new(0),
            memory_host_resident_valid: AtomicU64::new(0),
            memory_host_resident_sampler: Mutex::new(None),
            block_read_bytes: AtomicU64::new(0),
            block_write_bytes: AtomicU64::new(0),
            block_devices: Mutex::new(Vec::new()),
            filesystem_upper_used_bytes: AtomicU64::new(0),
            filesystem_upper_used_valid: AtomicU64::new(0),
            filesystem_upper_free_bytes: AtomicU64::new(0),
            filesystem_upper_free_valid: AtomicU64::new(0),
            filesystem_upper_sampled_at_unix_ms: AtomicU64::new(0),
        }
    }
}

impl fmt::Debug for MetricsState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MetricsState").finish_non_exhaustive()
    }
}

//--------------------------------------------------------------------------------------------------
// Functions
//--------------------------------------------------------------------------------------------------

fn valid_value(valid: &AtomicU64, value: &AtomicU64) -> Option<u64> {
    if valid.load(Ordering::Acquire) == 0 {
        None
    } else {
        Some(value.load(Ordering::Relaxed))
    }
}

// Use the same boot-relative clocks as the metrics registry; never substitute a
// wall clock, process-relative Instant, or sampler timestamp for a receipt.
fn host_monotonic_time() -> Option<Duration> {
    #[cfg(unix)]
    {
        #[cfg(target_os = "linux")]
        let clock = libc::CLOCK_BOOTTIME;
        #[cfg(not(target_os = "linux"))]
        let clock = libc::CLOCK_MONOTONIC;
        let mut time = std::mem::MaybeUninit::<libc::timespec>::uninit();
        // SAFETY: clock_gettime initializes the timespec on success.
        if unsafe { libc::clock_gettime(clock, time.as_mut_ptr()) } != 0 {
            return None;
        }
        let time = unsafe { time.assume_init() };
        Some(Duration::new(
            u64::try_from(time.tv_sec).ok()?,
            u32::try_from(time.tv_nsec).ok()?,
        ))
    }
    #[cfg(windows)]
    {
        // SAFETY: GetTickCount64 has no preconditions.
        Some(Duration::from_millis(unsafe {
            windows_sys::Win32::System::SystemInformation::GetTickCount64()
        }))
    }
}

fn unix_timestamp_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

//--------------------------------------------------------------------------------------------------
// Tests
//--------------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::sync::mpsc;
    use std::thread;

    use super::*;

    #[test]
    fn external_publication_finishes_before_lifecycle_invalidation_returns() {
        let writer = MetricsWriter::default();
        let handle = writer.handle();
        let published = Arc::new(AtomicU64::new(999));
        let destination = Arc::clone(&published);
        handle.register_guest_memory_invalidator(move || {
            destination.store(0, Ordering::Release);
        });
        assert_eq!(published.load(Ordering::Acquire), 0);
        assert!(writer.record_guest_memory_observation(0, 8192, 1024, Duration::from_secs(1)));

        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let publisher_handle = handle.clone();
        let destination = Arc::clone(&published);
        let publisher = thread::spawn(move || {
            publisher_handle.with_guest_memory_observation(|report| {
                entered_tx.send(()).unwrap();
                release_rx.recv_timeout(Duration::from_secs(5)).unwrap();
                destination.store(report.unwrap().total_bytes, Ordering::Release);
            });
        });
        entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let (started_tx, started_rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();
        let invalidation_handle = handle.clone();
        let invalidator = thread::spawn(move || {
            started_tx.send(()).unwrap();
            invalidation_handle.invalidate_guest_memory_observation();
            done_tx.send(()).unwrap();
        });
        started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(matches!(
            done_rx.recv_timeout(Duration::from_millis(25)),
            Err(mpsc::RecvTimeoutError::Timeout)
        ));
        release_tx.send(()).unwrap();
        publisher.join().unwrap();
        done_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        invalidator.join().unwrap();
        assert_eq!(published.load(Ordering::Acquire), 0);
        handle.with_guest_memory_observation(|report| assert!(report.is_none()));
        assert!(!writer.record_guest_memory_observation(0, 8192, 1024, Duration::from_secs(1)));
    }

    #[test]
    fn malformed_reports_clear_external_publications_but_old_epochs_do_not() {
        let writer = MetricsWriter::default();
        let handle = writer.handle();
        let invalidations = Arc::new(AtomicU64::new(0));
        for _ in 0..2 {
            let invalidations = Arc::clone(&invalidations);
            handle.register_guest_memory_invalidator(move || {
                invalidations.fetch_add(1, Ordering::AcqRel);
            });
        }
        assert_eq!(invalidations.load(Ordering::Acquire), 2);
        handle.invalidate_guest_memory_observation();
        assert_eq!(invalidations.load(Ordering::Acquire), 4);
        let epoch = writer.guest_memory_epoch();
        assert!(writer.record_guest_memory_observation(epoch, 8192, 1024, Duration::from_secs(1)));
        writer.discard_guest_memory_observation(epoch - 1);
        assert_eq!(invalidations.load(Ordering::Acquire), 4);
        assert!(handle.guest_memory_observation().is_some());
        assert!(!writer.record_guest_memory_observation(epoch, 1024, 8192, Duration::from_secs(1)));
        assert_eq!(invalidations.load(Ordering::Acquire), 6);
        assert!(handle.guest_memory_observation().is_none());
        assert!(writer.record_guest_memory_observation(epoch, 8192, 1024, Duration::from_secs(1)));
        writer.discard_guest_memory_observation(epoch);
        assert_eq!(invalidations.load(Ordering::Acquire), 8);
        assert!(handle.guest_memory_observation().is_none());
    }

    #[test]
    fn guest_pair_receipt_does_not_advance_on_reads_and_keeps_configured_total_distinct() {
        let writer = MetricsWriter::default();
        let handle = writer.handle();
        writer.set_memory_total_bytes(8192);
        assert!(writer.record_guest_memory_observation(0, 8000, 1000, Duration::from_secs(5)));
        let first = handle.guest_memory_observation().unwrap();
        assert_eq!(first.total_bytes, 8000);
        assert_eq!(handle.snapshot().memory.total_bytes, 8192);
        assert_eq!(handle.guest_memory_observation().unwrap(), first);
    }

    #[test]
    fn lifecycle_invalidation_rejects_in_flight_old_responses() {
        let writer = MetricsWriter::default();
        let handle = writer.handle();
        let before = writer.guest_memory_epoch();
        assert!(writer.record_guest_memory_observation(before, 8000, 1000, Duration::from_secs(1)));
        handle.invalidate_guest_memory_observation();
        assert!(handle.guest_memory_observation().is_none());
        assert!(!writer.record_guest_memory_observation(
            before,
            8000,
            2000,
            Duration::from_secs(1)
        ));
        let after = writer.guest_memory_epoch();
        assert_ne!(before, after);
        assert!(writer.record_guest_memory_observation(after, 9000, 3000, Duration::from_secs(1)));
        writer.discard_guest_memory_observation(before);
        assert_eq!(
            handle.guest_memory_observation().unwrap().available_bytes,
            3000
        );
        assert!(!writer.record_guest_memory_observation(after, 1000, 2000, Duration::from_secs(1)));
        assert!(handle.guest_memory_observation().is_none());
    }

    #[test]
    fn concurrent_guest_reports_never_mix_pair_fields_or_refresh_on_read() {
        let writer = MetricsWriter::default();
        let handle = writer.handle();
        let publisher = std::thread::spawn(move || {
            for available in 0..10_000 {
                assert!(writer.record_guest_memory_observation(
                    0,
                    available + 1000,
                    available,
                    Duration::from_secs(1)
                ));
            }
        });
        while !publisher.is_finished() {
            if let Some(report) = handle.guest_memory_observation() {
                assert_eq!(report.total_bytes, report.available_bytes + 1000);
            }
        }
        publisher.join().unwrap();
        assert_eq!(
            handle.guest_memory_observation().unwrap().available_bytes,
            9999
        );
    }

    #[test]
    fn cpu_time_is_unavailable_until_written() {
        let writer = MetricsWriter::default();

        assert_eq!(writer.handle().snapshot().cpu.vcpu_time_ns, None);

        writer.add_vcpu_time_ns(12);

        assert_eq!(writer.handle().snapshot().cpu.vcpu_time_ns, Some(12));
    }

    #[test]
    fn memory_used_is_derived_from_configured_total_and_available() {
        let writer = MetricsWriter::default();

        writer.set_memory_total_bytes(1024);

        let snapshot = writer.handle().snapshot();
        assert_eq!(snapshot.memory.total_bytes, 1024);
        assert_eq!(snapshot.memory.available_bytes, None);
        assert_eq!(snapshot.memory.used_bytes, None);

        writer.set_memory_available_bytes(256);

        let snapshot = writer.handle().snapshot();
        assert_eq!(snapshot.memory.available_bytes, Some(256));
        assert_eq!(snapshot.memory.used_bytes, Some(768));
    }

    #[test]
    fn memory_used_is_unavailable_when_available_exceeds_total() {
        let writer = MetricsWriter::default();

        writer.set_memory_total_bytes(1024);
        writer.set_memory_available_bytes(2048);

        let snapshot = writer.handle().snapshot();
        assert_eq!(snapshot.memory.available_bytes, Some(2048));
        assert_eq!(snapshot.memory.used_bytes, None);
    }

    #[test]
    fn host_resident_sampler_refreshes_snapshot() {
        let writer = MetricsWriter::default();
        let value = Arc::new(AtomicU64::new(4096));
        let sampler_value = Arc::clone(&value);

        writer
            .set_memory_host_resident_sampler(move || Some(sampler_value.load(Ordering::Relaxed)));

        assert_eq!(
            writer.handle().snapshot().memory.host_resident_bytes,
            Some(4096)
        );

        value.store(8192, Ordering::Relaxed);

        assert_eq!(
            writer.handle().snapshot().memory.host_resident_bytes,
            Some(8192)
        );
    }

    #[test]
    fn aggregate_snapshot_can_skip_residency_without_caching_other_counters() {
        let writer = MetricsWriter::default();
        let calls = Arc::new(AtomicU64::new(0));
        let sampler_calls = Arc::clone(&calls);
        writer.set_memory_host_resident_sampler(move || {
            Some((sampler_calls.fetch_add(1, Ordering::Relaxed) + 1) * 4096)
        });
        let handle = writer.handle();
        assert_eq!(
            handle.aggregate_snapshot().memory.host_resident_bytes,
            Some(4096)
        );

        // A skipped scan must not leak the previous residency or freeze unrelated counters.
        writer.set_memory_total_bytes(8192);
        writer.set_memory_available_bytes(2048);
        writer.set_upper_filesystem_bytes(1024, 3072);
        let block = writer.register_block_device("root".into());
        for tick in 1..=2 {
            writer.add_vcpu_time_ns(100);
            block.add_read_bytes(512);
            let sample = handle.aggregate_snapshot_without_host_residency();
            assert_eq!(sample.memory.host_resident_bytes, None);
            assert_eq!(sample.memory.used_bytes, Some(6144));
            assert_eq!(sample.filesystem.upper_used_bytes, Some(1024));
            assert_eq!(sample.cpu.vcpu_time_ns, Some(tick * 100));
            assert_eq!(sample.block.read_bytes, tick * 512);
            assert_eq!(calls.load(Ordering::Relaxed), 1);
        }

        // Normal sampling resumes with a new observation, not the pre-pause cache.
        assert_eq!(
            handle.aggregate_snapshot().memory.host_resident_bytes,
            Some(8192)
        );
        assert_eq!(calls.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn host_resident_sampler_failure_clears_previous_value() {
        let writer = MetricsWriter::default();
        let value = Arc::new(AtomicU64::new(4096));
        let sampler_value = Arc::clone(&value);

        writer.set_memory_host_resident_sampler(move || {
            match sampler_value.load(Ordering::Relaxed) {
                0 => None,
                bytes => Some(bytes),
            }
        });

        assert_eq!(
            writer.handle().snapshot().memory.host_resident_bytes,
            Some(4096)
        );

        value.store(0, Ordering::Relaxed);

        assert_eq!(writer.handle().snapshot().memory.host_resident_bytes, None);
    }

    #[test]
    fn block_metrics_include_aggregate_and_per_device_counters() {
        let writer = MetricsWriter::default();
        let root = writer.register_block_device("root".to_string());
        let data = writer.register_block_device("data".to_string());

        root.add_read_bytes(128);
        root.add_write_bytes(256);
        data.add_read_bytes(512);

        let snapshot = writer.handle().snapshot();
        assert_eq!(snapshot.block.read_bytes, 640);
        assert_eq!(snapshot.block.write_bytes, 256);
        assert_eq!(
            snapshot.block.devices,
            vec![
                BlockDeviceMetrics {
                    id: "root".to_string(),
                    read_bytes: 128,
                    write_bytes: 256,
                },
                BlockDeviceMetrics {
                    id: "data".to_string(),
                    read_bytes: 512,
                    write_bytes: 0,
                },
            ]
        );
    }

    #[test]
    fn aggregate_snapshot_omits_per_device_block_counters() {
        let writer = MetricsWriter::default();
        let root = writer.register_block_device("root".to_string());

        root.add_read_bytes(128);
        root.add_write_bytes(256);

        let snapshot = writer.handle().aggregate_snapshot();
        assert_eq!(snapshot.block.read_bytes, 128);
        assert_eq!(snapshot.block.write_bytes, 256);
        assert!(snapshot.block.devices.is_empty());
    }

    #[test]
    fn filesystem_metrics_are_unavailable_until_written() {
        let writer = MetricsWriter::default();

        assert_eq!(
            writer.handle().snapshot().filesystem,
            FilesystemMetrics::default()
        );

        writer.set_upper_filesystem_bytes(4096, 8192);

        let snapshot = writer.handle().snapshot();
        assert_eq!(snapshot.filesystem.upper_used_bytes, Some(4096));
        assert_eq!(snapshot.filesystem.upper_free_bytes, Some(8192));
        assert!(snapshot.filesystem.upper_sampled_at_unix_ms.is_some());
    }

    #[test]
    fn filesystem_metrics_can_be_cleared() {
        let writer = MetricsWriter::default();

        writer.set_upper_filesystem_bytes(4096, 8192);
        writer.clear_upper_filesystem_bytes();

        assert_eq!(
            writer.handle().snapshot().filesystem,
            FilesystemMetrics::default()
        );
    }
}
