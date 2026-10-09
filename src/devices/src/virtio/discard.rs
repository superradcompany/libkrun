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
//! Whatever the kernel, the discards here never cover a whole PMD block in one call: from each
//! such block the first page is left out, so no page table is ever empty enough to be reclaimed.
//! The guest's memory still goes back to the host, less one base page per PMD block (0.2 % with
//! 4 KiB pages), and the page tables stay, as they would have without reclaim.

use std::io;

/// Address ranges, as `(start, len)`.
type Ranges = Vec<(usize, usize)>;

/// The host's base page size and the span one page table maps (a PMD entry).
fn page_and_pmd_size() -> (usize, usize) {
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    let page = if page > 0 { page as usize } else { 4096 };
    // One page table holds page / 8 entries (64-bit), each mapping one page.
    (page, page * (page / 8))
}

/// The pieces of `[start, start + len)` to discard, as `(start, len)`, and the parts left out of
/// it, likewise. Only whole host pages are discarded: `madvise` rounds a length up to a whole
/// page, so a partial page at either end is left out, or a call ending in it could still cover a
/// whole PMD block. The rest gives one piece per PMD block that lies wholly inside the range
/// (without its first page) and the parts at either end, with neighbours joined.
fn plan(start: usize, len: usize, page: usize, pmd: usize) -> (Ranges, Ranges) {
    let end = start.saturating_add(len);
    let mut pieces = Ranges::new();
    let mut kept = Ranges::new();
    fn push(s: usize, e: usize, list: &mut Ranges) {
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
    push(start, first, &mut kept);

    let mut cur = first;
    while cur < last {
        let block = cur.div_ceil(pmd).saturating_mul(pmd);
        if block >= last || block.saturating_add(pmd) > last {
            push(cur, last, &mut pieces);
            break;
        }
        push(cur, block, &mut pieces);
        push(block, block + page, &mut kept);
        push(block + page, block + pmd, &mut pieces);
        cur = block + pmd;
    }

    push(last, end, &mut kept);
    (pieces, kept)
}

/// `madvise(MADV_DONTNEED)` over the whole pages of `[addr, addr + len)`, never over a whole PMD
/// block in one call. Returns the parts it left untouched, as `(start, len)`, so a caller that
/// needs the range zeroed can zero them.
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
        pieces
            .iter()
            .any(|&(s, l)| s.div_ceil(M) * M + M <= (s + l).div_ceil(P) * P)
    }

    /// Pieces and kept parts together are exactly `[start, start + len)`, without overlap, and
    /// every piece is whole pages.
    fn assert_exact(start: usize, len: usize, pieces: &[(usize, usize)], kept: &[(usize, usize)]) {
        assert!(pieces
            .iter()
            .all(|&(s, l)| s % P == 0 && l % P == 0 && l > 0));
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
    fn one_aligned_block_is_one_call_without_its_first_page() {
        let (pieces, kept) = plan(16 * M, M, P, M);
        assert_eq!(pieces, vec![(16 * M + P, M - P)]);
        assert_eq!(kept, vec![(16 * M, P)]);
    }

    #[test]
    fn two_aligned_blocks_leave_a_page_out_of_each() {
        let (pieces, kept) = plan(16 * M, 2 * M, P, M);
        assert_eq!(pieces, vec![(16 * M + P, M - P), (17 * M + P, M - P)]);
        assert_eq!(kept, vec![(16 * M, P), (17 * M, P)]);
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
        assert_eq!(kept, vec![(17 * M, P), (18 * M, P)]);
        assert_eq!(
            pieces,
            vec![
                (start, 17 * M - start),
                (17 * M + P, M - P),
                (18 * M + P, start + len - 18 * M - P),
            ]
        );
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
        // The 10 bytes before the block join its first page, which is left out too.
        assert_eq!(kept, vec![(start, 10 + P), (17 * M, 20)]);
        assert_eq!(pieces, vec![(16 * M + P, M - P)]);
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
    fn empty_range_does_nothing() {
        assert_eq!(plan(16 * M, 0, P, M), (vec![], vec![]));
    }

    #[test]
    fn dontneed_keeps_one_page_per_block_and_zeroes_the_rest() {
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
        let aligned = (map as usize).div_ceil(pmd) * pmd;
        let block = aligned as *mut u8;
        unsafe { std::ptr::write_bytes(block, 0xA5, pmd) };

        let kept = dontneed(block, pmd).unwrap();
        assert_eq!(kept, vec![(block, page)]);
        let bytes = unsafe { std::slice::from_raw_parts(block, pmd) };
        assert!(bytes[..page].iter().all(|&b| b == 0xA5));
        assert!(bytes[page..].iter().all(|&b| b == 0));

        unsafe { libc::munmap(map, map_len) };
    }

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
