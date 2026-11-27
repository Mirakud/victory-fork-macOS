//! Read-only counters for the 256-byte title pool with configuration key 0x3f678dcf.
//! The aggregate release generation is incremented when the title returns an entry.
//! A sample can span in-flight operations. It does not identify individual entries.
#[path = "title_srl.rs"]
mod srl;

const POOL_RVA: usize = 0x98d79a8;
const HEADER_SIZE: usize = 0x28;
const EMPTY: u32 = 0x1fffe;

// Check the allocation site's global load, counter read and sentinel comparison.
// The second signature checks the constructor's publication of the same global.
const SIGNATURES: &[(usize, &[u8])] = &[
    (0x6395e4, &[0x48, 0x8b, 0x0d, 0xbd, 0xe3, 0x29, 0x09]),
    (
        0x6395f7,
        &[
            0x48, 0x8b, 0x41, 0x20, 0x3d, 0xfe, 0xff, 0x01, 0, 0x74, 0x6d,
        ],
    ),
    (0x5ad8fac, &[0x48, 0x89, 0x3d, 0xf5, 0xe9, 0xdf, 0x03]),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Sample {
    pool: usize,
    storage: u64,
    capacity: u32,
    occupied: u32,
    head: u32,
    returns: u32,
}

fn verify_image(base: usize, size: usize, mut read: impl FnMut(usize, &mut [u8]) -> bool) -> bool {
    if size != 0xb06d000 || base.checked_add(size).is_none() {
        return false;
    }
    let mut bytes = [0u8; 16];
    SIGNATURES.iter().all(|&(rva, signature)| {
        let actual = &mut bytes[..signature.len()];
        read(base + rva, actual) && actual == signature
    })
}

fn word(bytes: &[u8; HEADER_SIZE], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap())
}

fn snapshot(
    base: usize,
    mut read: impl FnMut(usize, &mut [u8]) -> bool,
) -> Result<Sample, &'static str> {
    snapshot_at(base, POOL_RVA, 256, &mut read)
}

fn snapshot_at(
    base: usize,
    slot: usize,
    stride: u32,
    mut read: impl FnMut(usize, &mut [u8]) -> bool,
) -> Result<Sample, &'static str> {
    let global = base.checked_add(slot).ok_or("invalid image base")?;
    let mut pointer = [0; 8];
    if !read(global, &mut pointer) {
        return Err("unreadable pool reference");
    }
    let pool = u64::from_le_bytes(pointer) as usize;
    if pool == 0 {
        return Err("pool not initialized");
    }
    if !pool.is_multiple_of(8) || pool > isize::MAX as usize - HEADER_SIZE {
        return Err("invalid pool reference");
    }
    let get = |bytes: &[u8; HEADER_SIZE]| -> Result<Sample, &'static str> {
        let storage = u64::from_le_bytes(bytes[..8].try_into().unwrap());
        let capacity = word(bytes, 12);
        let occupied = word(bytes, 16);
        let head = word(bytes, 32);
        if storage == 0
            || word(bytes, 8) != stride
            || !(1..=EMPTY).contains(&capacity)
            || occupied > capacity
            || (head != EMPTY && head >= capacity)
        {
            return Err("invalid pool header");
        }
        Ok(Sample {
            pool,
            storage,
            capacity,
            occupied,
            head,
            returns: word(bytes, 36),
        })
    };
    let mut bytes = [0; HEADER_SIZE];
    if !read(pool, &mut bytes) {
        return Err("unreadable header");
    }
    let first = get(&bytes)?;
    if !read(pool, &mut bytes) || !read(global, &mut pointer) {
        return Err("unreadable verification");
    }
    if first != get(&bytes)? || pool != u64::from_le_bytes(pointer) as usize {
        return Err("changing sample");
    }
    Ok(first)
}

impl Sample {
    fn returns_since(&self, previous: Option<Self>) -> Option<u32> {
        let other = previous?;
        (other.pool == self.pool
            && other.storage == self.storage
            && other.capacity == self.capacity)
            .then(|| self.returns.wrapping_sub(other.returns))
    }
}

#[cfg(windows)]
pub(super) fn start() {
    if !std::env::var_os("VICTORY_TITLE_POOL_WATCH").is_some_and(|v| v == "1") {
        return;
    }
    if let Err(error) = std::thread::Builder::new()
        .name("title-pool-watch".into())
        .spawn(watch)
    {
        tracing::warn!(%error, "title pool observer could not start");
    }
}

#[cfg(windows)]
fn watch() {
    use std::time::{Duration, Instant};
    use windows_sys::Win32::System::{
        LibraryLoader::GetModuleHandleA,
        ProcessStatus::{K32GetModuleInformation, MODULEINFO},
        Threading::GetCurrentProcess,
    };
    let read = |at: usize, bytes: &mut [u8]| crate::host::read_into(at as *const u8, bytes);
    let interval = Duration::from_millis(250);
    let started = Instant::now();
    let mut last_tick = started;
    let mut previous = None;
    let mut srl = srl::Observer::from_env();
    let mut verified = None;
    let mut failures = 0u64;
    let mut last_error = None;
    let mut missed_ticks = 0u64;
    let mut sequence = 0u64;
    loop {
        sequence += 1;
        let now = Instant::now();
        missed_ticks += (now.duration_since(last_tick).as_millis() / interval.as_millis())
            .saturating_sub(1) as u64;
        last_tick = now;
        let frame = crate::queue::presented();
        let result = (|| {
            // SAFETY: GetModuleHandleA borrows a process module by its terminated name.
            let module =
                unsafe { GetModuleHandleA(c"victory-image-b99f876b74d4a77f.dll".as_ptr().cast()) };
            if module.is_null() {
                verified = None;
                return Err("title image absent");
            }
            let base = module as usize;
            if verified != Some(base) {
                // SAFETY: info is writable for the exact size supplied to K32.
                let mut info: MODULEINFO = unsafe { std::mem::zeroed() };
                let ok = unsafe {
                    K32GetModuleInformation(
                        GetCurrentProcess(),
                        module,
                        &mut info,
                        size_of::<MODULEINFO>() as u32,
                    )
                };
                if ok == 0
                    || info.lpBaseOfDll as usize != base
                    || !verify_image(base, info.SizeOfImage as usize, read)
                {
                    return Err("unverified title image");
                }
                verified = Some(base);
            }
            if srl.is_some() && sequence.is_multiple_of(8) {
                for (name, slot, stride) in [
                    ("building", 0xa9ec938, 224),
                    ("object", 0xabbe2e8, 272),
                    ("entitybatch", 0xa9ec948, 352),
                ] {
                    if let Ok(s) = snapshot_at(base, slot, stride, read) {
                        tracing::info!(target: "victory_gpu::title_pool", frame, name,
                            header = format_args!("{:#x}", s.pool), stride, capacity = s.capacity,
                            occupied = s.occupied, returns = s.returns, head_empty = s.head == EMPTY,
                            "map pool sample");
                    }
                }
            }
            if let Some(observer) = &mut srl {
                observer.sample(base, frame, started.elapsed(), read);
            }
            snapshot(base, read)
        })();
        let elapsed_ms = started.elapsed().as_millis() as u64;
        match result {
            Ok(sample) => {
                let returns_since = sample.returns_since(previous);
                tracing::info!(
                    sequence,
                    frame,
                    elapsed_ms,
                    failures,
                    missed_ticks,
                    capacity = sample.capacity,
                    occupied = sample.occupied,
                    head_empty = sample.head == EMPTY,
                    returns = sample.returns,
                    ?returns_since,
                    "title pool sample"
                );
                previous = Some(sample);
                last_error = None;
            }
            Err(reason) => {
                failures += 1;
                // A gap invalidates deltas even when a later header has the same address.
                previous = None;
                if last_error != Some(reason) || sequence.is_multiple_of(40) {
                    tracing::info!(
                        sequence,
                        frame,
                        elapsed_ms,
                        failures,
                        missed_ticks,
                        reason,
                        "title pool unavailable"
                    );
                }
                last_error = Some(reason);
            }
        }
        std::thread::sleep(interval.saturating_sub(now.elapsed()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: usize = 0x10000;
    const POOL: usize = 0x30000;

    fn header() -> [u8; HEADER_SIZE] {
        let mut bytes = [0; HEADER_SIZE];
        bytes[0..8].copy_from_slice(&0x5000u64.to_le_bytes());
        for (index, value) in [(8, 256u32), (12, 40), (16, 31), (32, 9), (36, 5)] {
            bytes[index..index + 4].copy_from_slice(&value.to_le_bytes());
        }
        bytes
    }

    fn sample(buf: [u8; HEADER_SIZE]) -> Result<Sample, &'static str> {
        snapshot(BASE, |addr, dst| {
            if addr == BASE + POOL_RVA {
                dst.copy_from_slice(&(POOL as u64).to_le_bytes());
            } else if addr == POOL {
                dst.copy_from_slice(&buf);
            } else {
                return false;
            }
            true
        })
    }

    #[test]
    fn verifies_size_bounds_and_all_signatures() {
        let mut visited = 0;
        let mut read = |at, dst: &mut [u8]| {
            let Some(&(_, signature)) = SIGNATURES.iter().find(|(rva, _)| BASE + rva == at) else {
                return false;
            };
            dst.copy_from_slice(signature);
            visited += 1;
            true
        };
        assert!(!verify_image(BASE, 4, &mut read));
        assert!(verify_image(BASE, 0xb06d000, &mut read));
        assert_eq!(visited, SIGNATURES.len());
        assert!(!verify_image(BASE, 0xb06d000, |_, _| true));
        assert!(!verify_image(usize::MAX, 0xb06d000, |_, _| panic!()));
    }

    #[test]
    fn occupancy_empty_head_and_release_delta() {
        let mut bytes = header();
        let first = sample(bytes).unwrap();
        assert_eq!((first.capacity, first.occupied, first.returns), (40, 31, 5));
        bytes[16..20].copy_from_slice(&40u32.to_le_bytes());
        bytes[32..36].copy_from_slice(&EMPTY.to_le_bytes());
        bytes[36..40].copy_from_slice(&11u32.to_le_bytes());
        let full = sample(bytes).unwrap();
        assert_eq!(full.head, EMPTY);
        assert_eq!(full.returns_since(Some(first)), Some(6));
        assert_eq!(full.returns_since(None), None);
        assert_eq!(full.returns_since(Some(Sample { pool: 8, ..first })), None);
        assert_eq!(
            full.returns_since(Some(Sample {
                returns: u32::MAX,
                ..first
            })),
            Some(12)
        );
    }

    #[test]
    fn rejects_bad_header_or_missing_read() {
        for (at, value) in [(8, 255u32), (12, 0), (12, EMPTY + 1), (16, 41), (32, 40)] {
            let mut bytes = header();
            bytes[at..at + 4].copy_from_slice(&value.to_le_bytes());
            assert_eq!(sample(bytes), Err("invalid pool header"));
        }
        assert_eq!(
            snapshot(BASE, |_, _| false),
            Err("unreadable pool reference")
        );
        assert_eq!(snapshot(BASE, |_, _| true), Err("pool not initialized"));
        assert_eq!(
            snapshot(BASE, |at, dst| {
                if at != BASE + POOL_RVA {
                    return false;
                }
                dst.copy_from_slice(&(POOL as u64).to_le_bytes());
                true
            }),
            Err("unreadable header")
        );
    }

    #[test]
    fn concurrent_release_and_replaced_pool_are_detected() {
        for change_pointer in [true, false] {
            let mut reads = 0;
            assert_eq!(
                snapshot(BASE, |at, dst| {
                    reads += 1;
                    if at == BASE + POOL_RVA {
                        let pool = if change_pointer && reads == 4 {
                            POOL + 8
                        } else {
                            POOL
                        };
                        dst.copy_from_slice(&(pool as u64).to_le_bytes());
                    } else {
                        dst.copy_from_slice(&header());
                        if !change_pointer && reads == 3 {
                            dst[36] += 1;
                        }
                    }
                    true
                }),
                Err("changing sample")
            );
        }
    }
}
