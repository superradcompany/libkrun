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

/// The host's base page size and the span one page table maps (a PMD entry).
fn page_and_pmd_size() -> (usize, usize) {
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    let page = if page > 0 { page as usize } else { 4096 };
    // One page table holds page / 8 entries (64-bit), each mapping one page.
    (page, page * (page / 8))
}

/// The pieces of `[start, start + len)` to discard, as `(start, len)`, and the pages left out of
/// it, as their start addresses: one piece per PMD block that lies wholly inside the range
/// (without its first page) and the parts at either end, with neighbours joined.
fn plan(start: usize, len: usize, page: usize, pmd: usize) -> (Vec<(usize, usize)>, Vec<usize>) {
    let end = start.saturating_add(len);
    let mut pieces: Vec<(usize, usize)> = Vec::new();
    let mut kept = Vec::new();
    fn push(s: usize, e: usize, pieces: &mut Vec<(usize, usize)>) {
        if s >= e {
            return;
        }
        match pieces.last_mut() {
            Some((ps, pl)) if *ps + *pl == s => *pl += e - s,
            _ => pieces.push((s, e - s)),
        }
    }

    let mut cur = start;
    while cur < end {
        let block = cur.div_ceil(pmd).saturating_mul(pmd);
        if block >= end || block.saturating_add(pmd) > end {
            push(cur, end, &mut pieces);
            break;
        }
        push(cur, block, &mut pieces);
        kept.push(block);
        push(block + page, block + pmd, &mut pieces);
        cur = block + pmd;
    }
    (pieces, kept)
}

/// `madvise(MADV_DONTNEED)` over `[addr, addr + len)`, never over a whole PMD block in one call.
/// Returns the pages it left resident, so a caller that needs the range zeroed can zero them.
pub(crate) fn dontneed(addr: *mut u8, len: usize) -> io::Result<Vec<*mut u8>> {
    let (page, pmd) = page_and_pmd_size();
    let base = addr as usize;
    let (pieces, kept) = plan(base, len, page, pmd);
    for (s, l) in pieces {
        if unsafe { libc::madvise(s as *mut libc::c_void, l, libc::MADV_DONTNEED) } < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(kept.into_iter().map(|p| p as *mut u8).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    const P: usize = 4096;
    const M: usize = 2 << 20;

    fn covers_whole_block(pieces: &[(usize, usize)]) -> bool {
        pieces.iter().any(|&(s, l)| s.div_ceil(M) * M + M <= s + l)
    }

    fn total(pieces: &[(usize, usize)]) -> usize {
        pieces.iter().map(|&(_, l)| l).sum()
    }

    #[test]
    fn one_aligned_block_is_one_call_without_its_first_page() {
        let (pieces, kept) = plan(16 * M, M, P, M);
        assert_eq!(pieces, vec![(16 * M + P, M - P)]);
        assert_eq!(kept, vec![16 * M]);
    }

    #[test]
    fn two_aligned_blocks_leave_a_page_out_of_each() {
        let (pieces, kept) = plan(16 * M, 2 * M, P, M);
        assert_eq!(pieces, vec![(16 * M + P, M - P), (17 * M + P, M - P)]);
        assert_eq!(kept, vec![16 * M, 17 * M]);
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
        assert_eq!(kept, vec![17 * M, 18 * M]);
        assert_eq!(
            pieces,
            vec![
                (start, 17 * M - start),
                (17 * M + P, M - P),
                (18 * M + P, start + len - 18 * M - P),
            ]
        );
        assert!(!covers_whole_block(&pieces));
        assert_eq!(total(&pieces) + kept.len() * P, len);
    }

    #[test]
    fn every_range_is_accounted_for_and_never_covers_a_block() {
        for start_pages in [0, 1, 255, 511, 512, 513] {
            for len_pages in [1, 511, 512, 513, 1024, 1025, 1536, 4096] {
                let start = 16 * M + start_pages * P;
                let len = len_pages * P;
                let (pieces, kept) = plan(start, len, P, M);
                assert!(!covers_whole_block(&pieces), "{start_pages} {len_pages}");
                assert_eq!(total(&pieces) + kept.len() * P, len);
                for &(s, l) in &pieces {
                    assert!(s >= start && s + l <= start + len);
                    assert!(kept.iter().all(|&k| k + P <= s || k >= s + l));
                }
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
        assert_eq!(kept, vec![block]);
        let bytes = unsafe { std::slice::from_raw_parts(block, pmd) };
        assert!(bytes[..page].iter().all(|&b| b == 0xA5));
        assert!(bytes[page..].iter().all(|&b| b == 0));

        unsafe { libc::munmap(map, map_len) };
    }
}
