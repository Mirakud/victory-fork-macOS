//! One immutable, preloaded snapshot of selected VSH1 translations.
//! VSP1: magic (8 bytes), ordinary cache build key (32), count (LE u32),
//! repeated full BLAKE3 of guest bytes (32), size (LE u32), VSH1 bytes,
//! followed by BLAKE3 of everything preceding the trailer.
use std::collections::HashMap;
use std::fs::File;
use std::io::{self, Read};
use std::ops::Range;
use std::path::Path;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Instant;
use victory_shader::emit::HostShader;

const MAGIC: &[u8; 8] = b"VSP1\r\n\x1a\n";
const MAX_BYTES: usize = 256 << 20;
const MAX_ENTRIES: usize = 8192;
static PACK: OnceLock<Pack> = OnceLock::new();

pub fn preload(path: &Path) {
    let began = Instant::now();
    // The optional executable-key mode reads the executable once. Keep that
    // I/O at startup too. Device-dependent fields will be checked on use.
    if std::env::var_os("VICTORY_SHADER_CACHE_KEY").is_some_and(|v| v == "exe") {
        let _ = super::build();
    }
    match File::open(path).and_then(Pack::read) {
        Ok(pack) => {
            tracing::info!(target: "victory_gpu::memory_watch", entries = pack.index.len(),
                bytes = pack.data.len(), build = %blake3::Hash::from(pack.build),
                preload_ms = began.elapsed().as_millis() as u64, "loaded read-only shader pack");
            let _ = PACK.set(pack);
        }
        Err(error) => {
            tracing::warn!(%error, path = %path.display(), "unavailable read-only shader pack")
        }
    }
}

pub(super) fn active() -> Option<&'static Pack> {
    PACK.get()
}

pub(crate) fn counters() -> [u64; 4] {
    active().map_or([0; 4], |pack| {
        pack.stats.each_ref().map(|c| c.load(Ordering::Relaxed))
    })
}

pub(super) struct Pack {
    data: Vec<u8>,
    build: [u8; 32],
    index: HashMap<[u8; 32], Range<usize>>,
    warned_mismatch: AtomicBool,
    stats: [AtomicU64; 4],
}

fn invalid(msg: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg)
}

impl Pack {
    fn read(mut file: File) -> io::Result<Self> {
        let len = file.metadata()?.len();
        if !(76..=MAX_BYTES as u64).contains(&len) {
            return Err(invalid("invalid shader pack size"));
        }
        let mut data = vec![0; len as usize];
        file.read_exact(&mut data)?;
        if file.read(&mut [0; 1])? != 0 {
            return Err(invalid("shader pack changed while loading"));
        }
        Self::parse(data)
    }

    fn parse(data: Vec<u8>) -> io::Result<Self> {
        if !(76..=MAX_BYTES).contains(&data.len()) || !data.starts_with(MAGIC) {
            return Err(invalid("wrong shader pack header or size"));
        }
        let (body, checksum) = data.split_at(data.len() - 32);
        if blake3::hash(body).as_bytes() != checksum {
            return Err(invalid("shader pack checksum mismatch"));
        }
        let build = body[8..40].try_into().unwrap();
        let count = u32::from_le_bytes(body[40..44].try_into().unwrap()) as usize;
        if !(1..=MAX_ENTRIES).contains(&count) {
            return Err(invalid("invalid shader pack entry count"));
        }
        let mut index = HashMap::with_capacity(count);
        let mut offset = 44;
        for _ in 0..count {
            let header = body
                .get(offset..offset + 36)
                .ok_or_else(|| invalid("truncated shader record"))?;
            let guest = header[..32].try_into().unwrap();
            let size = u32::from_le_bytes(header[32..].try_into().unwrap()) as usize;
            offset += 36;
            let end = offset
                .checked_add(size)
                .ok_or_else(|| invalid("oversized shader record"))?;
            if !body
                .get(offset..end)
                .is_some_and(|bytes| bytes.starts_with(super::MAGIC))
            {
                return Err(invalid("invalid VSH1 record"));
            }
            if index.insert(guest, offset..end).is_some() {
                return Err(invalid("duplicate guest shader in pack"));
            }
            offset = end;
        }
        if offset != body.len() {
            return Err(invalid("unexpected trailing shader records"));
        }
        Ok(Self {
            data,
            build,
            index,
            warned_mismatch: AtomicBool::new(false),
            stats: Default::default(),
        })
    }

    pub(super) fn translate(
        &self,
        build: impl FnOnce() -> [u8; 32],
        guest: &[u8],
        fallback: impl FnOnce(&[u8]) -> Result<HostShader, String>,
    ) -> Result<HostShader, String> {
        let began = Instant::now();
        let build = build();
        let found = if self.build == build {
            self.index
                .get(blake3::hash(guest).as_bytes())
                .and_then(|r| super::decode(&self.data[r.clone()]))
        } else {
            if !self.warned_mismatch.swap(true, Ordering::Relaxed) {
                tracing::warn!(target: "victory_gpu::memory_watch", packed = %blake3::Hash::from(self.build),
                    actual = %blake3::Hash::from(build), "read-only shader pack build mismatch");
            }
            None
        };
        self.stats[2].fetch_add(began.elapsed().as_micros() as u64, Ordering::Relaxed);
        if let Some(shader) = found {
            self.stats[0].fetch_add(1, Ordering::Relaxed);
            return Ok(shader);
        }
        self.stats[1].fetch_add(1, Ordering::Relaxed);
        let began = Instant::now();
        let result = fallback(guest);
        self.stats[3].fetch_add(began.elapsed().as_micros() as u64, Ordering::Relaxed);
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record() -> Vec<u8> {
        let mut blob = b"VSH1".to_vec();
        blob.extend(7u32.to_le_bytes()); // compute
        blob.extend(3u32.to_le_bytes());
        blob.extend(b"abc");
        blob.extend([0; 28]); // no bindings
        assert!(super::super::decode(&blob).is_some());
        blob
    }

    fn file(records: &[(&[u8], &[u8])]) -> Vec<u8> {
        let mut bytes = MAGIC.to_vec();
        bytes.extend([1; 32]);
        bytes.extend((records.len() as u32).to_le_bytes());
        for (guest, blob) in records {
            bytes.extend(blake3::hash(guest).as_bytes());
            bytes.extend((blob.len() as u32).to_le_bytes());
            bytes.extend(*blob);
        }
        bytes.extend(blake3::hash(&bytes).as_bytes());
        bytes
    }

    #[test]
    fn matching_build_and_guest_load_without_the_source_file() {
        let guest = b"guest shader";
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shaders.vsp");
        std::fs::write(&path, file(&[(guest, &record())])).unwrap();
        let pack = Pack::read(File::open(&path).unwrap()).unwrap();
        std::fs::remove_file(path).unwrap();
        let result = pack.translate(|| [1; 32], guest, |_| panic!("hit must not translate"));
        assert_eq!(result.unwrap().dxil, b"abc");
        let fallback = |_: &[u8]| Err::<HostShader, _>("translated".into());
        assert_eq!(
            pack.translate(|| [2; 32], guest, fallback).unwrap_err(),
            "translated"
        );
        assert_eq!(
            pack.translate(|| [1; 32], b"different shader", fallback)
                .unwrap_err(),
            "translated"
        );
        assert_eq!(pack.stats[0].load(Ordering::Relaxed), 1);
        assert_eq!(pack.stats[1].load(Ordering::Relaxed), 2);
        assert_eq!(std::fs::read_dir(dir).unwrap().count(), 0);
    }

    #[test]
    fn damaged_or_ambiguous_packs_are_rejected() {
        let record = record();
        let mut damaged = file(&[(b"a", &record)]);
        damaged[50] ^= 1;
        assert!(Pack::parse(damaged).is_err());
        assert!(Pack::parse(file(&[])).is_err());
        assert!(Pack::parse(file(&[(b"a", &record), (b"a", &record)])).is_err());
        assert!(Pack::parse(file(&[(b"a", b"bad!")])).is_err());
        let incomplete = file(&[(b"a", b"VSH1 invalid fields")]);
        let pack = Pack::parse(incomplete).unwrap();
        assert_eq!(
            pack.translate(|| [1; 32], b"a", |_| Err("fallback".into()))
                .unwrap_err(),
            "fallback"
        );
    }
}
