// Copyright 2026 Super Rad Company
// SPDX-License-Identifier: Apache-2.0

use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

// Keep one exact-zero bucket plus one bucket for each power-of-two nanosecond range. This covers
// the full u64 duration domain without choosing workload-specific latency boundaries.
const BLOCK_IO_LATENCY_BUCKETS: usize = 65;

//--------------------------------------------------------------------------------------------------
// Types
//--------------------------------------------------------------------------------------------------

type HostResidentMemorySampler = dyn Fn() -> Option<u64> + Send + Sync + 'static;

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

/// Coarse virtio-blk request class used by diagnostic instrumentation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BlockRequestKind {
    /// Guest read request.
    Read,
    /// Guest write request.
    Write,
    /// Guest flush request.
    Flush,
    /// Any other block request.
    Other,
}

/// Diagnostic snapshot of the virtio-blk request path.
///
/// This is populated only by builds that enable libkrun's `block-io-profile` feature. It is kept
/// separate from [`VmMetrics`] so ordinary metrics sampling does not copy the larger histograms.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct BlockIoProfile {
    /// Number of guest read requests observed.
    pub read_requests: u64,
    /// Number of guest write requests observed.
    pub write_requests: u64,
    /// Number of guest flush requests observed.
    pub flush_requests: u64,
    /// Number of other guest block requests observed.
    pub other_requests: u64,
    /// Number of requests that failed during parsing, execution or completion.
    pub failed_requests: u64,
    /// Number of used-ring completions published.
    pub completions: u64,
    /// Number of used-queue interrupts signalled.
    pub interrupts: u64,
    /// Number of known heap-backed scratch vectors constructed in the request path.
    ///
    /// This is a source-level event count, not a global allocator count. A vector construction may
    /// reuse zero capacity or cause more than one allocator operation while growing.
    pub scratch_vectors: u64,
    /// Time requests spent behind earlier work after the worker began draining the queue.
    pub worker_backlog: LatencyHistogram,
    /// Time spent constructing descriptor readers/writers and decoding the request header.
    pub descriptor_parse: LatencyHistogram,
    /// End-to-end host-side request handling time.
    pub request: LatencyHistogram,
    /// Time spent preparing guest-memory iovecs before entering Imago.
    pub iovec_prepare: LatencyHistogram,
    /// Time spent in Imago format access for reads, including mapping and storage.
    pub format_read: LatencyHistogram,
    /// Time spent in Imago format access for writes, including mapping and storage.
    pub format_write: LatencyHistogram,
    /// Time spent in the underlying storage read implementation.
    pub storage_read: LatencyHistogram,
    /// Time spent in the underlying storage write implementation.
    pub storage_write: LatencyHistogram,
    /// End-to-end guest flush-request latency.
    pub flush: LatencyHistogram,
    /// Time spent in the underlying storage flush implementation.
    pub storage_flush: LatencyHistogram,
    /// Time spent syncing the underlying storage to durable media.
    pub sync: LatencyHistogram,
    /// Time spent publishing status, used-ring state and any interrupt.
    pub completion: LatencyHistogram,
    /// Per-device diagnostic profiles.
    pub devices: Vec<BlockDeviceIoProfile>,
}

/// Diagnostic virtio-blk profile for one device.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct BlockDeviceIoProfile {
    /// VMM block device id.
    pub id: String,
    /// Number of guest read requests observed.
    pub read_requests: u64,
    /// Number of guest write requests observed.
    pub write_requests: u64,
    /// Number of guest flush requests observed.
    pub flush_requests: u64,
    /// Number of other guest block requests observed.
    pub other_requests: u64,
    /// Number of failed requests.
    pub failed_requests: u64,
    /// Number of used-ring completions published.
    pub completions: u64,
    /// Number of used-queue interrupts signalled.
    pub interrupts: u64,
    /// Number of known heap-backed scratch vectors constructed.
    pub scratch_vectors: u64,
    /// Time requests spent behind earlier work in the current queue drain.
    pub worker_backlog: LatencyHistogram,
    /// Descriptor construction and header decoding time.
    pub descriptor_parse: LatencyHistogram,
    /// End-to-end host-side request time.
    pub request: LatencyHistogram,
    /// Guest-memory iovec preparation time.
    pub iovec_prepare: LatencyHistogram,
    /// Imago format-access read time.
    pub format_read: LatencyHistogram,
    /// Imago format-access write time.
    pub format_write: LatencyHistogram,
    /// Underlying storage read time.
    pub storage_read: LatencyHistogram,
    /// Underlying storage write time.
    pub storage_write: LatencyHistogram,
    /// End-to-end guest flush-request time.
    pub flush: LatencyHistogram,
    /// Underlying storage flush time.
    pub storage_flush: LatencyHistogram,
    /// Durable storage sync time.
    pub sync: LatencyHistogram,
    /// Completion publication time.
    pub completion: LatencyHistogram,
}

/// Log2 nanosecond latency histogram.
///
/// Bucket zero contains exact zeroes. Bucket `n > 0` contains values from `2^(n - 1)` through
/// `2^n - 1` nanoseconds. Use [`LatencyHistogram::percentile_upper_bound_ns`] to turn a percentile
/// into a conservative bucket upper bound.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LatencyHistogram {
    /// Number of recorded samples.
    pub count: u64,
    /// Sum of recorded nanoseconds.
    pub total_ns: u64,
    /// Largest recorded sample in nanoseconds.
    pub max_ns: u64,
    /// Log2 nanosecond buckets.
    pub buckets: [u64; BLOCK_IO_LATENCY_BUCKETS],
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
    io_profile_enabled: AtomicU64,
    io_profile: BlockIoProfileState,
}

#[derive(Debug, Default)]
struct BlockIoProfileState {
    read_requests: AtomicU64,
    write_requests: AtomicU64,
    flush_requests: AtomicU64,
    other_requests: AtomicU64,
    failed_requests: AtomicU64,
    completions: AtomicU64,
    interrupts: AtomicU64,
    scratch_vectors: AtomicU64,
    worker_backlog: LatencyHistogramState,
    descriptor_parse: LatencyHistogramState,
    request: LatencyHistogramState,
    iovec_prepare: LatencyHistogramState,
    format_read: LatencyHistogramState,
    format_write: LatencyHistogramState,
    storage_read: LatencyHistogramState,
    storage_write: LatencyHistogramState,
    flush: LatencyHistogramState,
    storage_flush: LatencyHistogramState,
    sync: LatencyHistogramState,
    completion: LatencyHistogramState,
}

#[derive(Debug)]
struct LatencyHistogramState {
    count: AtomicU64,
    total_ns: AtomicU64,
    max_ns: AtomicU64,
    buckets: [AtomicU64; BLOCK_IO_LATENCY_BUCKETS],
}

//--------------------------------------------------------------------------------------------------
// Methods
//--------------------------------------------------------------------------------------------------

impl MetricsHandle {
    /// Return a coherent-enough snapshot of current VM metrics.
    ///
    /// Counters are monotonic atomics. Callers that need rates should compute
    /// deltas between snapshots.
    pub fn snapshot(&self) -> VmMetrics {
        self.snapshot_inner(true)
    }

    /// Return aggregate VM metrics without per-device block details.
    ///
    /// This avoids cloning the per-device vector for high-frequency samplers
    /// that only publish aggregate counters.
    pub fn aggregate_snapshot(&self) -> VmMetrics {
        self.snapshot_inner(false)
    }

    /// Return block-path diagnostics when an instrumented block device is active.
    ///
    /// Normal builds return `None`. Instrumented builds keep this separate from [`Self::snapshot`]
    /// and [`Self::aggregate_snapshot`] so routine samplers do not copy histogram data.
    pub fn block_io_profile(&self) -> Option<BlockIoProfile> {
        let devices = self.state.block_devices.lock().unwrap();
        let profiles = devices
            .iter()
            .filter(|device| device.io_profile_enabled.load(Ordering::Acquire) != 0)
            .map(|device| device.io_profile.snapshot(device.id.clone()))
            .collect::<Vec<_>>();
        if profiles.is_empty() {
            return None;
        }

        let mut aggregate = BlockIoProfile::default();
        for profile in &profiles {
            aggregate.merge_device(profile);
        }
        aggregate.devices = profiles;
        Some(aggregate)
    }

    fn snapshot_inner(&self, include_block_devices: bool) -> VmMetrics {
        let total_bytes = self.state.memory_total_bytes.load(Ordering::Relaxed);
        let available_bytes = valid_value(
            &self.state.memory_available_valid,
            &self.state.memory_available_bytes,
        );
        let host_resident_bytes = self.host_resident_bytes();
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
            io_profile_enabled: AtomicU64::new(0),
            io_profile: BlockIoProfileState::default(),
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

    /// Mark this device as block-I/O-profiled.
    pub fn enable_io_profile(&self) {
        self.device.io_profile_enabled.store(1, Ordering::Release);
    }

    /// Record a guest request by virtio operation class.
    pub fn record_request_kind(&self, kind: BlockRequestKind) {
        let counter = match kind {
            BlockRequestKind::Read => &self.device.io_profile.read_requests,
            BlockRequestKind::Write => &self.device.io_profile.write_requests,
            BlockRequestKind::Flush => &self.device.io_profile.flush_requests,
            BlockRequestKind::Other => &self.device.io_profile.other_requests,
        };
        counter.fetch_add(1, Ordering::Relaxed);
    }

    /// Record a failed guest request.
    pub fn record_failed_request(&self) {
        self.device
            .io_profile
            .failed_requests
            .fetch_add(1, Ordering::Relaxed);
    }

    /// Record one used-ring completion and whether it signalled an interrupt.
    pub fn record_completion(&self, interrupted: bool) {
        self.device
            .io_profile
            .completions
            .fetch_add(1, Ordering::Relaxed);
        if interrupted {
            self.device
                .io_profile
                .interrupts
                .fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Add known heap-backed scratch-vector construction events.
    pub fn add_scratch_vectors(&self, count: u64) {
        self.device
            .io_profile
            .scratch_vectors
            .fetch_add(count, Ordering::Relaxed);
    }

    /// Record worker queue-drain backlog latency.
    pub fn record_worker_backlog_ns(&self, ns: u64) {
        self.device.io_profile.worker_backlog.record(ns);
    }

    /// Record descriptor parsing latency.
    pub fn record_descriptor_parse_ns(&self, ns: u64) {
        self.device.io_profile.descriptor_parse.record(ns);
    }

    /// Record end-to-end request handling latency.
    pub fn record_request_ns(&self, ns: u64) {
        self.device.io_profile.request.record(ns);
    }

    /// Record guest iovec preparation latency.
    pub fn record_iovec_prepare_ns(&self, ns: u64) {
        self.device.io_profile.iovec_prepare.record(ns);
    }

    /// Record Imago format-access read latency.
    pub fn record_format_read_ns(&self, ns: u64) {
        self.device.io_profile.format_read.record(ns);
    }

    /// Record Imago format-access write latency.
    pub fn record_format_write_ns(&self, ns: u64) {
        self.device.io_profile.format_write.record(ns);
    }

    /// Record underlying storage read latency.
    pub fn record_storage_read_ns(&self, ns: u64) {
        self.device.io_profile.storage_read.record(ns);
    }

    /// Record underlying storage write latency.
    pub fn record_storage_write_ns(&self, ns: u64) {
        self.device.io_profile.storage_write.record(ns);
    }

    /// Record flush latency.
    pub fn record_flush_ns(&self, ns: u64) {
        self.device.io_profile.flush.record(ns);
    }

    /// Record underlying storage flush latency.
    pub fn record_storage_flush_ns(&self, ns: u64) {
        self.device.io_profile.storage_flush.record(ns);
    }

    /// Record durable-storage sync latency.
    pub fn record_sync_ns(&self, ns: u64) {
        self.device.io_profile.sync.record(ns);
    }

    /// Record completion-publication latency.
    pub fn record_completion_ns(&self, ns: u64) {
        self.device.io_profile.completion.record(ns);
    }
}

impl BlockIoProfile {
    fn merge_device(&mut self, device: &BlockDeviceIoProfile) {
        self.read_requests = self.read_requests.saturating_add(device.read_requests);
        self.write_requests = self.write_requests.saturating_add(device.write_requests);
        self.flush_requests = self.flush_requests.saturating_add(device.flush_requests);
        self.other_requests = self.other_requests.saturating_add(device.other_requests);
        self.failed_requests = self.failed_requests.saturating_add(device.failed_requests);
        self.completions = self.completions.saturating_add(device.completions);
        self.interrupts = self.interrupts.saturating_add(device.interrupts);
        self.scratch_vectors = self.scratch_vectors.saturating_add(device.scratch_vectors);
        self.worker_backlog.merge(&device.worker_backlog);
        self.descriptor_parse.merge(&device.descriptor_parse);
        self.request.merge(&device.request);
        self.iovec_prepare.merge(&device.iovec_prepare);
        self.format_read.merge(&device.format_read);
        self.format_write.merge(&device.format_write);
        self.storage_read.merge(&device.storage_read);
        self.storage_write.merge(&device.storage_write);
        self.flush.merge(&device.flush);
        self.storage_flush.merge(&device.storage_flush);
        self.sync.merge(&device.sync);
        self.completion.merge(&device.completion);
    }
}

impl LatencyHistogram {
    /// Return the arithmetic mean in nanoseconds, if any sample exists.
    pub fn mean_ns(&self) -> Option<u64> {
        (self.count != 0).then(|| self.total_ns / self.count)
    }

    /// Return a conservative upper bound for percentile `percentile` in `0..=100`.
    pub fn percentile_upper_bound_ns(&self, percentile: u8) -> Option<u64> {
        if self.count == 0 || percentile > 100 {
            return None;
        }
        let rank = self
            .count
            .saturating_mul(u64::from(percentile))
            .saturating_add(99)
            / 100;
        let rank = rank.max(1);
        let mut seen = 0_u64;
        for (index, count) in self.buckets.iter().enumerate() {
            seen = seen.saturating_add(*count);
            if seen >= rank {
                return Some(latency_bucket_upper_bound(index));
            }
        }
        Some(self.max_ns)
    }

    fn merge(&mut self, other: &LatencyHistogram) {
        self.count = self.count.saturating_add(other.count);
        self.total_ns = self.total_ns.saturating_add(other.total_ns);
        self.max_ns = self.max_ns.max(other.max_ns);
        for (target, source) in self.buckets.iter_mut().zip(&other.buckets) {
            *target = target.saturating_add(*source);
        }
    }
}

impl BlockIoProfileState {
    fn snapshot(&self, id: String) -> BlockDeviceIoProfile {
        BlockDeviceIoProfile {
            id,
            read_requests: self.read_requests.load(Ordering::Relaxed),
            write_requests: self.write_requests.load(Ordering::Relaxed),
            flush_requests: self.flush_requests.load(Ordering::Relaxed),
            other_requests: self.other_requests.load(Ordering::Relaxed),
            failed_requests: self.failed_requests.load(Ordering::Relaxed),
            completions: self.completions.load(Ordering::Relaxed),
            interrupts: self.interrupts.load(Ordering::Relaxed),
            scratch_vectors: self.scratch_vectors.load(Ordering::Relaxed),
            worker_backlog: self.worker_backlog.snapshot(),
            descriptor_parse: self.descriptor_parse.snapshot(),
            request: self.request.snapshot(),
            iovec_prepare: self.iovec_prepare.snapshot(),
            format_read: self.format_read.snapshot(),
            format_write: self.format_write.snapshot(),
            storage_read: self.storage_read.snapshot(),
            storage_write: self.storage_write.snapshot(),
            flush: self.flush.snapshot(),
            storage_flush: self.storage_flush.snapshot(),
            sync: self.sync.snapshot(),
            completion: self.completion.snapshot(),
        }
    }
}

impl LatencyHistogramState {
    fn record(&self, ns: u64) {
        self.count.fetch_add(1, Ordering::Relaxed);
        self.total_ns.fetch_add(ns, Ordering::Relaxed);
        self.max_ns.fetch_max(ns, Ordering::Relaxed);
        self.buckets[latency_bucket(ns)].fetch_add(1, Ordering::Relaxed);
    }

    fn snapshot(&self) -> LatencyHistogram {
        LatencyHistogram {
            count: self.count.load(Ordering::Relaxed),
            total_ns: self.total_ns.load(Ordering::Relaxed),
            max_ns: self.max_ns.load(Ordering::Relaxed),
            buckets: std::array::from_fn(|index| self.buckets[index].load(Ordering::Relaxed)),
        }
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

impl Default for LatencyHistogram {
    fn default() -> Self {
        Self {
            count: 0,
            total_ns: 0,
            max_ns: 0,
            buckets: [0; BLOCK_IO_LATENCY_BUCKETS],
        }
    }
}

impl Default for LatencyHistogramState {
    fn default() -> Self {
        Self {
            count: AtomicU64::new(0),
            total_ns: AtomicU64::new(0),
            max_ns: AtomicU64::new(0),
            buckets: std::array::from_fn(|_| AtomicU64::new(0)),
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

fn latency_bucket(ns: u64) -> usize {
    if ns == 0 {
        0
    } else {
        (u64::BITS - ns.leading_zeros()) as usize
    }
}

fn latency_bucket_upper_bound(index: usize) -> u64 {
    match index {
        0 => 0,
        64 => u64::MAX,
        other => (1_u64 << other) - 1,
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
    use super::*;

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
    fn block_io_profile_is_absent_until_enabled() {
        let writer = MetricsWriter::default();
        let root = writer.register_block_device("root".to_string());

        root.record_request_kind(BlockRequestKind::Read);
        assert_eq!(writer.handle().block_io_profile(), None);

        root.enable_io_profile();
        assert_eq!(writer.handle().block_io_profile().unwrap().read_requests, 1);
    }

    #[test]
    fn block_io_profile_aggregates_devices_and_latency_buckets() {
        let writer = MetricsWriter::default();
        let root = writer.register_block_device("root".to_string());
        let data = writer.register_block_device("data".to_string());
        root.enable_io_profile();
        data.enable_io_profile();

        root.record_request_kind(BlockRequestKind::Read);
        root.record_request_ns(1);
        root.record_request_ns(2);
        root.add_scratch_vectors(7);
        data.record_request_kind(BlockRequestKind::Write);
        data.record_request_ns(8);
        data.record_failed_request();

        let profile = writer.handle().block_io_profile().unwrap();
        assert_eq!(profile.read_requests, 1);
        assert_eq!(profile.write_requests, 1);
        assert_eq!(profile.failed_requests, 1);
        assert_eq!(profile.scratch_vectors, 7);
        assert_eq!(profile.request.count, 3);
        assert_eq!(profile.request.total_ns, 11);
        assert_eq!(profile.request.max_ns, 8);
        assert_eq!(profile.request.mean_ns(), Some(3));
        assert_eq!(profile.request.percentile_upper_bound_ns(50), Some(3));
        assert_eq!(profile.request.percentile_upper_bound_ns(100), Some(15));
        assert_eq!(profile.devices.len(), 2);
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
