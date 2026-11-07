//! Discarding guest RAM on Linux hosts without letting the host free page tables.
//!
//! Linux 6.14 and later can free a page table that `MADV_DONTNEED` empties
//! (`CONFIG_PT_RECLAIM`, page table reclaim in `zap_pte_range`). It does so only when one call
//! covers the whole range a page table maps: a PMD-sized, PMD-aligned block (2 MiB with 4 KiB
//! pages). From 7.0 to 7.1.8 that path flushes the wrong address after freeing the table
//! (4c640eb4181c; fixed by 478a1c3abebf, "mm: fix incorrect flush address in direct page table
//! reclaim", in 7.2 and 7.1.9; CVE-2026-74674), which corrupts host memory when the VMM keeps
//! discarding guest pages, as balloon free page reporting does: page tables of the VMM are
//! later found overwritten (`BUG: Bad page map ... pte:101010101010101`).
//!
//! Whatever the kernel, the discards here never cover a whole PMD block in one call. A
//! PMD-aligned range is first advanced by one base page, then split into PMD-sized calls from
//! that unaligned address. Each call therefore touches only parts of two adjacent PMD blocks;
//! the page-table reclaim condition cannot hold for either part. Unlike leaving a page behind in
//! every block, this still returns every complete guest page to the host.

#[cfg(target_os = "linux")]
use std::io;

/// Address ranges, as `(start, len)`.
type Ranges = Vec<(usize, usize)>;

/// The host's base page size and the span one page table maps (a PMD entry).
#[cfg(target_os = "linux")]
fn page_and_pmd_size() -> (usize, usize) {
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    let page = if page > 0 { page as usize } else { 4096 };
    // One page table holds page / 8 entries (64-bit), each mapping one page.
    (page, page * (page / 8))
}

/// The separate `madvise` calls covering the complete pages in `[start, start + len)`, plus
/// partial pages left out at either end. `madvise` rounds a length up to a host page, so partial
/// end pages must remain untouched: rounding one into a call could otherwise cover a whole PMD.
///
/// A PMD-sized call is safe when its start is not PMD-aligned: no individual PMD it crosses is
/// wholly covered. If the range starts on a PMD boundary, discard its first page in a distinct
/// small call to shift the following calls. Keeping the calls distinct is essential—combining
/// adjacent pieces would restore the exact range that triggers page-table reclaim.
fn plan(start: usize, len: usize, page: usize, pmd: usize) -> (Ranges, Ranges) {
    let end = start.saturating_add(len);
    let mut pieces = Ranges::new();
    let mut kept = Ranges::new();

    // Pieces represent system-call boundaries, so deliberately do not coalesce them.
    fn push_piece(s: usize, e: usize, list: &mut Ranges) {
        if s < e {
            list.push((s, e - s));
        }
    }

    // Kept fragments have no call-boundary meaning, so join neighbours for callers that zero
    // them manually after a successful discard.
    fn push_kept(s: usize, e: usize, list: &mut Ranges) {
        if s >= e {
            return;
        }
        match list.last_mut() {
            Some((ps, pl)) if *ps + *pl == s => *pl += e - s,
            _ => list.push((s, e - s)),
        }
    }

    let first = start.div_ceil(page).saturating_mul(page).min(end);
    let last = (end / page * page).max(first);
    push_kept(start, first, &mut kept);

    let mut cur = first;
    if last.saturating_sub(cur) >= pmd && cur.is_multiple_of(pmd) {
        // This first page moves every subsequent PMD-sized call off the PMD boundary.
        push_piece(cur, cur + page, &mut pieces);
        cur += page;
    }

    while last.saturating_sub(cur) > pmd {
        push_piece(cur, cur + pmd, &mut pieces);
        cur += pmd;
    }
    push_piece(cur, last, &mut pieces);

    push_kept(last, end, &mut kept);
    (pieces, kept)
}

/// `madvise(MADV_DONTNEED)` over the whole pages of `[addr, addr + len)`, never over a whole PMD
/// block in one call. Returns the parts it left untouched, as `(start, len)`, so a caller that
/// needs the range zeroed can zero them.
#[cfg(target_os = "linux")]
pub(crate) fn dontneed(addr: *mut u8, len: usize) -> io::Result<Vec<(*mut u8, usize)>> {
    let (page, pmd) = page_and_pmd_size();
    let (pieces, kept) = plan(addr as usize, len, page, pmd);
    for (s, l) in pieces {
        if unsafe { libc::madvise(s as *mut libc::c_void, l, libc::MADV_DONTNEED) } < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(kept.into_iter().map(|(s, l)| (s as *mut u8, l)).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    const P: usize = 4096;
    const M: usize = 2 << 20;

    /// Whether one call, with its length rounded up to whole pages as `madvise` does, covers a
    /// whole PMD block.
    fn covers_whole_block(pieces: &[(usize, usize)]) -> bool {
        covers_whole_block_for(pieces, P, M)
    }

    fn covers_whole_block_for(pieces: &[(usize, usize)], page: usize, pmd: usize) -> bool {
        pieces
            .iter()
            .any(|&(s, l)| s.div_ceil(pmd) * pmd + pmd <= (s + l).div_ceil(page) * page)
    }

    /// Pieces and kept parts together are exactly `[start, start + len)`, without overlap, and
    /// every piece is whole pages.
    fn assert_exact(start: usize, len: usize, pieces: &[(usize, usize)], kept: &[(usize, usize)]) {
        assert_exact_for(start, len, pieces, kept, P);
    }

    fn assert_exact_for(
        start: usize,
        len: usize,
        pieces: &[(usize, usize)],
        kept: &[(usize, usize)],
        page: usize,
    ) {
        assert!(pieces
            .iter()
            .all(|&(s, l)| s % page == 0 && l % page == 0 && l > 0));
        let mut all: Vec<(usize, usize)> = pieces.iter().chain(kept).copied().collect();
        all.sort();
        let mut cur = start;
        for (s, l) in all {
            assert_eq!(s, cur, "gap or overlap at {s:#x}");
            cur = s + l;
        }
        assert_eq!(cur, start + len);
    }

    #[test]
    fn one_aligned_block_is_split_without_retaining_a_page() {
        let (pieces, kept) = plan(16 * M, M, P, M);
        assert_eq!(pieces, vec![(16 * M, P), (16 * M + P, M - P)]);
        assert!(kept.is_empty());
        assert!(!covers_whole_block(&pieces));
    }

    #[test]
    fn aligned_blocks_use_offset_calls_without_retaining_guest_memory() {
        let (pieces, kept) = plan(16 * M, 2 * M, P, M);
        assert_eq!(
            pieces,
            vec![(16 * M, P), (16 * M + P, M), (17 * M + P, M - P),]
        );
        assert!(kept.is_empty());
        assert!(!covers_whole_block(&pieces));
    }

    #[test]
    fn a_range_holding_no_whole_block_is_discarded_whole() {
        let (pieces, kept) = plan(16 * M + P, M, P, M);
        assert_eq!(pieces, vec![(16 * M + P, M)]);
        assert!(kept.is_empty());
        let (pieces, kept) = plan(16 * M, 8 * P, P, M);
        assert_eq!(pieces, vec![(16 * M, 8 * P)]);
        assert!(kept.is_empty());
    }

    #[test]
    fn a_misaligned_range_joins_its_ends_to_the_blocks() {
        let start = 16 * M + 100 * P;
        let len = 3 * M;
        let (pieces, kept) = plan(start, len, P, M);
        assert!(kept.is_empty());
        assert_eq!(pieces, vec![(start, M), (start + M, M), (start + 2 * M, M)]);
        assert!(!covers_whole_block(&pieces));
        assert_exact(start, len, &pieces, &kept);
    }

    #[test]
    fn a_length_short_of_a_block_is_not_rounded_up_to_one() {
        // `madvise` would round `M - 1` up to `M`: the partial last page must be left out.
        let (pieces, kept) = plan(16 * M, M - 1, P, M);
        assert_eq!(pieces, vec![(16 * M, M - P)]);
        assert_eq!(kept, vec![(17 * M - P, P - 1)]);
        assert!(!covers_whole_block(&pieces));
    }

    #[test]
    fn partial_pages_at_either_end_are_left_out() {
        let start = 16 * M - 10;
        let len = 10 + M + 20;
        let (pieces, kept) = plan(start, len, P, M);
        assert_eq!(kept, vec![(start, 10), (17 * M, 20)]);
        assert_eq!(pieces, vec![(16 * M, P), (16 * M + P, M - P)]);
        assert_exact(start, len, &pieces, &kept);
    }

    #[test]
    fn less_than_a_page_is_left_alone() {
        assert_eq!(
            plan(16 * M + 1, P - 2, P, M),
            (vec![], vec![(16 * M + 1, P - 2)])
        );
    }

    #[test]
    fn every_range_is_accounted_for_and_never_covers_a_block() {
        for start_off in [
            0,
            1,
            P - 1,
            P,
            255 * P,
            511 * P,
            511 * P + 7,
            512 * P,
            513 * P,
        ] {
            for len in [
                1,
                P - 1,
                P,
                511 * P,
                M - 1,
                M,
                M + 1,
                1025 * P,
                2 * M - 1,
                2 * M,
                3 * M + 5,
            ] {
                let start = 16 * M + start_off;
                let (pieces, kept) = plan(start, len, P, M);
                assert!(!covers_whole_block(&pieces), "{start_off:#x} {len:#x}");
                assert_exact(start, len, &pieces, &kept);
            }
        }
    }

    #[test]
    fn planner_preserves_the_invariant_for_larger_linux_page_sizes() {
        // A 64-bit PTE table has page / 8 entries. Exercise common Linux base-page sizes rather
        // than baking the usual 4 KiB / 2 MiB geometry into the safety property.
        for page in [4096, 16384, 65536] {
            let pmd = page * (page / 8);
            for start_offset in [0, 1, page - 1, page, pmd - page, pmd - 1, pmd, pmd + page] {
                for len in [1, page - 1, page, pmd - 1, pmd, pmd + page, 2 * pmd + 1] {
                    let start = 16 * pmd + start_offset;
                    let (pieces, kept) = plan(start, len, page, pmd);
                    assert!(
                        !covers_whole_block_for(&pieces, page, pmd),
                        "page={page:#x} start_offset={start_offset:#x} len={len:#x}"
                    );
                    assert_exact_for(start, len, &pieces, &kept, page);

                    // All complete pages in the original range must be returned to the host;
                    // only incomplete boundary pages are allowed to remain in `kept`.
                    assert!(kept.iter().all(|&(s, l)| {
                        s.div_ceil(page).saturating_mul(page).saturating_add(page) > s + l
                    }));
                }
            }
        }
    }

    #[test]
    fn empty_range_does_nothing() {
        assert_eq!(plan(16 * M, 0, P, M), (vec![], vec![]));
    }

    // Linux guarantees anonymous pages read back as zero after MADV_DONTNEED. Other Unix hosts
    // may treat the advice lazily, so their portable planner coverage stops above.
    #[cfg(target_os = "linux")]
    #[test]
    fn dontneed_discards_an_entire_aligned_block() {
        let (_, pmd) = page_and_pmd_size();
        let map_len = 3 * pmd;
        let map = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                map_len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        assert_ne!(map, libc::MAP_FAILED);
        let aligned = (map as usize).div_ceil(pmd) * pmd;
        let block = aligned as *mut u8;
        unsafe { std::ptr::write_bytes(block, 0xA5, pmd) };

        let kept = dontneed(block, pmd).unwrap();
        assert!(kept.is_empty());
        let bytes = unsafe { std::slice::from_raw_parts(block, pmd) };
        assert!(bytes.iter().all(|&b| b == 0));

        unsafe { libc::munmap(map, map_len) };
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn dontneed_leaves_a_partial_last_page_untouched() {
        let (page, pmd) = page_and_pmd_size();
        let map_len = 3 * pmd;
        let map = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                map_len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        assert_ne!(map, libc::MAP_FAILED);
        let block = ((map as usize).div_ceil(pmd) * pmd) as *mut u8;
        unsafe { std::ptr::write_bytes(block, 0xA5, pmd) };

        let kept = dontneed(block, pmd - 1).unwrap();
        assert_eq!(kept, vec![(unsafe { block.add(pmd - page) }, page - 1)]);
        let bytes = unsafe { std::slice::from_raw_parts(block, pmd) };
        assert!(bytes[..pmd - page].iter().all(|&b| b == 0));
        assert!(bytes[pmd - page..].iter().all(|&b| b == 0xA5));

        unsafe { libc::munmap(map, map_len) };
    }
}
