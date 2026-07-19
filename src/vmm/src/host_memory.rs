// Copyright 2026 Super Rad Company
// SPDX-License-Identifier: Apache-2.0

//! Host virtual-memory policy for anonymous guest RAM.

use std::io;

use arch::ArchMemoryInfo;
#[cfg(all(target_os = "linux", not(any(feature = "tee", feature = "aws-nitro"))))]
use vm_memory::{Address, GuestMemoryBackend, GuestMemoryRegion};
use vm_memory::{GuestAddress, GuestMemoryMmap};

use crate::vmm_config::machine_config::HostMemoryPolicy;

//--------------------------------------------------------------------------------------------------
// Constants
//--------------------------------------------------------------------------------------------------

/// Preserve process-level THP exclusion except for VMAs explicitly marked with `MADV_HUGEPAGE`.
///
/// This flag is part of Linux's `PR_SET_THP_DISABLE` ABI but is not exposed by every supported
/// version of the libc crate.
#[cfg(all(target_os = "linux", not(any(feature = "tee", feature = "aws-nitro"))))]
const PR_THP_DISABLE_EXCEPT_ADVISED: libc::c_ulong = 1 << 1;

//--------------------------------------------------------------------------------------------------
// Types
//--------------------------------------------------------------------------------------------------

/// Failure to apply an explicitly requested host memory policy.
#[derive(Debug)]
pub(crate) enum Error {
    /// The current host or build cannot apply explicit page-size advice safely.
    #[cfg_attr(
        all(target_os = "linux", not(any(feature = "tee", feature = "aws-nitro"))),
        allow(dead_code)
    )]
    Unsupported,
    /// The host rejected advice for a guest RAM mapping.
    #[cfg_attr(
        any(not(target_os = "linux"), feature = "tee", feature = "aws-nitro"),
        allow(dead_code)
    )]
    Advice(io::Error),
}

#[cfg(any(
    all(target_os = "linux", not(any(feature = "tee", feature = "aws-nitro"))),
    test
))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct GuestAddressRange {
    start: u64,
    end: u64,
}

//--------------------------------------------------------------------------------------------------
// Functions
//--------------------------------------------------------------------------------------------------

/// Applies the requested policy only to architectural guest RAM and optional
/// virtio-mem capacity. Device shared-memory and firmware-only mappings are
/// intentionally outside these ranges.
pub(crate) fn apply(
    guest_memory: &GuestMemoryMmap,
    memory_info: &ArchMemoryInfo,
    hotplug_range: Option<(GuestAddress, usize)>,
    policy: HostMemoryPolicy,
) -> Result<(), Error> {
    if policy == HostMemoryPolicy::Inherit {
        return Ok(());
    }

    #[cfg(all(target_os = "linux", not(any(feature = "tee", feature = "aws-nitro"))))]
    {
        let mut ranges = guest_ram_ranges(memory_info);
        if let Some((start, len)) = hotplug_range {
            if len > 0 {
                ranges.push(GuestAddressRange {
                    start: start.raw_value(),
                    end: start.raw_value().saturating_add(len as u64),
                });
            }
        }

        apply_linux(guest_memory, &ranges, policy)
    }

    #[cfg(any(not(target_os = "linux"), feature = "tee", feature = "aws-nitro"))]
    {
        let _ = (guest_memory, memory_info, hotplug_range);
        Err(Error::Unsupported)
    }
}

#[cfg(all(target_os = "linux", not(any(feature = "tee", feature = "aws-nitro"))))]
fn apply_linux(
    guest_memory: &GuestMemoryMmap,
    ranges: &[GuestAddressRange],
    policy: HostMemoryPolicy,
) -> Result<(), Error> {
    if policy == HostMemoryPolicy::PreferHugePages {
        allow_advised_huge_pages()?;
    }

    let advice = match policy {
        HostMemoryPolicy::Inherit => return Ok(()),
        HostMemoryPolicy::PreferHugePages => libc::MADV_HUGEPAGE,
        HostMemoryPolicy::PreferBasePages => libc::MADV_NOHUGEPAGE,
    };

    let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if page_size <= 0 {
        return Err(Error::Advice(io::Error::last_os_error()));
    }
    let page_size = page_size as usize;

    for region in guest_memory.iter() {
        for range in ranges {
            let Some((offset, len)) =
                intersect_region(region.start_addr().raw_value(), region.len(), *range)
            else {
                continue;
            };

            let host_addr = region.as_ptr().wrapping_add(offset);
            if (host_addr as usize) % page_size != 0 || len % page_size != 0 {
                return Err(Error::Advice(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "guest RAM advice range is not host-page aligned",
                )));
            }

            // SAFETY: GuestMemoryMmap owns a live mapping covering `host_addr..host_addr + len`.
            // The intersection above confines the call to that mapping, and madvise does not
            // take ownership of or retain the pointer.
            if unsafe { libc::madvise(host_addr.cast(), len, advice) } != 0 {
                return Err(Error::Advice(io::Error::last_os_error()));
            }
        }
    }

    Ok(())
}

#[cfg(all(target_os = "linux", not(any(feature = "tee", feature = "aws-nitro"))))]
fn allow_advised_huge_pages() -> Result<(), Error> {
    // Launchers such as Bun may disable THP before exec, and Linux preserves that process setting
    // across execve. Except-advised mode keeps THP disabled for every unrelated VMA while allowing
    // the guest-RAM ranges below to opt in with MADV_HUGEPAGE.
    let result = unsafe {
        libc::prctl(
            libc::PR_SET_THP_DISABLE,
            1,
            PR_THP_DISABLE_EXCEPT_ADVISED,
            0,
            0,
        )
    };
    if result != 0 {
        return Err(Error::Advice(io::Error::last_os_error()));
    }

    Ok(())
}

#[cfg(all(target_os = "linux", not(any(feature = "tee", feature = "aws-nitro"))))]
fn guest_ram_ranges(info: &ArchMemoryInfo) -> Vec<GuestAddressRange> {
    #[cfg(target_arch = "x86_64")]
    {
        let mut ranges = Vec::new();
        if info.ram_below_gap > 0 {
            ranges.push(GuestAddressRange {
                start: 0,
                end: info.ram_below_gap,
            });
        }
        if info.ram_above_gap > 0 {
            ranges.push(GuestAddressRange {
                start: info.ram_last_addr.saturating_sub(info.ram_above_gap),
                end: info.ram_last_addr,
            });
        }
        ranges
    }

    #[cfg(target_arch = "aarch64")]
    {
        vec![GuestAddressRange {
            start: info.ram_start_addr,
            end: info.ram_last_addr,
        }]
    }

    #[cfg(target_arch = "riscv64")]
    {
        vec![GuestAddressRange {
            start: 0,
            end: info.ram_last_addr,
        }]
    }
}

#[cfg(any(
    all(target_os = "linux", not(any(feature = "tee", feature = "aws-nitro"))),
    test
))]
fn intersect_region(
    region_start: u64,
    region_len: u64,
    range: GuestAddressRange,
) -> Option<(usize, usize)> {
    if region_len == 0 || range.start >= range.end {
        return None;
    }

    let region_end = region_start.saturating_add(region_len);
    let overlap_start = region_start.max(range.start);
    let overlap_end = region_end.min(range.end);
    if overlap_start >= overlap_end {
        return None;
    }

    Some((
        usize::try_from(overlap_start - region_start).ok()?,
        usize::try_from(overlap_end - overlap_start).ok()?,
    ))
}

//--------------------------------------------------------------------------------------------------
// Tests
//--------------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intersection_confines_advice_to_guest_ram() {
        let range = GuestAddressRange {
            start: 0x2000,
            end: 0x6000,
        };

        assert_eq!(
            intersect_region(0x1000, 0x3000, range),
            Some((0x1000, 0x2000))
        );
        assert_eq!(intersect_region(0x4000, 0x4000, range), Some((0, 0x2000)));
        assert_eq!(intersect_region(0x7000, 0x1000, range), None);
    }

    #[test]
    fn empty_or_inverted_ranges_are_ignored() {
        assert_eq!(
            intersect_region(0, 0x1000, GuestAddressRange { start: 1, end: 1 }),
            None
        );
        assert_eq!(
            intersect_region(0, 0x1000, GuestAddressRange { start: 2, end: 1 }),
            None
        );
        assert_eq!(
            intersect_region(0, 0, GuestAddressRange { start: 0, end: 1 }),
            None
        );
    }

    #[cfg(all(target_os = "linux", not(any(feature = "tee", feature = "aws-nitro"))))]
    #[test]
    fn linux_marks_anonymous_guest_memory_with_requested_advice() {
        let previous_thp_mode = unsafe { libc::prctl(libc::PR_GET_THP_DISABLE, 0, 0, 0, 0) };
        assert!(previous_thp_mode >= 0, "read process THP mode");
        let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as usize;
        let guest_memory = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), page_size * 2)])
            .expect("anonymous guest memory");
        let ranges = [GuestAddressRange {
            start: 0,
            end: (page_size * 2) as u64,
        }];

        apply_linux(&guest_memory, &ranges, HostMemoryPolicy::PreferHugePages)
            .expect("MADV_HUGEPAGE");
        assert_eq!(
            unsafe { libc::prctl(libc::PR_GET_THP_DISABLE, 0, 0, 0, 0) },
            3,
            "huge-page preference must survive an inherited process-wide THP disable"
        );
        let host_addr = guest_memory
            .find_region(GuestAddress(0))
            .expect("guest region")
            .as_ptr() as usize;
        assert!(mapping_vm_flags(host_addr).contains("hg"));

        apply_linux(&guest_memory, &ranges, HostMemoryPolicy::PreferBasePages)
            .expect("MADV_NOHUGEPAGE");
        assert!(mapping_vm_flags(host_addr).contains("nh"));

        let (disable, flags) = match previous_thp_mode {
            0 => (0, 0),
            1 => (1, 0),
            3 => (1, PR_THP_DISABLE_EXCEPT_ADVISED),
            other => panic!("unexpected process THP mode {other}"),
        };
        assert_eq!(
            unsafe { libc::prctl(libc::PR_SET_THP_DISABLE, disable, flags, 0, 0) },
            0,
            "restore process THP mode"
        );
    }

    #[cfg(all(target_os = "linux", not(any(feature = "tee", feature = "aws-nitro"))))]
    fn mapping_vm_flags(address: usize) -> String {
        let smaps = std::fs::read_to_string("/proc/self/smaps").expect("read /proc/self/smaps");
        let mut contains_address = false;

        for line in smaps.lines() {
            if let Some(range) = line.split_whitespace().next() {
                if let Some((start, end)) = range.split_once('-') {
                    if let (Ok(start), Ok(end)) = (
                        usize::from_str_radix(start, 16),
                        usize::from_str_radix(end, 16),
                    ) {
                        contains_address = start <= address && address < end;
                        continue;
                    }
                }
            }

            if contains_address {
                if let Some(flags) = line.strip_prefix("VmFlags:") {
                    return flags.trim().to_string();
                }
            }
        }

        panic!("mapping containing host address {address:#x} was absent from smaps");
    }
}
