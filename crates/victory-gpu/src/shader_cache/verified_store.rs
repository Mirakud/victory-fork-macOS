//! An authenticated third tier for the native ARM64 translations of build 578b.
//! The inventory names the guest container, content-addressed store path, size
//! and SHA-256 of every published VSH1, including its binding metadata.
use super::invalid;
use std::collections::HashMap;
use std::fs::File;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use victory_shader::emit::HostShader;
use windows_sys::Win32::Security::Cryptography::{BCryptHash, BCRYPT_SHA256_ALG_HANDLE};

const BUILD: &str = "578b25415d58675d5ba8637836bf4f93baf92f43cb501d6a0365e01c3cec4218";
const INVENTORY_SIZE: usize = 5_838_789;
const INVENTORY_BLAKE3: &str = "e8b6a29069b1011c93670b6152c33eb47b8b00e2b3ae42c43f69b1032bc28845";

struct Entry {
    cache_key: [u8; 32],
    sha256: [u8; 32],
    size: usize,
}

pub(super) struct Store {
    root: PathBuf,
    entries: HashMap<blake3::Hash, Entry>,
    hits: AtomicU64,
    warned: AtomicBool,
}

fn read_sized(path: &Path, size: usize) -> io::Result<Vec<u8>> {
    let file = File::open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.len() != size as u64 {
        return Err(invalid("unexpected shader store file size"));
    }
    let mut bytes = Vec::with_capacity(size);
    file.take((size + 1) as u64).read_to_end(&mut bytes)?;
    if bytes.len() != size { return Err(invalid("shader store file changed during read")); }
    Ok(bytes)
}

fn hex(value: &str) -> io::Result<blake3::Hash> {
    blake3::Hash::from_hex(value).map_err(|_| invalid("invalid digest in shader inventory"))
}

impl Store {
    pub(super) fn configured(build: [u8; 32]) -> Option<Self> {
        let root = PathBuf::from(std::env::var_os("VICTORY_READONLY_SHADER_STORE")?);
        if build != *blake3::Hash::from_hex(BUILD).unwrap().as_bytes() { return None; }
        match Self::open(&root) {
            Ok(store) => {
                tracing::info!(target: "victory_gpu::memory_watch", entries = store.entries.len(),
                    "opened verified read-only shader store");
                Some(store)
            }
            Err(error) => {
                tracing::warn!(target: "victory_gpu::memory_watch", %error, "unavailable verified shader store");
                None
            }
        }
    }

    fn open(root: &Path) -> io::Result<Self> {
        let bytes = read_sized(&root.join("translations.tsv"), INVENTORY_SIZE)?;
        if blake3::hash(&bytes) != hex(INVENTORY_BLAKE3)? {
            return Err(invalid("unrecognized shader store inventory checksum"));
        }
        let inventory = std::str::from_utf8(&bytes).map_err(|_| invalid("invalid shader inventory text"))?;
        let mut entries = HashMap::with_capacity(25_638);
        for row in inventory.lines().skip(1) {
            let mut fields = row.split('\t');
            let guest = fields.next().unwrap_or_default();
            if fields.nth(2) != Some("ok") { continue; }
            let cache_key = hex(fields.next().unwrap_or_default())?;
            let size: usize = fields.next().and_then(|n| n.parse().ok()).ok_or_else(|| invalid("invalid shader size"))?;
            let sha256 = hex(fields.nth(1).unwrap_or_default())?;
            if fields.nth(1) != Some("1") { continue; }
            entries.insert(hex(guest)?, Entry { cache_key: *cache_key.as_bytes(), size, sha256: *sha256.as_bytes() });
        }
        if entries.len() != 25_638 { return Err(invalid("incomplete shader inventory")); }
        Ok(Self { root: root.join("shaders"), entries, hits: AtomicU64::new(0), warned: AtomicBool::new(false) })
    }

    pub(super) fn shader(&self, guest: &blake3::Hash) -> Option<HostShader> {
        let entry = self.entries.get(guest)?;
        let key = blake3::Hash::from(entry.cache_key).to_hex();
        let path = self.root.join(&key[..2]).join(&key[2..]);
        let shader = read_sized(&path, entry.size).and_then(|bytes| {
            let mut digest = [0u8; 32];
            // SAFETY: CNG reads one initialized slice and writes into the 32-byte
            // digest. The verified inventory bounds the input size well below u32.
            let status = unsafe { BCryptHash(BCRYPT_SHA256_ALG_HANDLE, std::ptr::null(), 0,
                bytes.as_ptr(), bytes.len() as u32, digest.as_mut_ptr(), digest.len() as u32) };
            if status < 0 || digest != entry.sha256 { return Err(invalid("shader store record checksum mismatch")); }
            super::super::decode(&bytes).ok_or_else(|| invalid("invalid verified VSH1 record"))
        });
        match shader {
            Ok(shader) => {
                let hits = self.hits.fetch_add(1, Ordering::Relaxed) + 1;
                if hits.is_power_of_two() {
                    tracing::info!(target: "victory_gpu::memory_watch", hits, "reused verified shader store records");
                }
                Some(shader)
            }
            Err(error) => {
                if !self.warned.swap(true, Ordering::Relaxed) {
                    tracing::warn!(target: "victory_gpu::memory_watch", %guest, %error, "shader store record unavailable");
                }
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "requires the verified 578b corpus, both actual shader packs and guest fixtures"]
    fn checked_store_after_both_packs() {
        let dir = PathBuf::from(std::env::var_os("VICTORY_TEST_VERIFIED_STORE").unwrap());
        crate::descriptor::host::probe_view_limit();
        let build = crate::shader_cache::build();
        assert_eq!(blake3::Hash::from(build).to_hex().as_str(), BUILD);
        let primary = super::super::Pack::read(File::open(dir.join("primary.vsp")).unwrap()).unwrap();
        let mut packs = super::super::Packs::new(primary, Some(&dir.join("overlay.vsp")));
        let count = |p: &super::super::Packs| p.verified_store.as_ref().unwrap().hits.load(Ordering::Relaxed);
        for n in ["primary.guest", "overlay.guest"] {
            let guest = std::fs::read(dir.join(n)).unwrap();
            let result = packs.translate(|| build, &guest, |_| panic!("packed shader unexpectedly compiled"));
            assert!(!result.unwrap().dxil.is_empty());
            assert_eq!(count(&packs), 0);
        }
        let mut samples = 0;
        for entry in std::fs::read_dir(dir.join("selections")).unwrap() {
            let guest = std::fs::read(entry.unwrap().path()).unwrap();
            let shader = packs.translate(|| build, &guest, |_| panic!("corpus shader unexpectedly compiled")).unwrap();
            assert_eq!(shader.dxil.get(0..4), Some(b"DXBC".as_slice()));
            samples += 1;
        }
        assert!(samples >= 40);
        assert_eq!(count(&packs), samples);
        let guest = std::fs::read(dir.join("one.guest")).unwrap();
        let hash = blake3::hash(&guest);
        let store = packs.verified_store.as_ref().unwrap();
        let key = blake3::Hash::from(store.entries[&hash].cache_key).to_hex();
        let expected = store.shader(&hash).unwrap();
        assert_eq!(expected.dxil, victory_shader::emit::translate(&guest).unwrap().dxil);
        let path = Path::new(&key[..2]).join(&key[2..]);
        let mut bytes = std::fs::read(store.root.join(&path)).unwrap();
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        std::fs::copy(dir.join("corpus/translations.tsv"), root.join("translations.tsv")).unwrap();
        let copy = root.join("shaders").join(path);
        std::fs::create_dir_all(copy.parent().unwrap()).unwrap();
        packs.verified_store = Some(Store::open(root).unwrap());
        let fallback = || Err::<HostShader, String>("ordinary translator".into());
        assert_eq!(packs.translate(|| build, &guest, |_| fallback()).unwrap_err(), "ordinary translator");
        std::fs::write(&copy, &bytes).unwrap();
        assert_eq!(packs.translate(|| build, &guest, |_| panic!()).unwrap().dxil, expected.dxil);
        *bytes.last_mut().unwrap() ^= 1;
        std::fs::write(&copy, &bytes).unwrap();
        assert_eq!(packs.translate(|| build, &guest, |_| fallback()).unwrap_err(), "ordinary translator");
        assert_eq!(packs.translate(|| [9; 32], &guest, |_| fallback()).unwrap_err(), "ordinary translator");
        std::fs::write(&copy, &bytes[..bytes.len()-1]).unwrap();
        assert_eq!(packs.translate(|| build, &guest, |_| fallback()).unwrap_err(), "ordinary translator");
        let index = root.join("translations.tsv");
        let mut data = std::fs::read(&index).unwrap();
        *data.last_mut().unwrap() ^= 1;
        std::fs::write(index, data).unwrap();
        assert!(Store::open(root).is_err());
        println!("verified {samples} published shaders after both RAM packs; fresh DXIL matched, modified record and inventory rejected");
    }
}
