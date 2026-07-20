// Copyright 2026 Super Rad Company
// SPDX-License-Identifier: Apache-2.0

//! Development-only switches shared with Microsandbox performance builds.

//--------------------------------------------------------------------------------------------------
// Constants
//--------------------------------------------------------------------------------------------------

/// Comma-separated experiment selector inherited from the Microsandbox runtime.
pub const PERF_EXPERIMENTS_ENV: &str = "MSB_PERF_EXPERIMENTS";

//--------------------------------------------------------------------------------------------------
// Types
//--------------------------------------------------------------------------------------------------

/// A libkrun-owned performance experiment.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PerfExperiment {
    MetricsResidency,
    NetworkOffload,
    NetworkMultiqueue,
    VcpuAccounting,
    BlockDescriptors,
    BlockCompletions,
    BlockFdatasync,
    BlockIoUring,
    BlockMultiqueue,
}

//--------------------------------------------------------------------------------------------------
// Methods
//--------------------------------------------------------------------------------------------------

impl PerfExperiment {
    /// Stable selector shared with the Microsandbox benchmark harness.
    pub const fn name(self) -> &'static str {
        match self {
            Self::MetricsResidency => "metrics-residency",
            Self::NetworkOffload => "network-offload",
            Self::NetworkMultiqueue => "network-multiqueue",
            Self::VcpuAccounting => "vcpu-accounting",
            Self::BlockDescriptors => "block-descriptors",
            Self::BlockCompletions => "block-completions",
            Self::BlockFdatasync => "block-fdatasync",
            Self::BlockIoUring => "block-io-uring",
            Self::BlockMultiqueue => "block-multiqueue",
        }
    }

    /// Subsystem selector that enables the experiment as part of a group.
    pub const fn group(self) -> &'static str {
        match self {
            Self::MetricsResidency => "metrics",
            Self::NetworkOffload | Self::NetworkMultiqueue => "network",
            Self::VcpuAccounting => "vcpu",
            Self::BlockDescriptors
            | Self::BlockCompletions
            | Self::BlockFdatasync
            | Self::BlockIoUring
            | Self::BlockMultiqueue => "block",
        }
    }

    /// Return whether this experiment is enabled for the current process.
    pub fn enabled(self) -> bool {
        std::env::var(PERF_EXPERIMENTS_ENV)
            .ok()
            .is_some_and(|raw| self.enabled_in(&raw))
    }

    fn enabled_in(self, raw: &str) -> bool {
        raw.split(',')
            .map(str::trim)
            .fold(false, |enabled, selector| {
                let (selected, selector) = selector
                    .strip_prefix('-')
                    .map_or((true, selector), |selector| (false, selector));
                if selector.eq_ignore_ascii_case("all")
                    || selector.eq_ignore_ascii_case(self.group())
                    || selector.eq_ignore_ascii_case(self.name())
                {
                    selected
                } else {
                    enabled
                }
            })
    }
}

//--------------------------------------------------------------------------------------------------
// Tests
//--------------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selectors_support_individual_group_and_all_modes() {
        assert!(PerfExperiment::BlockIoUring.enabled_in("block-io-uring"));
        assert!(PerfExperiment::BlockIoUring.enabled_in("block"));
        assert!(PerfExperiment::VcpuAccounting.enabled_in("all"));
        assert!(!PerfExperiment::VcpuAccounting.enabled_in("network"));
        assert!(!PerfExperiment::BlockIoUring.enabled_in("all,-block-io-uring"));
        assert!(PerfExperiment::BlockCompletions.enabled_in("all,-block-io-uring"));
    }
}
