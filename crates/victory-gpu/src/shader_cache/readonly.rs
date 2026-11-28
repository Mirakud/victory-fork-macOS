//! Immutable, preloaded VSH1 translations: a primary pack and an optional overlay.
//! VSP1: magic (8 bytes), ordinary cache build key (32), count (LE u32),
//! repeated full BLAKE3 of guest bytes (32), size (LE u32), VSH1 bytes,
//! followed by BLAKE3 of everything preceding the trailer.
use std::collections::HashMap;
use std::fs::File;
use std::io::{self, Read};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Instant;
use victory_shader::emit::HostShader;

#[cfg(windows)]
#[path = "belobug.rs"]
pub(crate) mod belobug;
#[cfg(windows)]
#[path = "bvh_scan.rs"]
pub(crate) mod bvh_scan;
#[cfg(windows)]
#[path = "verified_store.rs"]
mod verified_store;
#[path = "refusal.rs"]
mod refusal;
#[path = "store.rs"]
mod store;

const MAGIC: &[u8; 8] = b"VSP1\r\n\x1a\n";
const MAX_BYTES: usize = 256 << 20;
const MAX_ENTRIES: usize = 8192;
static PACKS: OnceLock<Packs> = OnceLock::new();

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
            let overlay = std::env::var_os("VICTORY_READONLY_SHADER_OVERLAY");
            let _ = PACKS.set(Packs::new(pack, overlay.as_deref().map(Path::new)));
        }
        Err(error) => {
            tracing::warn!(%error, path = %path.display(), "unavailable read-only shader pack")
        }
    }
}

pub(super) fn active() -> Option<&'static Packs> {
    static STORE_ONLY: OnceLock<Option<Packs>> = OnceLock::new();
    PACKS.get().or_else(|| {
        STORE_ONLY.get_or_init(|| Some(Packs {
            primary: None,
            overlay: None,
            store: Some(store::configured()?),
            #[cfg(windows)]
            verified_store: verified_store::Store::configured(super::build()),
            warned_mismatch: AtomicBool::new(false),
            stats: Default::default(),
        })).as_ref()
    })
}

/// Optional offline preparation with the same executable and host settings
/// as the run. The ordinary mutable cache reports conversion failures with
/// this context; keep its result and only learn the verified emitter error.
pub fn warm(container: &[u8]) -> Result<bool, String> {
    const CONVERSION: &str = "converter shader.dxil v1: ";
    let Some(cache) = refusal::Cache::configured() else {
        return super::warm(container);
    };
    if super::CACHE.get().is_none() {
        return super::warm(container);
    }
    let build = super::build();
    let guest = *blake3::hash(container).as_bytes();
    if let Some(error) = cache.lookup(build, guest) {
        return Err(format!("{CONVERSION}{error}"));
    }
    let began = Instant::now();
    let result = super::warm(container);
    if let Err(error) = &result && let Some(emitter) = error.strip_prefix(CONVERSION) {
        cache.observe(build, guest, emitter, began.elapsed().as_micros() as u64);
    }
    result
}

#[cfg(feature = "diagnostics")]
pub(crate) fn counters() -> [u64; 9] {
    active().map_or([0; 9], |pack| {
        pack.stats.each_ref().map(|c| c.load(Ordering::Relaxed))
    })
}

struct Pack {
    data: Vec<u8>,
    build: [u8; 32],
    index: HashMap<[u8; 32], Range<usize>>,
}

pub(super) struct Packs {
    primary: Option<Pack>,
    overlay: Option<Pack>,
    store: Option<PathBuf>,
    #[cfg(windows)]
    verified_store: Option<verified_store::Store>,
    warned_mismatch: AtomicBool,
    // Total hits, misses, lookup time, fallback time, hits from the overlay.
    // Store hits, filesystem time, lookups and unreadable/malformed records.
    stats: [AtomicU64; 9],
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
        Ok(Self { data, build, index })
    }

    fn shader(&self, guest: &[u8; 32]) -> Option<HostShader> {
        let range = self.index.get(guest)?;
        super::decode(&self.data[range.clone()])
    }
}

impl Packs {
    fn new(primary: Pack, overlay: Option<&Path>) -> Self {
        let overlay = overlay.and_then(|path| {
            let began = Instant::now();
            match File::open(path).and_then(Pack::read) {
                Ok(pack) if pack.build == primary.build => {
                    tracing::info!(target: "victory_gpu::memory_watch", entries = pack.index.len(),
                        bytes = pack.data.len(), build = %blake3::Hash::from(pack.build),
                        preload_ms = began.elapsed().as_millis() as u64, "loaded read-only shader overlay");
                    Some(pack)
                }
                Ok(pack) => {
                    tracing::warn!(target: "victory_gpu::memory_watch", path = %path.display(),
                        packed = %blake3::Hash::from(pack.build), primary = %blake3::Hash::from(primary.build),
                        "read-only shader overlay build mismatch");
                    None
                }
                Err(error) => {
                    tracing::warn!(target: "victory_gpu::memory_watch", %error, path = %path.display(),
                        "unavailable read-only shader overlay");
                    None
                }
            }
        });
        #[cfg(windows)]
        let verified_store = verified_store::Store::configured(primary.build);
        Self { #[cfg(windows)] verified_store, primary: Some(primary), overlay, store: store::configured(),
            warned_mismatch: AtomicBool::new(false), stats: Default::default() }
    }

    pub(super) fn translate(
        &self,
        build: impl FnOnce() -> [u8; 32],
        guest: &[u8],
        fallback: impl FnOnce(&[u8]) -> Result<HostShader, String>,
    ) -> Result<HostShader, String> {
        let began = Instant::now();
        let build = build();
        let hash = blake3::hash(guest);
        let matching = self.primary.as_ref().is_none_or(|primary| primary.build == build);
        let packed = self.primary.as_ref().and_then(|primary| {
            if !matching {
                if !self.warned_mismatch.swap(true, Ordering::Relaxed) {
                    tracing::warn!(target: "victory_gpu::memory_watch", packed = %blake3::Hash::from(primary.build),
                        actual = %blake3::Hash::from(build), "read-only shader pack build mismatch");
                }
                return None;
            }
            primary.shader(hash.as_bytes()).or_else(|| {
                let shader = self.overlay.as_ref()?.shader(hash.as_bytes())?;
                self.stats[4].fetch_add(1, Ordering::Relaxed);
                Some(shader)
            })
        });
        let found = packed.or_else(|| {
            #[cfg(windows)]
            if let Some(store) = &self.verified_store {
                if self.primary.as_ref().is_some_and(|pack| pack.build != build) { return None; }
                self.stats[7].fetch_add(1, Ordering::Relaxed);
                let started = Instant::now();
                let shader = store.shader(&hash);
                self.stats[6].fetch_add(started.elapsed().as_micros() as u64, Ordering::Relaxed);
                if shader.is_some() { self.stats[5].fetch_add(1, Ordering::Relaxed); }
                return shader;
            }
            let root = self.store.as_deref()?;
            self.stats[7].fetch_add(1, Ordering::Relaxed);
            match store::lookup(root, &build, guest, &self.stats[6]) {
                Ok(shader) => {
                    let hits = self.stats[5].fetch_add(1, Ordering::Relaxed) + 1;
                    if hits.is_power_of_two() {
                        tracing::info!(target: "victory_gpu::memory_watch", hits,
                            "reused prepared shaders from the on-demand store");
                    }
                    Some(shader)
                }
                Err(error) => {
                    if error.kind() != io::ErrorKind::NotFound
                        && self.stats[8].fetch_add(1, Ordering::Relaxed) == 0
                    {
                        tracing::warn!(target: "victory_gpu::memory_watch", %error,
                            "unreadable shader store entry; using normal translation fallback");
                    }
                    None
                }
            }
        });
        self.stats[2].fetch_add(began.elapsed().as_micros() as u64, Ordering::Relaxed);
        if let Some(shader) = found {
            self.stats[0].fetch_add(1, Ordering::Relaxed);
            return Ok(shader);
        }
        self.stats[1].fetch_add(1, Ordering::Relaxed);
        let refusals = matching.then(refusal::Cache::configured).flatten();
        if let Some(error) = refusals.and_then(|cache| cache.lookup(build, *hash.as_bytes())) {
            return Err(error);
        }
        let began = Instant::now();
        let result = fallback(guest);
        let fallback_us = began.elapsed().as_micros() as u64;
        self.stats[3].fetch_add(fallback_us, Ordering::Relaxed);
        if matching && let Ok(shader) = &result {
            record_missing_input(hash, guest, shader.stage, fallback_us);
        } else if let (Some(cache), Err(error)) = (refusals, &result) {
            cache.observe(build, *hash.as_bytes(), error, fallback_us);
        }
        result
    }
}

// Save successful misses for the native offline translator. The rendering
// thread never waits on disk or on the recorder when its queue is full.
fn record_missing_input(hash: blake3::Hash, guest: &[u8], stage: victory_shader::HwStage, fallback_us: u64) {
    use std::collections::HashSet;
    use std::sync::mpsc::{SyncSender, sync_channel};

    struct Captured {
        hash: blake3::Hash,
        bytes: Vec<u8>,
        stage: victory_shader::HwStage,
        fallback_us: u64,
    }

    static RECORDER: OnceLock<Option<SyncSender<Captured>>> = OnceLock::new();
    let sender = RECORDER.get_or_init(|| {
        let directory = std::path::PathBuf::from(std::env::var_os("VICTORY_SHADER_MISSES_DIR")?);
        let (sender, receiver) = sync_channel::<Captured>(8);
        std::thread::Builder::new()
            .name("shader-inputs".into())
            .spawn(move || {
                let mut seen = HashSet::new();
                let mut written = 0;
                for capture in receiver {
                    let bytes = &capture.bytes;
                    if seen.len() >= 512 || written + bytes.len() > (64 << 20) {
                        continue;
                    }
                    let hash = capture.hash;
                    if seen.contains(&hash) {
                        continue;
                    }
                    let target = directory.join(hash.to_hex().as_str());
                    let temporary = target.with_extension("partial");
                    if let Err(error) = std::fs::write(&temporary, bytes)
                        .and_then(|()| std::fs::rename(&temporary, &target))
                    {
                        tracing::warn!(%error, "could not save shader input");
                        continue;
                    }
                    tracing::info!(target: "victory_gpu::memory_watch", guest = %hash,
                        stage = ?capture.stage, fallback_us = capture.fallback_us,
                        bytes = bytes.len(), "captured shader translation");
                    written += bytes.len();
                    seen.insert(hash);
                }
            })
            .ok()?;
        Some(sender)
    });
    if let Some(sender) = sender && guest.len() <= 4 << 20 {
        let _ = sender.try_send(Captured { hash, bytes: guest.to_vec(), stage, fallback_us });
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
        let pack = Packs::new(Pack::read(File::open(&path).unwrap()).unwrap(), None);
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
    fn overlay_supplies_only_missing_translations_and_counts_one_lookup() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("overlay.vsp");
        let primary_record = record();
        let mut overlay_record = record();
        overlay_record[12..15].copy_from_slice(b"xyz");
        std::fs::write(&path, file(&[(b"shared", &overlay_record), (b"added", &overlay_record)])).unwrap();
        let primary = Pack::parse(file(&[(b"shared", &primary_record)])).unwrap();
        let packs = Packs::new(primary, Some(&path));
        std::fs::remove_file(&path).unwrap();

        let hit = |guest: &[u8]| packs.translate(|| [1; 32], guest, |_| panic!("cached shader translated"));
        assert_eq!(hit(b"shared").unwrap().dxil, b"abc");
        assert_eq!(hit(b"added").unwrap().dxil, b"xyz");
        let fallback = |_: &[u8]| Err::<HostShader, _>("translated".into());
        assert_eq!(packs.translate(|| [1; 32], b"missing", fallback).unwrap_err(), "translated");
        assert_eq!(packs.translate(|| [2; 32], b"added", fallback).unwrap_err(), "translated");
        assert_eq!(packs.stats[0].load(Ordering::Relaxed), 2);
        assert_eq!(packs.stats[1].load(Ordering::Relaxed), 2);
        assert_eq!(packs.stats[4].load(Ordering::Relaxed), 1);
    }

    #[test]
    fn missing_damaged_or_foreign_overlay_keeps_the_primary_and_fallback() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("overlay.vsp");
        let blob = record();
        let mut damaged = file(&[(b"added", &blob)]);
        damaged[44] ^= 1;
        let mut foreign = file(&[(b"added", &blob)]);
        foreign[8] ^= 1;
        let end = foreign.len() - 32;
        let checksum = blake3::hash(&foreign[..end]);
        foreign[end..].copy_from_slice(checksum.as_bytes());
        for file_contents in [None, Some(damaged), Some(foreign)] {
            if let Some(bytes) = file_contents {
                std::fs::write(&path, bytes).unwrap();
            }
            let primary = Pack::parse(file(&[(b"shared", &blob)])).unwrap();
            let packs = Packs::new(primary, Some(&path));
            assert!(packs.overlay.is_none());
            assert_eq!(packs.translate(|| [1; 32], b"shared", |_| panic!()).unwrap().dxil, b"abc");
            assert_eq!(packs.translate(|| [1; 32], b"added", |_| Err("translated".into())).unwrap_err(), "translated");
            assert_eq!(packs.stats[4].load(Ordering::Relaxed), 0);
        }
    }

    #[test]
    fn disk_store_follows_both_packs_and_recovers_from_invalid_entries() {
        use victory_pipeline::Cache;
        let directory = tempfile::tempdir().unwrap();
        let store = Cache::open(directory.path()).unwrap();
        let build = [1; 32];
        let guest = b"disk only";
        let valid_shader = || {
            let mut shader = super::super::decode(&record()).unwrap();
            shader.dxil = include_bytes!("../host/card_compare.dxil").to_vec();
            shader
        };
        // Publish with the ordinary converter exactly as the ARM64 helper does.
        super::super::cached(&store, &build, guest, |_| Ok(valid_shader())).unwrap();
        let path = store::key(&build, guest).path_under(directory.path());
        let valid = std::fs::read(&path).unwrap();
        let overlay_path = directory.path().join("overlay.vsp");
        std::fs::write(&overlay_path, file(&[(b"overlay", &record())])).unwrap();
        let primary = Pack::parse(file(&[(b"primary", &record())])).unwrap();
        let mut packs = Packs::new(primary, Some(&overlay_path));
        packs.store = Some(directory.path().to_owned());

        for guest in [b"primary".as_slice(), b"overlay"] {
            assert_eq!(packs.translate(|| build, guest, |_| panic!()).unwrap().dxil, b"abc");
        }
        assert_eq!(packs.stats[7].load(Ordering::Relaxed), 0);
        assert_eq!(packs.translate(|| build, guest, |_| panic!()).unwrap().dxil, valid_shader().dxil);
        assert_eq!(packs.stats[5].load(Ordering::Relaxed), 1);
        let error = |_: &[u8]| Err::<HostShader, _>("original compiler error".into());
        assert_eq!(packs.translate(|| [2; 32], guest, error).unwrap_err(), "original compiler error");
        assert_eq!(packs.translate(|| build, b"other guest", error).unwrap_err(), "original compiler error");

        for malformed in [b"VSH1".as_slice(), &record(), &valid[..valid.len() - 1]] {
            std::fs::write(&path, malformed).unwrap();
            assert_eq!(packs.translate(|| build, guest, error).unwrap_err(), "original compiler error");
            assert_eq!(std::fs::read(&path).unwrap(), malformed);
        }
        let oversized = std::fs::File::create(&path).unwrap();
        oversized.set_len((32 << 20) + 1).unwrap();
        drop(oversized);
        assert_eq!(packs.translate(|| build, guest, |_| Ok(valid_shader())).unwrap().dxil, valid_shader().dxil);
        assert_eq!(packs.stats[8].load(Ordering::Relaxed), 4);
        // A readable publication becomes available in the same process.
        std::fs::write(&path, valid).unwrap();
        assert_eq!(packs.translate(|| build, guest, |_| panic!()).unwrap().dxil, valid_shader().dxil);
        assert_eq!(packs.stats[5].load(Ordering::Relaxed), 2);
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
        let pack = Packs::new(Pack::parse(incomplete).unwrap(), None);
        assert_eq!(
            pack.translate(|| [1; 32], b"a", |_| Err("fallback".into()))
                .unwrap_err(),
            "fallback"
        );
    }
}
