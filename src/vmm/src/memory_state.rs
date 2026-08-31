// Copyright 2026 Super Rad Company.
// SPDX-License-Identifier: Apache-2.0

//! Backend-neutral memory generation and incremental-baseline contracts.
//!
//! Hypervisor dirty trackers produce changed guest-physical ranges. This module validates and
//! coalesces those ranges, scopes retained baselines to one tracker and memory topology, and keeps
//! baseline advancement explicit so callers can publish durable state before accepting a new base.

use std::fmt::{Display, Formatter};
use std::io;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_TRACKER_ID: AtomicU64 = AtomicU64::new(1);

/// Identifies one immutable guest-memory topology within a running VMM.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct MemoryTopologyGeneration(u64);

impl MemoryTopologyGeneration {
    /// Constructs a topology generation from the VMM-owned monotonic value.
    pub fn new(value: u64) -> Self {
        Self(value)
    }

    /// Returns the numeric topology generation.
    pub fn get(self) -> u64 {
        self.0
    }
}

/// Identifies one candidate or published memory-content generation.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct MemoryGeneration(u64);

impl MemoryGeneration {
    /// Returns the numeric memory generation.
    pub fn get(self) -> u64 {
        self.0
    }
}

/// One non-empty guest-physical byte range.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GuestMemoryRange {
    start: u64,
    length: u64,
}

impl GuestMemoryRange {
    /// Constructs a checked, non-empty guest-physical range.
    pub fn new(start: u64, length: u64) -> Result<Self> {
        if length == 0 {
            return Err(Error::EmptyRange);
        }
        start.checked_add(length).ok_or(Error::RangeOverflow)?;
        Ok(Self { start, length })
    }

    /// Returns the first guest-physical byte in the range.
    pub fn start(self) -> u64 {
        self.start
    }

    /// Returns the number of bytes in the range.
    pub fn length(self) -> u64 {
        self.length
    }

    /// Returns the exclusive range end.
    pub fn end(self) -> u64 {
        self.start + self.length
    }
}

/// Runtime-local capability naming the latest successfully published memory baseline.
///
/// Tokens are valid only for the ledger that issued them and while that ledger retains continuous
/// dirty coverage for the same topology. They are not portable artifact identifiers.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MemoryBaselineToken {
    tracker_id: u64,
    topology: MemoryTopologyGeneration,
    generation: MemoryGeneration,
}

impl MemoryBaselineToken {
    /// Returns the published memory generation represented by this baseline.
    pub fn generation(self) -> MemoryGeneration {
        self.generation
    }

    /// Returns the memory topology to which this baseline is bound.
    pub fn topology(self) -> MemoryTopologyGeneration {
        self.topology
    }
}

/// Describes whether a candidate capture is complete or relative to a retained baseline.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MemoryCaptureKind {
    /// Every ordinary memory-content range must be produced.
    Full,
    /// Only changed ranges are produced; unchanged content references come from this generation.
    Incremental {
        /// Last successfully published generation whose unchanged references may be reused.
        baseline: MemoryGeneration,
    },
}

/// Candidate memory generation that has not yet become the retained baseline.
#[derive(Debug, Eq, PartialEq)]
pub struct MemoryCapturePlan {
    tracker_id: u64,
    topology: MemoryTopologyGeneration,
    generation: MemoryGeneration,
    kind: MemoryCaptureKind,
    changed_ranges: Vec<GuestMemoryRange>,
}

/// Bounded streaming configuration for memory generation production.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MemoryCaptureOptions {
    chunk_size: usize,
    detect_zero: bool,
}

impl MemoryCaptureOptions {
    /// Creates a streaming configuration with a non-zero maximum chunk size.
    pub fn new(chunk_size: usize, detect_zero: bool) -> Result<Self> {
        if chunk_size == 0 {
            return Err(Error::InvalidChunkSize);
        }
        Ok(Self {
            chunk_size,
            detect_zero,
        })
    }

    /// Maximum number of bytes borrowed by one sink call.
    pub fn chunk_size(self) -> usize {
        self.chunk_size
    }

    /// Whether all-zero chunks should be emitted as sparse zero ranges.
    pub fn detects_zero(self) -> bool {
        self.detect_zero
    }
}

impl Default for MemoryCaptureOptions {
    fn default() -> Self {
        Self {
            chunk_size: 2 * 1024 * 1024,
            detect_zero: true,
        }
    }
}

/// Receives one bounded, ordered memory-generation stream.
///
/// Implementations normally fuse content hashing, compression, and immutable-object publication
/// into `write_bytes`. Calls are synchronous so the VMM can reuse one staging buffer.
pub trait MemoryCaptureSink {
    /// Writes exact bytes for one guest-physical range.
    fn write_bytes(&mut self, range: GuestMemoryRange, bytes: &[u8]) -> io::Result<()>;

    /// Records that one guest-physical range reads entirely as zero.
    fn write_zero(&mut self, range: GuestMemoryRange) -> io::Result<()>;
}

/// Measurements from producing one full or incremental memory generation.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct MemoryCaptureStats {
    /// Total logical bytes covered by the capture.
    pub logical_bytes: u64,
    /// Non-zero bytes passed to the sink.
    pub emitted_bytes: u64,
    /// Zero bytes represented sparsely.
    pub zero_bytes: u64,
    /// Number of bounded sink calls.
    pub chunks: u64,
}

impl MemoryCapturePlan {
    /// Returns the topology captured by this plan.
    pub fn topology(&self) -> MemoryTopologyGeneration {
        self.topology
    }

    /// Returns the candidate generation assigned to this plan.
    pub fn generation(&self) -> MemoryGeneration {
        self.generation
    }

    /// Returns whether this is a complete or incremental capture.
    pub fn kind(&self) -> MemoryCaptureKind {
        self.kind
    }

    /// Returns sorted, non-overlapping changed ranges for incremental capture.
    pub fn changed_ranges(&self) -> &[GuestMemoryRange] {
        &self.changed_ranges
    }
}

/// Reason an incremental request must safely use complete capture instead.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FullCaptureReason {
    /// No complete generation has been published by this ledger.
    NoPublishedBaseline,
    /// Dirty coverage was explicitly invalidated or lost.
    DirtyCoverageInvalidated,
    /// The supplied token belongs to another ledger.
    DifferentTracker,
    /// The supplied token belongs to another memory topology.
    DifferentTopology,
    /// The supplied token is not the latest published generation.
    StaleBaseline,
    /// Changed coverage is large enough that bounded complete capture is cheaper.
    DeltaNotBeneficial,
}

/// Result of requesting capture relative to a retained baseline.
#[derive(Debug, Eq, PartialEq)]
pub enum IncrementalCaptureDecision {
    /// Dirty coverage is complete and the contained delta may be produced.
    Incremental(MemoryCapturePlan),
    /// Tracking was valid, but capture policy selected a complete generation instead.
    Complete {
        /// Complete candidate that may be streamed immediately.
        capture: MemoryCapturePlan,
        /// Why complete capture was selected.
        reason: FullCaptureReason,
    },
    /// Correctness requires a complete capture for the stated reason.
    FullRequired(FullCaptureReason),
}

/// Errors produced while managing memory generations.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    /// A guest-memory range had zero length.
    EmptyRange,
    /// A guest-memory range overflowed the guest-physical address space.
    RangeOverflow,
    /// Another candidate capture must be published or abandoned first.
    CaptureAlreadyPending,
    /// The candidate generation does not belong to this ledger.
    ForeignCapture,
    /// The supplied candidate is no longer the active pending capture.
    StaleCapture,
    /// The memory-generation counter is exhausted.
    GenerationExhausted,
    /// A streaming memory chunk size was zero.
    InvalidChunkSize,
}

impl Display for Error {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyRange => write!(f, "guest-memory range cannot be empty"),
            Self::RangeOverflow => write!(f, "guest-memory range overflows the address space"),
            Self::CaptureAlreadyPending => write!(f, "another memory capture is already pending"),
            Self::ForeignCapture => write!(f, "memory capture belongs to another tracker"),
            Self::StaleCapture => write!(f, "memory capture is no longer pending"),
            Self::GenerationExhausted => write!(f, "memory generation counter is exhausted"),
            Self::InvalidChunkSize => write!(f, "memory capture chunk size must be non-zero"),
        }
    }
}

impl std::error::Error for Error {}

/// Shorthand result for memory-generation operations.
pub type Result<T> = std::result::Result<T, Error>;

/// Tracks the single latest published baseline for one VMM memory topology.
///
/// The ledger intentionally retains no historical dirty generations. Hypervisor and host-writer
/// trackers remain responsible for continuous coverage between the published baseline and the
/// next final seal.
#[derive(Debug)]
pub struct MemoryGenerationLedger {
    tracker_id: u64,
    topology: MemoryTopologyGeneration,
    next_generation: u64,
    published: Option<MemoryGeneration>,
    dirty_coverage_valid: bool,
    pending: Option<MemoryGeneration>,
}

impl MemoryGenerationLedger {
    /// Creates an empty ledger for one VMM memory topology.
    pub fn new(topology: MemoryTopologyGeneration) -> Self {
        Self {
            tracker_id: NEXT_TRACKER_ID.fetch_add(1, Ordering::Relaxed),
            topology,
            next_generation: 1,
            published: None,
            dirty_coverage_valid: false,
            pending: None,
        }
    }

    /// Plans a complete memory generation.
    pub fn plan_full_capture(&mut self) -> Result<MemoryCapturePlan> {
        self.plan_capture(MemoryCaptureKind::Full, Vec::new())
    }

    /// Plans capture relative to the latest retained baseline.
    ///
    /// Ranges may overlap or be adjacent. The returned plan sorts and coalesces them off the
    /// resident request path so storage receives the smallest equivalent range list.
    pub fn plan_incremental_capture<I>(
        &mut self,
        baseline: MemoryBaselineToken,
        changed_ranges: I,
    ) -> Result<IncrementalCaptureDecision>
    where
        I: IntoIterator<Item = GuestMemoryRange>,
    {
        if let Some(reason) = self.incremental_full_reason(baseline) {
            return Ok(IncrementalCaptureDecision::FullRequired(reason));
        }

        let ranges = coalesce_ranges(changed_ranges);
        self.plan_capture(
            MemoryCaptureKind::Incremental {
                baseline: baseline.generation,
            },
            ranges,
        )
        .map(IncrementalCaptureDecision::Incremental)
    }

    /// Returns why `baseline` cannot currently start an incremental capture.
    pub fn incremental_full_reason(
        &self,
        baseline: MemoryBaselineToken,
    ) -> Option<FullCaptureReason> {
        if self.published.is_none() {
            return Some(FullCaptureReason::NoPublishedBaseline);
        }
        if !self.dirty_coverage_valid {
            return Some(FullCaptureReason::DirtyCoverageInvalidated);
        }
        if baseline.tracker_id != self.tracker_id {
            return Some(FullCaptureReason::DifferentTracker);
        }
        if baseline.topology != self.topology {
            return Some(FullCaptureReason::DifferentTopology);
        }
        if Some(baseline.generation) != self.published {
            return Some(FullCaptureReason::StaleBaseline);
        }
        None
    }

    /// Whether a candidate capture still awaits publication or abandonment.
    pub fn has_pending_capture(&self) -> bool {
        self.pending.is_some()
    }

    /// Returns the currently retained baseline token, if dirty coverage remains valid.
    pub fn retained_baseline(&self) -> Option<MemoryBaselineToken> {
        if !self.dirty_coverage_valid {
            return None;
        }
        self.published.map(|generation| MemoryBaselineToken {
            tracker_id: self.tracker_id,
            topology: self.topology,
            generation,
        })
    }

    /// Accepts a candidate only after its objects and complete logical manifest are published.
    ///
    /// Publication alone does not establish continuous dirty coverage. The backend must arm its
    /// next CPU tracker generation and host-writer ledger, then call
    /// [`retain_dirty_coverage`](Self::retain_dirty_coverage) before incremental capture is legal.
    pub fn publish(&mut self, capture: &MemoryCapturePlan) -> Result<MemoryBaselineToken> {
        self.validate_pending(capture)?;
        self.pending = None;
        self.published = Some(capture.generation);
        self.dirty_coverage_valid = false;

        Ok(MemoryBaselineToken {
            tracker_id: self.tracker_id,
            topology: self.topology,
            generation: capture.generation,
        })
    }

    /// Confirms that CPU and host-writer tracking continuously cover this published baseline.
    pub fn retain_dirty_coverage(&mut self, baseline: MemoryBaselineToken) -> Result<()> {
        if baseline.tracker_id != self.tracker_id || baseline.topology != self.topology {
            return Err(Error::ForeignCapture);
        }
        if Some(baseline.generation) != self.published || self.pending.is_some() {
            return Err(Error::StaleCapture);
        }
        self.dirty_coverage_valid = true;
        Ok(())
    }

    /// Abandons a candidate without advancing the retained published baseline.
    pub fn abandon(&mut self, capture: &MemoryCapturePlan) -> Result<()> {
        self.validate_pending(capture)?;
        self.pending = None;
        Ok(())
    }

    /// Invalidates incremental capture after any uncertainty about dirty coverage.
    pub fn invalidate_dirty_coverage(&mut self) {
        self.dirty_coverage_valid = false;
        self.pending = None;
    }

    /// Replaces the memory topology and requires the next capture to be complete.
    pub fn replace_topology(&mut self, topology: MemoryTopologyGeneration) {
        self.topology = topology;
        self.published = None;
        self.dirty_coverage_valid = false;
        self.pending = None;
    }

    fn plan_capture(
        &mut self,
        kind: MemoryCaptureKind,
        changed_ranges: Vec<GuestMemoryRange>,
    ) -> Result<MemoryCapturePlan> {
        if self.pending.is_some() {
            return Err(Error::CaptureAlreadyPending);
        }

        let generation = MemoryGeneration(self.next_generation);
        self.next_generation = self
            .next_generation
            .checked_add(1)
            .ok_or(Error::GenerationExhausted)?;
        self.pending = Some(generation);

        Ok(MemoryCapturePlan {
            tracker_id: self.tracker_id,
            topology: self.topology,
            generation,
            kind,
            changed_ranges,
        })
    }

    pub(crate) fn validate_pending(&self, capture: &MemoryCapturePlan) -> Result<()> {
        if capture.tracker_id != self.tracker_id || capture.topology != self.topology {
            return Err(Error::ForeignCapture);
        }
        if self.pending != Some(capture.generation) {
            return Err(Error::StaleCapture);
        }
        Ok(())
    }
}

fn coalesce_ranges<I>(ranges: I) -> Vec<GuestMemoryRange>
where
    I: IntoIterator<Item = GuestMemoryRange>,
{
    let mut ranges: Vec<_> = ranges.into_iter().collect();
    ranges.sort_unstable_by_key(|range| range.start);

    let mut coalesced: Vec<GuestMemoryRange> = Vec::with_capacity(ranges.len());
    for range in ranges {
        if let Some(previous) = coalesced.last_mut() {
            if range.start <= previous.end() {
                let end = previous.end().max(range.end());
                previous.length = end - previous.start;
                continue;
            }
        }
        coalesced.push(range);
    }
    coalesced
}

#[cfg(test)]
mod tests {
    use super::*;

    fn range(start: u64, length: u64) -> GuestMemoryRange {
        GuestMemoryRange::new(start, length).unwrap()
    }

    #[test]
    fn incremental_capture_reuses_only_latest_published_baseline() {
        let topology = MemoryTopologyGeneration::new(7);
        let mut ledger = MemoryGenerationLedger::new(topology);

        assert_eq!(
            ledger
                .plan_incremental_capture(
                    MemoryBaselineToken {
                        tracker_id: 0,
                        topology,
                        generation: MemoryGeneration(0),
                    },
                    [],
                )
                .unwrap(),
            IncrementalCaptureDecision::FullRequired(FullCaptureReason::NoPublishedBaseline)
        );

        let first = ledger.plan_full_capture().unwrap();
        let first_baseline = ledger.publish(&first).unwrap();
        ledger.retain_dirty_coverage(first_baseline).unwrap();
        let second = match ledger
            .plan_incremental_capture(first_baseline, [range(0x3000, 0x1000)])
            .unwrap()
        {
            IncrementalCaptureDecision::Incremental(capture) => capture,
            decision => panic!("unexpected decision: {decision:?}"),
        };
        let second_baseline = ledger.publish(&second).unwrap();
        ledger.retain_dirty_coverage(second_baseline).unwrap();

        assert_eq!(
            ledger.plan_incremental_capture(first_baseline, []).unwrap(),
            IncrementalCaptureDecision::FullRequired(FullCaptureReason::StaleBaseline)
        );
        assert_eq!(second_baseline.generation().get(), 2);
    }

    #[test]
    fn failed_publication_does_not_advance_baseline() {
        let topology = MemoryTopologyGeneration::new(1);
        let mut ledger = MemoryGenerationLedger::new(topology);
        let first = ledger.plan_full_capture().unwrap();
        let baseline = ledger.publish(&first).unwrap();
        ledger.retain_dirty_coverage(baseline).unwrap();

        let failed = match ledger
            .plan_incremental_capture(baseline, [range(0x1000, 0x1000)])
            .unwrap()
        {
            IncrementalCaptureDecision::Incremental(capture) => capture,
            decision => panic!("unexpected decision: {decision:?}"),
        };
        ledger.abandon(&failed).unwrap();

        let retry = ledger
            .plan_incremental_capture(baseline, [range(0x1000, 0x2000)])
            .unwrap();
        assert!(matches!(retry, IncrementalCaptureDecision::Incremental(_)));
    }

    #[test]
    fn changed_ranges_are_sorted_and_coalesced() {
        let topology = MemoryTopologyGeneration::new(1);
        let mut ledger = MemoryGenerationLedger::new(topology);
        let first = ledger.plan_full_capture().unwrap();
        let baseline = ledger.publish(&first).unwrap();
        ledger.retain_dirty_coverage(baseline).unwrap();

        let capture = match ledger
            .plan_incremental_capture(
                baseline,
                [
                    range(0x4000, 0x1000),
                    range(0x1000, 0x2000),
                    range(0x3000, 0x1000),
                    range(0x8000, 0x1000),
                ],
            )
            .unwrap()
        {
            IncrementalCaptureDecision::Incremental(capture) => capture,
            decision => panic!("unexpected decision: {decision:?}"),
        };

        assert_eq!(
            capture.changed_ranges(),
            &[range(0x1000, 0x4000), range(0x8000, 0x1000)]
        );
    }

    #[test]
    fn invalid_dirty_coverage_requires_full_capture() {
        let topology = MemoryTopologyGeneration::new(1);
        let mut ledger = MemoryGenerationLedger::new(topology);
        let first = ledger.plan_full_capture().unwrap();
        let baseline = ledger.publish(&first).unwrap();
        ledger.retain_dirty_coverage(baseline).unwrap();
        ledger.invalidate_dirty_coverage();

        assert_eq!(
            ledger.plan_incremental_capture(baseline, []).unwrap(),
            IncrementalCaptureDecision::FullRequired(FullCaptureReason::DirtyCoverageInvalidated)
        );
    }

    #[test]
    fn ranges_reject_empty_and_overflowing_values() {
        assert_eq!(GuestMemoryRange::new(0, 0), Err(Error::EmptyRange));
        assert_eq!(
            GuestMemoryRange::new(u64::MAX, 2),
            Err(Error::RangeOverflow)
        );
    }

    #[test]
    fn publication_requires_explicit_dirty_coverage_confirmation() {
        let topology = MemoryTopologyGeneration::new(1);
        let mut ledger = MemoryGenerationLedger::new(topology);
        let first = ledger.plan_full_capture().unwrap();
        let baseline = ledger.publish(&first).unwrap();

        assert_eq!(
            ledger.plan_incremental_capture(baseline, []).unwrap(),
            IncrementalCaptureDecision::FullRequired(FullCaptureReason::DirtyCoverageInvalidated)
        );

        ledger.retain_dirty_coverage(baseline).unwrap();
        assert!(matches!(
            ledger.plan_incremental_capture(baseline, []).unwrap(),
            IncrementalCaptureDecision::Incremental(_)
        ));
    }
}
