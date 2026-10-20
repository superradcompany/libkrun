//! Accumulate one complete balloon report before publishing guest memory observations.

use std::time::Duration;

use utils::metrics::MetricsWriter;

use super::device::{VIRTIO_BALLOON_S_AVAIL, VIRTIO_BALLOON_S_MEMTOT};

//--------------------------------------------------------------------------------------------------
// Types
//--------------------------------------------------------------------------------------------------

#[derive(Default)]
pub(super) struct MemoryStats {
    total: Option<u64>,
    available: Option<u64>,
    malformed: bool,
}

//--------------------------------------------------------------------------------------------------
// Methods
//--------------------------------------------------------------------------------------------------

impl MemoryStats {
    pub(super) fn push(&mut self, tag: u16, value: u64) {
        match tag {
            VIRTIO_BALLOON_S_MEMTOT => self.malformed |= self.total.replace(value).is_some(),
            VIRTIO_BALLOON_S_AVAIL => self.malformed |= self.available.replace(value).is_some(),
            _ => {} // Other standard/extension tags do not invalidate this memory pair.
        }
    }

    pub(super) fn invalidate(&mut self) {
        self.malformed = true;
    }

    pub(super) fn publish(
        self,
        metrics: &MetricsWriter,
        epoch: Option<u64>,
        interval: Option<Duration>,
    ) {
        if !self.malformed {
            // Older guests may report only AVAIL. Preserve that existing metric,
            // but never substitute configured capacity for a missing MEMTOT.
            if let Some(available) = self.available {
                metrics.set_memory_available_bytes(available);
            }
        }
        // A restored queue can contain a captured buffer. Only a response to
        // a request from this host incarnation can establish freshness.
        let Some(epoch) = epoch else { return };
        match (self.total, self.available, interval) {
            (Some(total), Some(available), Some(interval)) if !self.malformed => {
                metrics.record_guest_memory_observation(epoch, total, available, interval);
            }
            _ => metrics.discard_guest_memory_observation(epoch),
        }
    }
}

//--------------------------------------------------------------------------------------------------
// Tests
//--------------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn report(pairs: &[(u16, u64)], malformed: bool) -> MemoryStats {
        let mut report = MemoryStats::default();
        for &(tag, value) in pairs {
            report.push(tag, value);
        }
        if malformed {
            report.invalidate();
        }
        report
    }

    #[test]
    fn pair_is_order_independent_and_ignores_unrelated_tags() {
        let metrics = MetricsWriter::default();
        for pairs in [
            vec![(5, 8192), (99, 7), (6, 1024)],
            vec![(6, 1024), (5, 8192)],
        ] {
            report(&pairs, false).publish(&metrics, Some(0), Some(Duration::from_secs(5)));
            let observed = metrics.handle().guest_memory_observation().unwrap();
            assert_eq!(observed.total_bytes, 8192);
            assert_eq!(observed.available_bytes, 1024);
            assert_eq!(observed.reporting_interval, Duration::from_secs(5));
        }
    }

    #[test]
    fn missing_duplicate_partial_and_inconsistent_reports_do_not_reuse_old_pair() {
        let metrics = MetricsWriter::default();
        for (pairs, malformed) in [
            (vec![(5, 8192)], false),
            (vec![(6, 1024)], false),
            (vec![(5, 8192), (6, 1024), (6, 1024)], false),
            (vec![(5, 8192), (5, 8192), (6, 1024)], false),
            (vec![(5, 8192), (6, 1024)], true),
            (vec![(5, 1024), (6, 8192)], false),
        ] {
            report(&[(5, 8192), (6, 1024)], false).publish(
                &metrics,
                Some(0),
                Some(Duration::from_secs(1)),
            );
            assert!(metrics.handle().guest_memory_observation().is_some());
            report(&pairs, malformed).publish(&metrics, Some(0), Some(Duration::from_secs(1)));
            assert!(metrics.handle().guest_memory_observation().is_none());
        }
    }

    #[test]
    fn unsolicited_or_restored_buffer_never_establishes_freshness() {
        let metrics = MetricsWriter::default();
        report(&[(5, 8192), (6, 1024)], false).publish(
            &metrics,
            None,
            Some(Duration::from_secs(1)),
        );
        assert!(metrics.handle().guest_memory_observation().is_none());
        report(&[(5, 8192), (6, 1024)], false).publish(
            &metrics,
            Some(metrics.guest_memory_epoch()),
            Some(Duration::from_secs(1)),
        );
        assert!(metrics.handle().guest_memory_observation().is_some());
    }

    #[test]
    fn old_guest_available_remains_available_without_a_fabricated_total() {
        let metrics = MetricsWriter::default();
        metrics.set_memory_total_bytes(8192);
        report(&[(6, 1024)], false).publish(&metrics, Some(0), Some(Duration::from_secs(1)));
        assert!(metrics.handle().guest_memory_observation().is_none());
        let memory = metrics.handle().snapshot().memory;
        assert_eq!(memory.available_bytes, Some(1024));
        assert_eq!(memory.used_bytes, Some(7168));
    }
}
