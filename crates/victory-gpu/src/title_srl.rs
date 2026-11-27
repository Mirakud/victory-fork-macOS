//! The intro streaming request list, read from the verified b99f title image.
//! `0x2298ba0` polls only the indexed prefix shared with the metadata count.
//! Its success does not imply the mission or cinematic has finished.
const REGISTRY: usize = 0xab442c8;
const INTRO: u32 = 0xbb00fe4e;
const SLOTS: usize = 6; // inline pointers end at the registry lock (0xab44300)
const STRIDE: usize = 0x84;
const EXAMPLES: usize = 3;
// Bound work under malformed or mid-update container headers. The metadata count is u16.
const MAX_ROWS: usize = u16::MAX as usize;
const SIGNATURES: &[(usize, &[u8])] = &[
    (0x227d010, &[0x8b, 0x15, 0xb6, 0x72, 0x8c, 0x08]), // registry count
    (0x227d032, &[0x4c, 0x8b, 0x08, 0x41, 0x39, 0x49, 0x1c]), // lookup key
    (
        0x2298c18,
        &[
            0x41, 0x80, 0x79, 0x7f, 0, 0x75, 0x0d, 0x41, 0xf6, 0x81, 0x80, 0, 0, 0, 1,
        ],
    ),
];

type Result<T> = std::result::Result<T, &'static str>;

fn get<const N: usize>(data: &[u8], offset: usize) -> [u8; N] {
    data[offset..offset + N].try_into().unwrap()
}

fn word(data: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(get(data, offset))
}

fn pointer(data: &[u8], offset: usize) -> usize {
    u64::from_le_bytes(get(data, offset)) as usize
}

fn copy(
    at: usize,
    offset: usize,
    out: &mut [u8],
    read: &mut impl FnMut(usize, &mut [u8]) -> bool,
) -> Result<()> {
    let start = at.checked_add(offset).ok_or("invalid address")?;
    if at < 0x10000
        || start
            .checked_add(out.len())
            .is_none_or(|end| end > isize::MAX as usize)
    {
        return Err("invalid address");
    }
    read(start, out).then_some(()).ok_or("unreadable memory")
}

#[derive(Debug, PartialEq)]
struct Pending {
    index: u32,
    words: [u32; 3],
    kind: u8,
    byte_7d: u8,
    byte_7f: u8,
    byte_80: u8,
    channel_cursors: [u32; 6], // 0 ModelInfo, 1 MapDataStore, 4 StaticBounds
    metadata_items: Option<u16>,
    metadata_mode: Option<u8>,
}

#[derive(Debug, PartialEq)]
struct Counts {
    metadata_entries: usize,
    indexed_entries: usize,
    eligible: usize,
    ready: usize,
    pending: usize,
    examples: Vec<Pending>,
}

#[derive(Debug, PartialEq)]
enum Sample {
    Absent {
        registered: Vec<u32>,
    },
    Intro {
        object: usize,
        state: u8,
        flags: u32,
        metadata: usize,
        runtime: usize,
        indices: usize,
        counts: Result<Option<Counts>>,
        poll_ready: Option<bool>,
    },
}

fn counts(
    header: &[u8],
    metadata: usize,
    read: &mut impl FnMut(usize, &mut [u8]) -> bool,
) -> Result<Counts> {
    let mut meta = [0; 10];
    copy(metadata, 0x10, &mut meta, read)?;
    let metadata_entries = u16::from_le_bytes(get(&meta, 8)) as usize;
    let metadata_rows = pointer(&meta, 0);
    let runtime_count = word(header, 0x34) as usize;
    let indexed_entries = word(header, 0x44) as usize;
    if runtime_count > word(header, 0x30) as usize || indexed_entries > word(header, 0x40) as usize
    {
        return Err("count exceeds capacity");
    }
    let eligible = metadata_entries.min(indexed_entries);
    let mut counts = Counts {
        metadata_entries,
        indexed_entries,
        eligible,
        ready: 0,
        pending: 0,
        examples: Vec::new(),
    };
    if eligible == 0 {
        return Ok(counts);
    }
    let mut indices = vec![0; eligible * 4];
    copy(pointer(header, 0x48), 0, &mut indices, read)?;
    let indices: Vec<u32> = indices
        .as_chunks::<4>()
        .0
        .iter()
        .map(|&entry| u32::from_le_bytes(entry))
        .collect();
    let last = *indices.iter().max().unwrap() as usize;
    if last >= runtime_count || last >= MAX_ROWS {
        return Err("runtime index out of bounds");
    }
    // A single bounded read also avoids thousands of Wine calls for one list.
    let mut rows = vec![0; (last + 1) * STRIDE];
    copy(pointer(header, 0x38), 0, &mut rows, read)?;
    for index in indices {
        let start = index as usize * STRIDE;
        let row = &rows[start..start + STRIDE];
        let kind = row[0x0c];
        let (byte_7d, byte_7f, byte_80) = (row[0x7d], row[0x7f], row[0x80]);
        if (kind != 0 && byte_7f == 0 && byte_80 & 1 == 0) || byte_7d & 1 != 0 {
            counts.ready += 1;
            continue;
        }
        counts.pending += 1;
        if counts.examples.len() < EXAMPLES {
            let detail = if (index as usize) < metadata_entries && metadata_rows != 0 {
                let mut detail = [0; 0x19];
                // The poll does not need these optional diagnostics to establish readiness.
                copy(metadata_rows, index as usize * 0x20, &mut detail, read)
                    .ok()
                    .map(|()| (u16::from_le_bytes(get(&detail, 8)), detail[0x18]))
            } else {
                None
            };
            counts.examples.push(Pending {
                index,
                words: std::array::from_fn(|i| word(row, 4 * i)),
                kind,
                byte_7d,
                byte_7f,
                byte_80,
                channel_cursors: std::array::from_fn(|i| word(row, 0x10 + 4 * i)),
                metadata_items: detail.map(|(items, _)| items),
                metadata_mode: detail.map(|(_, mode)| mode),
            });
        }
    }
    Ok(counts)
}

fn snapshot(base: usize, read: &mut impl FnMut(usize, &mut [u8]) -> bool) -> Result<Sample> {
    let mut registry = [0; 8 + SLOTS * 8];
    copy(base, REGISTRY, &mut registry, read)?;
    let capacity = word(&registry, 0) as usize;
    let count = word(&registry, 4) as usize;
    if count > capacity || capacity > SLOTS {
        return Err("invalid registry bounds");
    }
    let mut registered = Vec::new();
    for i in 0..count {
        let object = pointer(&registry, 8 + i * 8);
        let mut header = [0; 0x50];
        copy(object, 0, &mut header, read)?;
        let key = word(&header, 0x1c);
        if key != INTRO {
            registered.push(key);
            continue;
        }
        let state = header[0x28];
        let metadata = pointer(&header, 0x20);
        let counts = if state > 4 {
            Err("unrecognized SRL state")
        } else if metadata == 0 {
            Ok(None)
        } else {
            counts(&header, metadata, read).map(Some)
        };
        let poll_ready = counts
            .as_ref()
            .ok()
            .map(|c| c.as_ref().is_some_and(|c| c.pending == 0));
        return Ok(Sample::Intro {
            object,
            state,
            flags: word(&header, 0),
            metadata,
            runtime: pointer(&header, 0x38),
            indices: pointer(&header, 0x48),
            counts,
            poll_ready,
        });
    }
    Ok(Sample::Absent { registered })
}

fn verified(base: usize, read: &mut impl FnMut(usize, &mut [u8]) -> bool) -> bool {
    let mut buffer = [0; 16];
    SIGNATURES.iter().all(|&(rva, expected)| {
        let out = &mut buffer[..expected.len()];
        copy(base, rva, out, read).is_ok() && out == expected
    })
}

fn confirmed(base: usize, mut read: impl FnMut(usize, &mut [u8]) -> bool) -> Result<Sample> {
    let first = snapshot(base, &mut read)?;
    if first != snapshot(base, &mut read)? {
        return Err("changing SRL sample");
    }
    Ok(first)
}

#[cfg(windows)]
use std::time::Duration;

#[cfg(windows)]
pub(super) struct Observer {
    verified: Option<usize>,
    next: Duration,
    heartbeat: Duration,
    previous: Option<Result<Sample>>,
}

#[cfg(windows)]
impl Observer {
    pub(super) fn from_env() -> Option<Self> {
        std::env::var_os("VICTORY_TITLE_SRL_WATCH")
            .is_some_and(|v| v == "1")
            .then_some(Self {
                verified: None,
                next: Duration::ZERO,
                heartbeat: Duration::ZERO,
                previous: None,
            })
    }

    pub(super) fn sample(
        &mut self,
        base: usize,
        frame: u64,
        elapsed: Duration,
        mut read: impl FnMut(usize, &mut [u8]) -> bool,
    ) {
        if elapsed < self.next {
            return;
        }
        self.next = elapsed + Duration::from_secs(2);
        let result = if self.verified == Some(base) || verified(base, &mut read) {
            self.verified = Some(base);
            confirmed(base, read)
        } else {
            Err("unverified SRL sites")
        };
        if self.previous.as_ref() != Some(&result) || elapsed >= self.heartbeat {
            let elapsed_ms = elapsed.as_millis() as u64;
            tracing::info!(
                frame,
                elapsed_ms,
                intro_key = INTRO,
                ?result,
                "title SRL sample"
            );
            self.heartbeat = elapsed + Duration::from_secs(30);
            self.previous = Some(result);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    const BASE: usize = 0x400000;
    const REG: usize = BASE + REGISTRY;
    const OBJECT: usize = 0x010000;
    const META: usize = 0x020000;
    const ROW: usize = 0x030000;
    const IDS: usize = 0x040000;
    const DEFINITIONS: usize = 0x050000;

    type Image = BTreeMap<usize, Vec<u8>>;

    fn put(bytes: &mut [u8], offset: usize, value: u32) {
        bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
    }

    fn image() -> Image {
        let mut reg = vec![0; 8 + SLOTS * 8];
        put(&mut reg, 0, 4);
        put(&mut reg, 4, 1);
        put(&mut reg, 8, OBJECT as u32);
        let mut object = vec![0; 0x50];
        for (at, value) in [
            (0x1c, INTRO),
            (0x20, META as u32),
            (0x28, 1),
            (0x30, 4),
            (0x34, 4),
            (0x38, ROW as u32),
            (0x40, 4),
            (0x44, 4),
            (0x48, IDS as u32),
        ] {
            put(&mut object, at, value);
        }
        let mut meta = vec![0; 0x20];
        put(&mut meta, 0x10, DEFINITIONS as u32);
        put(&mut meta, 0x18, 3);
        let mut rows = vec![0; 4 * STRIDE];
        rows[0x7d] = 3;
        rows[STRIDE + 0xc] = 7;
        rows[STRIDE * 2 + 0xc] = 2;
        rows[STRIDE * 2 + 0x80] = 1;
        let mut defs = vec![0; 4 * 0x20];
        defs[2 * 0x20 + 8] = 24;
        defs[2 * 0x20 + 0x18] = 1;
        let mut indices = vec![0; 16];
        put(&mut indices, 0, 2);
        put(&mut indices, 4, 1);
        put(&mut indices, 8, 0);
        put(&mut indices, 12, 999);
        [
            (REG, reg),
            (OBJECT, object),
            (META, meta),
            (ROW, rows),
            (IDS, indices),
            (DEFINITIONS, defs),
        ]
        .into()
    }

    fn read(mem: &Image, at: usize, out: &mut [u8]) -> bool {
        let Some((&base, data)) = mem.range(..=at).next_back() else {
            return false;
        };
        let start = at - base;
        let Some(bytes) = data.get(start..start + out.len()) else {
            return false;
        };
        out.copy_from_slice(bytes);
        true
    }

    #[test]
    fn predicate_uses_both_paths_and_only_the_indexed_prefix() {
        let mut mem = image();
        let Sample::Intro {
            counts: Ok(Some(c)),
            ..
        } = confirmed(BASE, |a, b| read(&mem, a, b)).unwrap()
        else {
            panic!()
        };
        assert_eq!(
            (
                c.metadata_entries,
                c.indexed_entries,
                c.eligible,
                c.ready,
                c.pending
            ),
            (3, 4, 3, 2, 1)
        );
        assert_eq!(
            (
                c.examples[0].index,
                c.examples[0].metadata_items,
                c.examples[0].metadata_mode
            ),
            (2, Some(24), Some(1))
        );
        mem.get_mut(&ROW).unwrap()[STRIDE + 0x7f] = 1;
        let Sample::Intro {
            counts: Ok(Some(c)),
            ..
        } = confirmed(BASE, |a, b| read(&mem, a, b)).unwrap()
        else {
            panic!()
        };
        assert_eq!(c.pending, 2);
        mem.get_mut(&ROW).unwrap()[STRIDE + 0x7d] = 1;
        mem.get_mut(&ROW).unwrap()[STRIDE * 2 + 0x80] = 0;
        assert!(matches!(
            confirmed(BASE, |a, b| read(&mem, a, b)),
            Ok(Sample::Intro {
                poll_ready: Some(true),
                ..
            })
        ));
    }

    #[test]
    fn absence_no_metadata_empty_and_index_under_construction_are_distinct() {
        let mut mem = image();
        put(mem.get_mut(&OBJECT).unwrap(), 0x44, 0);
        let Sample::Intro {
            counts: Ok(Some(c)),
            ..
        } = confirmed(BASE, |a, b| read(&mem, a, b)).unwrap()
        else {
            panic!()
        };
        assert_eq!((c.metadata_entries, c.eligible, c.pending), (3, 0, 0));
        put(mem.get_mut(&META).unwrap(), 0x18, 0);
        let Sample::Intro {
            counts: Ok(Some(c)),
            ..
        } = confirmed(BASE, |a, b| read(&mem, a, b)).unwrap()
        else {
            panic!()
        };
        assert_eq!((c.metadata_entries, c.eligible, c.pending), (0, 0, 0));
        put(mem.get_mut(&OBJECT).unwrap(), 0x20, 0);
        assert!(matches!(
            confirmed(BASE, |a, b| read(&mem, a, b)),
            Ok(Sample::Intro {
                counts: Ok(None),
                poll_ready: Some(false),
                ..
            })
        ));
        put(mem.get_mut(&OBJECT).unwrap(), 0x1c, 15);
        assert_eq!(
            confirmed(BASE, |a, b| read(&mem, a, b)),
            Ok(Sample::Absent {
                registered: vec![15]
            })
        );
    }

    #[test]
    fn rejects_faults_out_of_bounds_and_changing_samples() {
        let mut mem = image();
        let mut seen = false;
        assert_eq!(
            confirmed(BASE, |a, b| {
                if a == REG {
                    if seen {
                        put(mem.get_mut(&OBJECT).unwrap(), 0x28, 2);
                    }
                    seen = true;
                }
                read(&mem, a, b)
            }),
            Err("changing SRL sample")
        );
        put(mem.get_mut(&IDS).unwrap(), 0, 4);
        assert!(matches!(
            confirmed(BASE, |a, b| read(&mem, a, b)),
            Ok(Sample::Intro {
                state: 2,
                counts: Err("runtime index out of bounds"),
                poll_ready: None,
                ..
            })
        ));
        put(mem.get_mut(&OBJECT).unwrap(), 0x34, 10);
        assert!(matches!(
            confirmed(BASE, |a, b| read(&mem, a, b)),
            Ok(Sample::Intro {
                state: 2,
                counts: Err("count exceeds capacity"),
                poll_ready: None,
                ..
            })
        ));
        put(mem.get_mut(&REG).unwrap(), 0, 100);
        assert_eq!(
            confirmed(BASE, |a, b| read(&mem, a, b)),
            Err("invalid registry bounds")
        );
        assert_eq!(confirmed(BASE, |_, _| false), Err("unreadable memory"));
        assert!(!verified(usize::MAX, &mut |_, _| panic!()));
        let sigs: Image = SIGNATURES
            .iter()
            .map(|(r, s)| (BASE + r, s.to_vec()))
            .collect();
        assert!(verified(BASE, &mut |a, b| read(&sigs, a, b)));
        assert!(!verified(BASE, &mut |_, _| true));
    }
}
