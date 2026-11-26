//! Exact comparison of unwatched title bytes against an initialized card
//! staging copy. Ranges are offsets within one staging resource.
use std::ops::Range;

pub(super) const MAX_READ: usize = 4 << 20;
const BLOCK: usize = 256;
const MAX_RUNS: usize = 64;
const COARSE_BLOCK: usize = MAX_READ / MAX_RUNS;
const MAX_KNOWN: usize = 128;

#[derive(Default)]
pub(super) struct Initialized(Vec<Range<usize>>);

impl Initialized {
    pub(super) fn covers(&self, range: &Range<usize>) -> bool {
        let after = self.0.partition_point(|known| known.start <= range.start);
        after > 0 && self.0[after - 1].end >= range.end
    }

    /// Forgetting a range only forces its next upload to be complete.
    pub(super) fn mark(&mut self, range: Range<usize>) {
        let first = self.0.partition_point(|known| known.end < range.start);
        let last = self.0.partition_point(|known| known.start <= range.end);
        let mut joined = range;
        if first < last {
            joined.start = joined.start.min(self.0[first].start);
            joined.end = joined.end.max(self.0[last - 1].end);
        }
        self.0.splice(first..last, [joined]);
        if self.0.len() > MAX_KNOWN {
            let smallest = self
                .0
                .iter()
                .enumerate()
                .min_by_key(|(_, r)| r.len())
                .unwrap()
                .0;
            self.0.remove(smallest);
        }
    }
}

/// The existing CPU side is also the card's reference, including ranges
/// that are dirty but not sent yet. Store and report only changed blocks.
/// Never widen a range past the free portion authorized by the caller.
pub(super) fn replace(source: &[u8], held: &mut [u8], changed: impl FnMut(Range<usize>)) -> bool {
    assert_eq!(source.len(), held.len());
    assert!(source.len() <= MAX_READ);
    static FAST: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if *FAST
        .get_or_init(|| std::env::var_os("VICTORY_FAST_MIRROR_COMPARE").is_some_and(|v| v == "1"))
    {
        return replace_fast(source, held, changed);
    }
    replace_reference(source, held, changed)
}

fn replace_reference(
    source: &[u8],
    held: &mut [u8],
    mut changed: impl FnMut(Range<usize>),
) -> bool {
    if source == held {
        return false;
    }

    // A highly fragmented upload can cost more D3D commands than a full
    // copy. Every coarse block is checked too; equal blocks stay untouched.
    let mut runs: [Range<usize>; MAX_RUNS] = std::array::from_fn(|_| 0..0);
    let fine = differences(source, held, BLOCK, &mut runs);
    let count = fine.unwrap_or_else(|| differences(source, held, COARSE_BLOCK, &mut runs).unwrap());
    for r in &runs[..count] {
        held[r.clone()].copy_from_slice(&source[r.clone()]);
        changed(r.clone());
    }
    fine.is_none()
}

/// The staging mirror is host-mapped write-back memory. Both slices have
/// already been read or initialized by the normal guarded path. Skip equal
/// coarse blocks in bulk, then locate the same 256-byte runs as before.
/// Keep the coarse mask during the first pass so fragmentation never
/// requires rescanning the entire buffer.
pub(super) fn replace_fast(
    source: &[u8],
    held: &mut [u8],
    mut changed: impl FnMut(Range<usize>),
) -> bool {
    assert_eq!(source.len(), held.len());
    assert!(source.len() <= MAX_READ);
    let mut runs: [Range<usize>; MAX_RUNS] = std::array::from_fn(|_| 0..0);
    let mut count = 0;
    let mut coarse = false;
    let mut dirty_groups = 0u64;
    for (group, (new, old)) in source
        .chunks(COARSE_BLOCK)
        .zip(held.chunks(COARSE_BLOCK))
        .enumerate()
    {
        if equal_bulk(new, old) {
            continue;
        }
        dirty_groups |= 1u64 << group;
        if coarse {
            continue;
        }
        let base = group * COARSE_BLOCK;
        for (i, (n, o)) in new.chunks(BLOCK).zip(old.chunks(BLOCK)).enumerate() {
            let at = base + i * BLOCK;
            if n != o && !append_run(&mut runs, &mut count, at..at + n.len()) {
                coarse = true;
                break;
            }
        }
    }
    if coarse {
        count = 0;
        while dirty_groups != 0 {
            let first = dirty_groups.trailing_zeros() as usize;
            let at = first * COARSE_BLOCK;
            // At most 64 groups; this mask names only groups verified unequal.
            let end = (at + COARSE_BLOCK).min(source.len());
            let added = append_run(&mut runs, &mut count, at..end);
            debug_assert!(added);
            dirty_groups &= dirty_groups - 1;
        }
    }
    for r in &runs[..count] {
        held[r.clone()].copy_from_slice(&source[r.clone()]);
        changed(r.clone());
    }
    coarse
}

fn append_run(runs: &mut [Range<usize>], count: &mut usize, range: Range<usize>) -> bool {
    if *count != 0 && runs[*count - 1].end == range.start {
        runs[*count - 1].end = range.end;
        return true;
    }
    let Some(slot) = runs.get_mut(*count) else {
        return false;
    };
    *slot = range;
    *count += 1;
    true
}

#[cfg(target_arch = "x86_64")]
fn equal_bulk(a: &[u8], b: &[u8]) -> bool {
    use std::arch::x86_64::*;
    debug_assert_eq!(a.len(), b.len());
    let mut at = 0;
    while at + 128 <= a.len() {
        // SAFETY: the x86-64 baseline provides SSE2. Each unaligned load
        // is fully inside the two equal-length slices, even at a page edge.
        let mask = unsafe {
            let mut different = _mm_setzero_si128();
            for lane in 0..8 {
                let i = at + lane * 16;
                let lhs = _mm_loadu_si128(a.as_ptr().add(i).cast());
                let rhs = _mm_loadu_si128(b.as_ptr().add(i).cast());
                different = _mm_or_si128(different, _mm_xor_si128(lhs, rhs));
            }
            _mm_movemask_epi8(_mm_cmpeq_epi8(different, _mm_setzero_si128()))
        };
        if mask != 0xffff {
            return false;
        }
        at += 128;
    }
    a[at..] == b[at..]
}

#[cfg(not(target_arch = "x86_64"))]
fn equal_bulk(a: &[u8], b: &[u8]) -> bool {
    a == b
}

fn differences(
    source: &[u8],
    held: &[u8],
    block: usize,
    runs: &mut [Range<usize>],
) -> Option<usize> {
    let mut count = 0;
    for (i, (new, old)) in source.chunks(block).zip(held.chunks(block)).enumerate() {
        if new == old {
            continue;
        }
        let start = i * block;
        let end = start + new.len();
        if !append_run(runs, &mut count, start..end) {
            return None;
        }
    }
    Some(count)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bulk_equality_checks_every_lane_and_tail_without_reading_outside_the_slice() {
        let source: Vec<_> = (0..777).map(|n| (n * 31) as u8).collect();
        let mut held = source.clone();
        for (start, len) in [(0, 0), (1, 17), (3, 127), (7, 128), (15, 129), (5, 515)] {
            let end = start + len;
            assert!(equal_bulk(&source[start..end], &held[start..end]));
            held[end] ^= 4;
            assert!(equal_bulk(&source[start..end], &held[start..end]));
            held[end] ^= 4;
            for i in start..end {
                held[i] ^= 0x80;
                assert!(!equal_bulk(&source[start..end], &held[start..end]));
                held[i] ^= 0x80;
            }
        }
    }

    #[test]
    fn fast_refresh_has_identical_publications_at_boundaries_and_under_fragmentation() {
        let len = MAX_READ - 19;
        let expected: Vec<_> = (0..len + 21).map(|i| (i * 53) as u8).collect();
        let source = &expected[7..7 + len];
        for pattern in 0..5 {
            let mut old = expected.clone();
            let data = &mut old[7..7 + len];
            match pattern {
                0 => {}
                1 => {
                    for pos in [0, 127, BLOCK - 1, COARSE_BLOCK - 1, COARSE_BLOCK, len - 1] {
                        data[pos] ^= 1;
                    }
                }
                2 => {
                    for i in 0..64 {
                        data[2 * i * BLOCK] ^= 2;
                    }
                }
                3 => {
                    for i in 0..65 {
                        data[2 * i * BLOCK] ^= 2;
                    }
                    data[len - 1] ^= 2;
                }
                _ => {
                    for i in (0..len).step_by(3 * BLOCK) {
                        if i / COARSE_BLOCK % 3 != 1 {
                            data[i] ^= 4;
                        }
                    }
                }
            }
            let mut other = old.clone();
            let mut original = vec![];
            let mut fast = vec![];
            let coarse = replace_reference(source, &mut old[7..7 + len], |r| original.push(r));
            let actual = replace_fast(source, &mut other[7..7 + len], |r| fast.push(r));
            assert_eq!((actual, fast), (coarse, original), "pattern {pattern}");
            assert_eq!(other, expected);
        }
    }

    #[test]
    fn coverage_never_infers_initialization_across_a_hole() {
        let mut known = Initialized::default();
        known.mark(400..500);
        known.mark(10..90);
        assert!(known.covers(&(40..90)));
        assert!(!known.covers(&(11..499)));
        known.mark(90..400);
        assert!(known.covers(&(10..500)));
        assert!(!known.covers(&(9..500)));
        assert!(!known.covers(&(10..501)));
        for i in 0..160 {
            known.mark(1000 + 2 * i..1001 + 2 * i);
        }
        assert!(known.0.len() <= MAX_KNOWN);
        assert!(!known.covers(&(1101..1102))); // gap after a one-byte range
        assert!(known.covers(&(10..500)));
    }

    #[test]
    fn every_byte_is_compared_and_adjacent_changed_blocks_are_joined() {
        let mut held = vec![7; 3 * BLOCK + 3];
        let mut src = held.clone();
        for i in [BLOCK - 1, BLOCK, 3 * BLOCK + 2] {
            src[i] ^= 0x80;
        }
        let mut changed = vec![];
        replace(&src, &mut held, |r| changed.push(r));
        assert_eq!(held, src);
        assert_eq!(changed, [0..2 * BLOCK, 3 * BLOCK..3 * BLOCK + 3]);
        replace(&src, &mut held, |_| panic!("identical data"));
    }

    #[test]
    fn fragmentation_uses_only_dirty_coarse_blocks_and_bounds_commands() {
        let mut held = vec![0; MAX_READ];
        let mut src = held.clone();
        for i in (0..MAX_READ).step_by(BLOCK * 2) {
            if (i / COARSE_BLOCK).is_multiple_of(2) {
                src[i] = 1;
            }
        }
        let mut ranges = vec![];
        assert!(replace(&src, &mut held, |r| ranges.push(r)));
        assert_eq!(held, src);
        assert_eq!(ranges.len(), MAX_RUNS / 2);
        for (i, r) in ranges.iter().enumerate() {
            assert_eq!(*r, i * 2 * COARSE_BLOCK..(i * 2 + 1) * COARSE_BLOCK);
        }
    }

    #[test]
    fn model_with_first_fill_pending_gaps_and_zeroed_holes_matches_full_copy() {
        let size = MAX_READ + 37;
        let mut guest = vec![41; size];
        let mut held = vec![41; size]; // initial card memory still undefined
        let mut card = vec![0xcd; size];
        let mut known = Initialized::default();
        let mut refresh = |r: Range<usize>, guest: &[u8]| {
            let mut changed = 0;
            if !known.covers(&r) {
                held[r.clone()].copy_from_slice(&guest[r.clone()]);
                card[r.clone()].copy_from_slice(&held[r.clone()]);
                changed = r.len();
                known.mark(r);
            } else {
                for from in (r.start..r.end).step_by(MAX_READ) {
                    let to = (from + MAX_READ).min(r.end);
                    replace(&guest[from..to], &mut held[from..to], |delta| {
                        let at = from + delta.start..from + delta.end;
                        card[at.clone()].copy_from_slice(&guest[at]);
                        changed += delta.len();
                    });
                }
            }
            changed
        };
        assert_eq!(refresh(0..size, &guest), size);
        assert_eq!(refresh(0..size, &guest), 0);
        guest[4..12].fill(0); // unreadable guest bytes normalized by copy_mapped
        guest[MAX_READ + 36] = 0;
        guest[900..908].fill(0xf0); // a GPU write still pending, excluded
        assert_eq!(refresh(0..897, &guest), BLOCK);
        assert_eq!(refresh(912..size, &guest), (size - 912 - 1) % BLOCK + 1);
        assert!(card[900..908].iter().all(|b| *b == 41));
        assert_eq!(&card[0..897], &guest[0..897]);
        assert_eq!(&card[912..], &guest[912..]);
    }
}
