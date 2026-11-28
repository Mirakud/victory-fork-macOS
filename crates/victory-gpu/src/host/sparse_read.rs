//! Reader strategy hints, never page contents or a cached VM map.
use std::hash::{DefaultHasher, Hash, Hasher};
use std::ops::Range;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

pub(super) const LARGE: usize = 64 << 10;
const PAGE: usize = 4 << 10;
const SHARDS: usize = 32;
const SLOTS: usize = 64;

type Key = (usize, usize);
pub(super) struct Hints([Mutex<[Key; SLOTS]>; SHARDS]);

impl Hints {
    pub(super) const fn new() -> Self {
        Self([const { Mutex::new([(0, 0); SLOTS]) }; SHARDS])
    }

    fn bucket(&self, address: usize, len: usize) -> (&Mutex<[Key; SLOTS]>, usize) {
        let mut hash = DefaultHasher::new();
        (address, len).hash(&mut hash);
        let slot = hash.finish() as usize % (SHARDS * SLOTS);
        (&self.0[slot / SLOTS], slot % SLOTS)
    }

    pub(super) fn has(&self, address: usize, len: usize) -> bool {
        let (shard, slot) = self.bucket(address, len);
        shard.lock().unwrap_or_else(|e| e.into_inner())[slot] == (address, len)
    }

    pub(super) fn set(&self, address: usize, len: usize, sparse: bool) {
        let (shard, slot) = self.bucket(address, len);
        let mut entries = shard.lock().unwrap_or_else(|e| e.into_inner());
        if sparse {
            entries[slot] = (address, len);
        } else if entries[slot] == (address, len) {
            // Another range may have replaced this slot during the walk.
            entries[slot] = (0, 0);
        }
    }
}

pub(super) static HINTS: Hints = Hints::new();
// Small descriptors are much more numerous. Their failures must never
// evict the already learned strategy for a large source.
pub(super) static SMALL_HINTS: Hints = Hints::new();

pub(super) fn small_enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_SMALL_SPARSE_READ").is_some_and(|v| v == "1"))
}

pub(super) fn table(len: usize, large_on: bool, small_on: bool) -> Option<&'static Hints> {
    if len >= LARGE {
        large_on.then_some(&HINTS)
    } else {
        small_on.then_some(&SMALL_HINTS)
    }
}

pub(super) fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_SPARSE_READ_HINT").is_some_and(|v| v == "1"))
}

pub(super) fn class(address: usize, len: usize) -> usize {
    if one_page(address, len) {
        0
    } else if len < LARGE {
        1
    } else if len < 1 << 20 {
        2
    } else {
        3
    }
}

pub(super) fn one_page(address: usize, len: usize) -> bool {
    len <= PAGE - address % PAGE
}

// Actual initial ReadProcessMemory calls and failures, hinted walks,
// entirely readable hinted walks, skipped same-fragment retries, requested bytes.
static COUNTS: [[AtomicU64; 6]; 4] = [const { [const { AtomicU64::new(0) }; 6] }; 4];

fn count(class: usize, values: [u64; 6]) {
    for (counter, value) in COUNTS[class].iter().zip(values) {
        if value != 0 {
            counter.fetch_add(value, Ordering::Relaxed);
        }
    }
}

pub(super) fn initial(address: usize, len: usize, success: bool) {
    count(
        class(address, len),
        [1, u64::from(!success), 0, 0, 0, len as u64],
    );
}

pub(super) fn hinted(address: usize, len: usize, readable: bool, skipped: u64) {
    count(
        class(address, len),
        [0, 0, 1, u64::from(readable), skipped, len as u64],
    );
}

pub(super) fn skipped(address: usize, len: usize, retries: u64) {
    if retries != 0 {
        count(class(address, len), [0, 0, 0, 0, retries, 0]);
    }
}

pub(super) fn take_counts() -> [[u64; 6]; 4] {
    COUNTS
        .each_ref()
        .map(|row| row.each_ref().map(|n| n.swap(0, Ordering::Relaxed)))
}

fn parts(address: usize, len: usize, step: usize, mut f: impl FnMut(usize, usize)) {
    let mut offset = 0;
    while offset < len {
        let at = address.wrapping_add(offset);
        let n = (step - at % step).min(len - offset);
        f(at, n);
        offset += n;
    }
}

/// Split an unreadable span using the original 64 KiB / 4 KiB boundaries.
/// With deduplication, an identical kernel read that failed in this copy
/// is conclusive for that one page fragment. A future copy reads it again.
pub(super) fn recover(
    address: usize,
    len: usize,
    known_failed: bool,
    dedup: bool,
    read: &mut impl FnMut(usize, usize) -> bool,
    zero: &mut impl FnMut(usize, usize),
) -> u64 {
    if known_failed && dedup && one_page(address, len) {
        zero(address, len);
        return 2;
    }
    let mut skipped = 0;
    parts(address, len, LARGE, |at, n| {
        if read(at, n) {
            return;
        }
        if dedup && one_page(at, n) {
            zero(at, n);
            skipped += 1;
        } else {
            parts(at, n, PAGE, |at, n| {
                if !read(at, n) {
                    zero(at, n)
                }
            });
        }
    });
    skipped
}

/// Read entirely from the current VM map. 'true' means every region was
/// readable and read successfully; the caller can then discard the hint.
pub(super) fn walk(
    address: usize,
    len: usize,
    dedup: bool,
    mut query: impl FnMut(usize) -> Option<(usize, bool)>,
    mut read: impl FnMut(usize, usize) -> bool,
    mut zero: impl FnMut(usize, usize),
) -> (bool, u64) {
    let mut offset = 0;
    let mut readable = true;
    let mut skipped = 0;
    while offset < len {
        let at = address.wrapping_add(offset);
        let remaining = len - offset;
        let Some((length, can_read)) = query(at).filter(|(n, _)| *n > 0) else {
            skipped += recover(at, remaining, false, dedup, &mut read, &mut zero);
            return (false, skipped);
        };
        let n = remaining.min(length);
        if !can_read {
            zero(at, n);
            readable = false;
        } else if !read(at, n) {
            skipped += recover(at, n, true, dedup, &mut read, &mut zero);
            readable = false;
        }
        offset += n;
    }
    (readable, skipped)
}

// Only the bounds of earlier successful reads are hints. Each such span must
// pass a fresh kernel read before its bytes can be used. Everything else,
// including every former hole, is discovered again from the current map.
const MAX_RUNS: usize = 64;
// Attempts to read learned spans, successes, bytes read that way, VM map queries.
static GROUPED: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];

pub(super) fn take_grouped_counts() -> [u64; 4] {
    GROUPED.each_ref().map(|n| n.swap(0, Ordering::Relaxed))
}

type SavedRuns = (Key, Arc<[Range<usize>]>);
const RUN_BUCKETS: usize = 4096;
struct ReadRuns([Mutex<Option<SavedRuns>>; RUN_BUCKETS]);

impl ReadRuns {
    const fn new() -> Self {
        Self([const { Mutex::new(None) }; RUN_BUCKETS])
    }

    fn bucket(&self, key: Key) -> &Mutex<Option<SavedRuns>> {
        let mut hash = DefaultHasher::new();
        key.hash(&mut hash);
        &self.0[hash.finish() as usize % self.0.len()]
    }

    fn walk(
        &self,
        address: usize,
        len: usize,
        mut query: impl FnMut(usize) -> Option<(usize, bool)>,
        mut read: impl FnMut(usize, usize) -> bool,
        mut zero: impl FnMut(usize, usize),
    ) -> (bool, u64) {
        let Some(end) = address.checked_add(len) else {
            return walk(address, len, true, query, read, zero);
        };
        let key = (address, len);
        let bucket = self.bucket(key);
        let previous = bucket
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .filter(|(k, _)| *k == key)
            .map(|(_, runs)| Arc::clone(runs));
        let mut runs = Vec::<Range<usize>>::new();
        let mut checked_read = |at, n| {
            let ok = read(at, n);
            if ok && runs.len() <= MAX_RUNS {
                if let Some(last) = runs.last_mut()
                    && last.end == at
                {
                    last.end += n;
                } else {
                    runs.push(at..at + n);
                }
            }
            ok
        };
        let (mut attempts, mut successes, mut bytes) = (0, 0, 0);
        let mut complete = true;
        let mut skipped = 0;
        let mut visit = |start, end, bulk| {
            if start == end {
                return;
            }
            let size = end - start;
            if bulk {
                attempts += 1;
                if checked_read(start, size) {
                    successes += 1;
                    bytes += size as u64;
                    return;
                }
            }
            let (ok, count) = walk(
                start,
                size,
                true,
                &mut query,
                &mut checked_read,
                &mut zero,
            );
            complete &= ok;
            skipped += count;
        };
        let mut cursor = address;
        for run in previous.as_deref().unwrap_or_default() {
            visit(cursor, run.start, false);
            visit(run.start, run.end, true);
            cursor = run.end;
        }
        visit(cursor, end, false);
        for (counter, n) in GROUPED.iter().zip([attempts, successes, bytes]) {
            counter.fetch_add(n, Ordering::Relaxed);
        }
        let keep = !runs.is_empty() && runs.len() <= MAX_RUNS;
        if keep && previous.as_deref() != Some(runs.as_slice()) {
            *bucket.lock().unwrap_or_else(|e| e.into_inner()) = Some((key, runs.into()));
        } else if !keep && previous.is_some() {
            let mut entry = bucket.lock().unwrap_or_else(|e| e.into_inner());
            if entry.as_ref().is_some_and(|(k, _)| *k == key) {
                *entry = None;
            }
        }
        (complete, skipped)
    }
}

pub(super) fn walk_source(
    address: usize,
    len: usize,
    dedup: bool,
    query: impl FnMut(usize) -> Option<(usize, bool)>,
    read: impl FnMut(usize, usize) -> bool,
    zero: impl FnMut(usize, usize),
) -> (bool, u64) {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    static RUNS: ReadRuns = ReadRuns::new();
    if dedup
        && len >= 1 << 20
        && *ENABLED
            .get_or_init(|| std::env::var_os("VICTORY_SPARSE_READ_RUNS").is_some_and(|v| v == "1"))
    {
        walk_windows(&RUNS, address, len, query, read, zero)
    } else {
        walk(address, len, dedup, query, read, zero)
    }
}

// Overlapping requests share the complete, aligned megabytes between their
// endpoints. The table holds only the bounds of earlier successful reads.
// Every hit re-reads those bytes and every gap examines the current map.
fn walk_windows(
    runs: &ReadRuns,
    address: usize,
    len: usize,
    mut query: impl FnMut(usize) -> Option<(usize, bool)>,
    mut read: impl FnMut(usize, usize) -> bool,
    mut zero: impl FnMut(usize, usize),
) -> (bool, u64) {
    const WINDOW: usize = 1 << 20;
    let Some(end) = address.checked_add(len) else {
        return walk(address, len, true, query, read, zero);
    };
    let mut cursor = address;
    if len == 0 {
        return (true, 0);
    }
    let mut observed = query(address)
        .map(|(length, readable)| (address, address.saturating_add(length), readable));
    let (mut complete, mut skipped, mut queries) = (true, 0, 1);
    while cursor < end {
        // A single VM query can describe a hole spanning many windows.
        // Honor its full extent inside this copy, just as walk() does.
        if let Some((_, stop, false)) = observed
            && cursor < stop
        {
            let to = stop.min(end);
            zero(cursor, to - cursor);
            complete = false;
            cursor = to;
            continue;
        }
        let n = (WINDOW - cursor % WINDOW).min(end - cursor);
        let mut current = |at| {
            if let Some((start, stop, can_read)) = observed
                && start <= at && at < stop
            {
                return Some((stop - at, can_read));
            }
            queries += 1;
            let region = query(at);
            observed = region.map(|(length, can_read)| (at, at.saturating_add(length), can_read));
            region
        };
        let (readable, retries) = runs.walk(cursor, n, &mut current, &mut read, &mut zero);
        complete &= readable;
        skipped += retries;
        cursor += n;
    }
    GROUPED[3].fetch_add(queries, Ordering::Relaxed);
    (complete, skipped)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    // Distinct views may be adjacent and readable. A failed bulk read may
    // still leave prefix bytes in the caller's buffer.
    fn exercise(cache: &ReadRuns, pages: &[bool], value: u8) -> (usize, usize) {
        let n = pages.len() * PAGE;
        let dest = RefCell::new(vec![0xa5; n]);
        let (mut reads, mut queries) = (0, 0);
        let (all, _) = cache.walk(
            0,
            n,
            |at| {
                queries += 1;
                Some((PAGE - at % PAGE, pages[at / PAGE]))
            },
            |at, n| {
                reads += 1;
                let mut bytes = dest.borrow_mut();
                for i in at..at + n {
                    if !pages[i / PAGE] {
                        return false;
                    }
                    bytes[i] = value;
                }
                true
            },
            |at, n| dest.borrow_mut()[at..at + n].fill(0),
        );
        assert_eq!(all, pages.iter().all(|&readable| readable));
        for (page, bytes) in pages.iter().zip(dest.borrow().as_chunks::<PAGE>().0) {
            assert!(bytes.iter().all(|&b| b == if *page { value } else { 0 }));
        }
        (queries, reads)
    }

    #[test]
    fn overlapping_windows_read_new_mappings_and_contents() {
        const WINDOW: usize = 1 << 20;
        let logical = 4 * WINDOW;
        let mut output = vec![0; logical];
        let runs = ReadRuns::new();
        for (start, end, hole, value) in [
            (13, logical - 61, 1, 0x32),
            (PAGE + 3, logical - PAGE, 4, 0x57),
            (WINDOW + 7, logical - 81, 5, 0xaa),
        ] {
            output.fill(0xee);
            let storage = RefCell::new(&mut output);
            let len = end - start;
            let readable = |at: usize| at / LARGE % 13 != hole;
            let (complete, _) = walk_windows(
                &runs,
                start,
                len,
                |at| Some((LARGE - at % LARGE, readable(at))),
                |at, n| {
                    let mut bytes = storage.borrow_mut();
                    // A failed kernel copy can have overwritten its prefix.
                    for page in (at / PAGE)..=(at + n - 1) / PAGE {
                        let lo = at.max(page * PAGE);
                        let hi = (at + n).min((page + 1) * PAGE);
                        if !readable(lo) {
                            return false;
                        }
                        bytes[lo..hi].fill(value);
                    }
                    true
                },
                |at, n| storage.borrow_mut()[at..at + n].fill(0),
            );
            assert!(!complete);
            assert!(output[..start].iter().chain(&output[end..]).all(|&v| v == 0xee));
            for (i, b) in output[start..end].iter().enumerate() {
                assert_eq!(*b, if readable(start + i) { value } else { 0 });
            }
        }
    }

    #[test]
    fn saved_runs_recheck_holes_splits_and_remapped_values() {
        let cache = ReadRuns::new();
        let mut pages = [true; 16];
        pages[7] = false;
        assert_eq!(exercise(&cache, &pages, 11), (16, 15));
        assert_eq!(exercise(&cache, &pages, 22), (1, 2));
        pages[7] = true;
        pages[13] = false;
        exercise(&cache, &pages, 33);
        assert_eq!(exercise(&cache, &pages, 44), (1, 2));
        pages.fill(true);
        exercise(&cache, &pages, 55);
        assert_eq!(cache.bucket((0, 16 * PAGE)).lock().unwrap().as_ref().unwrap().1.as_ref(), &[0..16 * PAGE]);
    }

    #[test]
    fn fragmented_ranges_and_concurrent_readers_get_their_own_bytes() {
        let cache = ReadRuns::new();
        let pages = (0..MAX_RUNS * 2 + 1)
            .map(|i| i % 2 == 0)
            .collect::<Vec<_>>();
        exercise(&cache, &pages, 1);
        assert!(
            cache
                .bucket((0, pages.len() * PAGE))
                .lock()
                .unwrap()
                .is_none()
        );
        std::thread::scope(|scope| {
            for thread in 0..3 {
                let cache = &cache;
                scope.spawn(move || {
                    for turn in 0..12 {
                        let mut pages = [true; 16];
                        pages[(turn + thread) % 16] = false;
                        exercise(cache, &pages, 1 + (turn + thread) as u8);
                    }
                });
            }
        });
    }

    #[test]
    fn a_mapping_lost_between_query_and_read_is_recovered() {
        let cache = ReadRuns::new();
        let available = std::cell::Cell::new(true);
        let dest = RefCell::new(vec![0xff; 3 * PAGE]);
        let (all, _) = cache.walk(
            0,
            3 * PAGE,
            |at| {
                if at == PAGE {
                    available.set(false);
                }
                Some((PAGE, true))
            },
            |at, len| {
                if at / PAGE == 1 && !available.get() {
                    return false;
                }
                dest.borrow_mut()[at..at + len].fill(0x55);
                true
            },
            |at, n| dest.borrow_mut()[at..at + n].fill(0),
        );
        assert!(!all);
        let dest = dest.into_inner();
        assert!(dest[..PAGE].iter().all(|&b| b == 0x55));
        assert!(dest[PAGE..2 * PAGE].iter().all(|&b| b == 0));
        assert!(dest[2 * PAGE..].iter().all(|&b| b == 0x55));
    }

    #[test]
    fn a_failed_page_fragment_is_zeroed_without_rereading_any_byte() {
        let mut calls = vec![];
        let mut zeroed = vec![];
        assert_eq!(
            recover(
                PAGE + 13,
                137,
                true,
                true,
                &mut |at, n| {
                    calls.push((at, n));
                    false
                },
                &mut |at, n| zeroed.push((at, n))
            ),
            2
        );
        assert!(calls.is_empty());
        assert_eq!(zeroed, [(PAGE + 13, 137)]);
        assert_eq!(
            recover(
                PAGE + 13,
                137,
                true,
                false,
                &mut |at, n| {
                    calls.push((at, n));
                    false
                },
                &mut |_, _| {}
            ),
            0
        );
        assert_eq!(calls, [(PAGE + 13, 137), (PAGE + 13, 137)]);
    }

    #[test]
    fn a_short_read_across_a_page_keeps_its_readable_fragment() {
        let memory = RefCell::new([0x33; 4]);
        let at = PAGE - 2;
        recover(
            at,
            4,
            true,
            true,
            &mut |addr, n| {
                if addr + n <= PAGE {
                    memory.borrow_mut()[addr - at..addr - at + n].fill(0xaa);
                    true
                } else {
                    false
                }
            },
            &mut |addr, n| memory.borrow_mut()[addr - at..addr - at + n].fill(0),
        );
        assert_eq!(*memory.borrow(), [0xaa, 0xaa, 0, 0]);
    }

    #[test]
    fn a_hinted_walk_sees_holes_mapping_and_data_changes_and_can_be_forgotten() {
        let hints = Hints::new();
        let base = 0x100000;
        let dest = RefCell::new(vec![0xff; LARGE]);
        let mapped = std::cell::Cell::new(false);
        let value = std::cell::Cell::new(7);
        let run = || {
            walk(
                base,
                LARGE,
                true,
                |addr| {
                    let n = PAGE - (addr - base) % PAGE;
                    Some((n, mapped.get() || (addr - base) / PAGE != 2))
                },
                |addr, n| {
                    let offset = addr - base;
                    dest.borrow_mut()[offset..offset + n].fill(value.get());
                    true
                },
                |addr, n| dest.borrow_mut()[addr - base..addr - base + n].fill(0),
            )
        };
        let (readable, skipped) = run();
        assert!(!readable);
        assert_eq!(skipped, 0);
        hints.set(base, LARGE, !readable);
        assert!(hints.has(base, LARGE));
        assert!(dest.borrow()[2 * PAGE..3 * PAGE].iter().all(|v| *v == 0));
        assert!(dest.borrow()[3 * PAGE..].iter().all(|v| *v == 7));
        mapped.set(true);
        value.set(41);
        let (readable, _) = run();
        hints.set(base, LARGE, !readable);
        assert!(readable && !hints.has(base, LARGE));
        assert!(dest.borrow().iter().all(|v| *v == 41));
    }

    #[test]
    fn mapping_race_or_query_failure_recovers_live_content() {
        let dest = RefCell::new(vec![0xee; 2 * PAGE]);
        let mut read_calls = vec![];
        let (fully, skipped) = walk(
            0,
            2 * PAGE,
            true,
            |at| if at == 0 { Some((PAGE, true)) } else { None },
            |at, n| {
                read_calls.push((at, n));
                dest.borrow_mut()[at..at + n].fill(0xcc); // includes partial bytes from a failed read
                at >= PAGE
            },
            |at, n| dest.borrow_mut()[at..at + n].fill(0),
        );
        assert!(!fully);
        assert_eq!(skipped, 2);
        assert_eq!(read_calls, [(0, PAGE), (PAGE, PAGE)]);
        assert!(dest.borrow()[..PAGE].iter().all(|v| *v == 0));
        assert!(dest.borrow()[PAGE..].iter().all(|v| *v == 0xcc));
    }

    #[test]
    fn counters_distinguish_actual_initial_failures_from_hinted_walks_and_skipped_calls() {
        let hints = Hints::new();
        let base = 4 * LARGE;
        // 100 copies of a range with a hole, then a remap, then a fully readable copy.
        for iteration in 0..102 {
            let hole = iteration < 100;
            let used_hint = hints.has(base, LARGE);
            if !used_hint {
                initial(base, LARGE, !hole);
                if !hole {
                    continue;
                }
            }
            let (readable, retries) = walk(
                base,
                LARGE,
                true,
                |at| Some((PAGE, !hole || at != base + PAGE)),
                |_, _| true,
                |_, _| {},
            );
            if used_hint {
                hinted(base, LARGE, readable, retries);
            }
            hints.set(base, LARGE, !readable);
        }
        // 128 small failed reads previously issued two additional, identical reads each.
        for _ in 0..128 {
            initial(base + 17, 512, false);
            let retries = recover(
                base + 17,
                512,
                true,
                true,
                &mut |_, _| unreachable!(),
                &mut |_, _| {},
            );
            skipped(base + 17, 512, retries);
        }
        let totals = take_counts();
        assert_eq!(totals[0], [128, 128, 0, 0, 256, 128 * 512]);
        assert_eq!(totals[2], [2, 1, 100, 1, 0, (102 * LARGE) as u64]);
        assert!(!hints.has(base, LARGE));
    }

    #[test]
    fn small_strategy_is_optional_and_cannot_displace_a_large_hint() {
        assert!(table(128, true, false).is_none());
        assert!(table(LARGE, false, true).is_none());
        let small = table(LARGE - 1, false, true).unwrap();
        let large = table(LARGE, true, false).unwrap();
        assert!(!std::ptr::eq(small, large));
        assert!(std::ptr::eq(table(1, true, true).unwrap(), small));
        assert!(std::ptr::eq(table(2 * LARGE, true, true).unwrap(), large));
    }

    #[test]
    fn hints_are_exact_bounded_and_collision_safe() {
        assert!(std::mem::size_of::<Hints>() < 40 * 1024);
        let hints = Hints::new();
        let slots: Vec<_> = (0..5000).map(|n| (n * 4096, LARGE)).collect();
        let first = slots[0];
        let (shard, slot) = hints.bucket(first.0, first.1);
        let second = (1..200_000)
            .map(|n| (n * 4096, LARGE))
            .find(|k| {
                let (s, n) = hints.bucket(k.0, k.1);
                std::ptr::eq(s, shard) && n == slot
            })
            .unwrap();
        hints.set(first.0, first.1, true);
        hints.set(second.0, second.1, true);
        assert!(!hints.has(first.0, first.1));
        assert!(hints.has(second.0, second.1));
        assert!(!hints.has(second.0, second.1 + 1));
        hints.set(first.0, first.1, false);
        assert!(hints.has(second.0, second.1));
        for (at, n) in slots {
            hints.set(at, n, true);
        }
        assert_eq!(
            [
                class(13, 10),
                class(PAGE - 1, 3),
                class(5, LARGE),
                class(8, 1 << 20)
            ],
            [0, 1, 2, 3]
        );
    }
}
