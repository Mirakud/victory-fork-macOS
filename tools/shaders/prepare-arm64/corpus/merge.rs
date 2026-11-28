//! Reassemble verified VSP1 records without retranslating the immutable seed.
use std::{collections::BTreeMap, fs, io, ops::Range, path::Path};

const MAX_BYTES: usize = 256 << 20;
const MAX_ENTRIES: usize = 8192;

fn invalid(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg)
}

struct Pack {
    data: Vec<u8>,
    entries: BTreeMap<[u8; 32], Range<usize>>,
}

impl Pack {
    fn read(path: &Path) -> io::Result<Self> {
        let len = fs::metadata(path)?.len();
        if !(76..=MAX_BYTES as u64).contains(&len) {
            return Err(invalid("invalid pack size"));
        }
        let data = fs::read(path)?;
        if data.len() != len as usize || !data.starts_with(b"VSP1\r\n\x1a\n") {
            return Err(invalid("invalid VSP1 header"));
        }
        let (body, checksum) = data.split_at(data.len() - 32);
        if blake3::hash(body).as_bytes() != checksum {
            return Err(invalid("invalid pack checksum"));
        }
        let count = u32::from_le_bytes(body[40..44].try_into().unwrap()) as usize;
        if !(1..=MAX_ENTRIES).contains(&count) {
            return Err(invalid("invalid entry count"));
        }
        let mut entries = BTreeMap::new();
        let mut cursor = 44;
        for _ in 0..count {
            let start = cursor;
            let header = body
                .get(cursor..cursor + 36)
                .ok_or_else(|| invalid("truncated entry"))?;
            let guest = header[..32].try_into().unwrap();
            let length = u32::from_le_bytes(header[32..36].try_into().unwrap()) as usize;
            cursor += 36;
            let end = cursor
                .checked_add(length)
                .ok_or_else(|| invalid("entry overflows"))?;
            let bytes = body
                .get(cursor..end)
                .ok_or_else(|| invalid("truncated VSH1"))?;
            if !bytes.starts_with(b"VSH1") || entries.insert(guest, start..end).is_some() {
                return Err(invalid("invalid or duplicate VSH1 record"));
            }
            cursor = end;
        }
        if cursor != body.len() {
            return Err(invalid("trailing pack data"));
        }
        Ok(Self { data, entries })
    }

    fn body(&self) -> &[u8] {
        &self.data[44..self.data.len() - 32]
    }
}

pub fn compose(fixed: &Path, current: &Path, delta: &Path, output: &Path) -> io::Result<()> {
    let seed = Pack::read(fixed)?;
    let loaded = if current == fixed {
        None
    } else {
        Some(Pack::read(current)?)
    };
    let prior = loaded.as_ref().unwrap_or(&seed);
    let delta = Pack::read(delta)?;
    let build = &seed.data[8..40];
    if &prior.data[8..40] != build
        || &delta.data[8..40] != build
        || !prior.body().starts_with(seed.body())
    {
        return Err(invalid("inconsistent translator or prior fixed entries"));
    }
    let count = prior.entries.len() + delta.entries.len();
    let bytes = prior.data.len() + delta.body().len();
    if count > MAX_ENTRIES || bytes > MAX_BYTES {
        return Err(invalid("combined pack exceeds runtime limits"));
    }
    let mut learned = BTreeMap::new();
    for (id, range) in &prior.entries {
        if !seed.entries.contains_key(id) {
            learned.insert(*id, &prior.data[range.clone()]);
        }
    }
    for (id, range) in &delta.entries {
        if seed.entries.contains_key(id)
            || learned.insert(*id, &delta.data[range.clone()]).is_some()
        {
            return Err(invalid("delta overlaps already prepared entries"));
        }
    }
    let mut pack = Vec::with_capacity(bytes);
    pack.extend_from_slice(&seed.data[..40]);
    pack.extend_from_slice(&(count as u32).to_le_bytes());
    pack.extend_from_slice(seed.body());
    for record in learned.into_values() {
        pack.extend_from_slice(record);
    }
    pack.extend_from_slice(blake3::hash(&pack).as_bytes());
    super::write_report(output, &pack)?;
    eprintln!(
        "fixed={} learned={} total={} bytes={}",
        seed.entries.len(),
        count - seed.entries.len(),
        count,
        bytes
    );
    Ok(())
}
