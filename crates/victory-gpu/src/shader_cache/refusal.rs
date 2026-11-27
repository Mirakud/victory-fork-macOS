//! One local, opt-in record of a deterministic lowering error. A positive
//! shader pack hit always wins. An absent or damaged record retries the emitter.
use std::fs::File;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

const MAGIC: &[u8; 8] = b"VREF1\r\n\x1a";
const WHAT: &str = "a lane mask exchanged where not every lane runs";
const SIZE: usize = 8 + 32 + 32 + 32 + 8 + 8 + 32;

#[derive(Clone, Copy)]
struct Record {
    exe: [u8; 32],
    build: [u8; 32],
    guest: [u8; 32],
    offset: usize,
    elapsed_us: u64,
}

impl Record {
    fn read(path: &Path) -> Option<Self> {
        let mut file = File::open(path).ok()?;
        let mut bytes = [0; SIZE];
        file.read_exact(&mut bytes).ok()?;
        if file.read(&mut [0]).ok()? != 0
            || &bytes[..8] != MAGIC
            || blake3::hash(&bytes[..SIZE - 32]).as_bytes() != &bytes[SIZE - 32..]
        {
            return None;
        }
        Some(Self {
            exe: bytes[8..40].try_into().ok()?,
            build: bytes[40..72].try_into().ok()?,
            guest: bytes[72..104].try_into().ok()?,
            offset: u64::from_le_bytes(bytes[104..112].try_into().ok()?)
                .try_into()
                .ok()?,
            elapsed_us: u64::from_le_bytes(bytes[112..120].try_into().ok()?),
        })
    }

    fn write(self, path: &Path) -> io::Result<()> {
        let mut bytes = Vec::with_capacity(SIZE);
        bytes.extend(MAGIC);
        bytes.extend(self.exe);
        bytes.extend(self.build);
        bytes.extend(self.guest);
        bytes.extend((self.offset as u64).to_le_bytes());
        bytes.extend(self.elapsed_us.to_le_bytes());
        bytes.extend(blake3::hash(&bytes).as_bytes());
        // The per-process lock covers all writes. Other processes have their
        // own staging file; a late writer can evict a record, never mix keys.
        let mut temporary = path.as_os_str().to_owned();
        temporary.push(format!(".{}.partial", std::process::id()));
        let temporary = PathBuf::from(temporary);
        if let Err(error) =
            std::fs::write(&temporary, bytes).and_then(|()| std::fs::rename(&temporary, path))
        {
            let _ = std::fs::remove_file(temporary);
            return Err(error);
        }
        Ok(())
    }
}

fn message(offset: usize) -> String {
    victory_shader::ShaderError::Unsupported { offset, what: WHAT }.to_string()
}

fn known_offset(error: &str) -> Option<usize> {
    // The shader cache API returns text. Require the exact canonical
    // rendering of this one emitter variant, including its offset.
    let (hex, _) = error.strip_prefix("GFX10 code at 0x")?.split_once(':')?;
    let offset = usize::from_str_radix(hex, 16).ok()?;
    (message(offset) == error).then_some(offset)
}

pub(super) struct Cache {
    path: PathBuf,
    exe: [u8; 32],
    record: Mutex<Option<Record>>,
}

impl Cache {
    fn open(path: PathBuf, exe: [u8; 32]) -> Self {
        let record = Mutex::new(Record::read(&path));
        Self { path, exe, record }
    }

    pub(super) fn configured() -> Option<&'static Self> {
        static CACHE: OnceLock<Option<Cache>> = OnceLock::new();
        CACHE.get_or_init(|| {
            let path = PathBuf::from(std::env::var_os("VICTORY_SHADER_REFUSAL_FILE")?);
            if !path.is_absolute() {
                tracing::warn!("shader refusal cache requires an absolute path");
                return None;
            }
            // The positive pack's key includes the device and translator switches.
            // This extra digest also catches a new compiler, dependency, or linked PE.
            let executable = std::env::current_exe().and_then(File::open);
            let hash = executable.and_then(|file| {
                let mut hasher = blake3::Hasher::new();
                hasher.update_reader(file)?;
                Ok(*hasher.finalize().as_bytes())
            });
            match hash {
                Ok(exe) => Some(Self::open(path, exe)),
                Err(error) => {
                    tracing::warn!(%error, "cannot identify executable for shader refusal cache");
                    None
                }
            }
        }).as_ref()
    }

    pub(super) fn lookup(&self, build: [u8; 32], guest: [u8; 32]) -> Option<String> {
        let saved = self.record.lock().unwrap_or_else(|e| e.into_inner());
        let record = saved.as_ref()?;
        if record.exe != self.exe || record.build != build || record.guest != guest {
            return None;
        }
        let error = message(record.offset);
        tracing::info!(target: "victory_gpu::memory_watch", guest = %blake3::Hash::from(guest),
            %error, original_translation_us = record.elapsed_us, "reused deterministic shader refusal");
        Some(error)
    }

    pub(super) fn observe(&self, build: [u8; 32], guest: [u8; 32], error: &str, elapsed_us: u64) {
        let Some(offset) = known_offset(error) else {
            return;
        };
        let mut saved = self.record.lock().unwrap_or_else(|e| e.into_inner());
        if saved
            .as_ref()
            .is_some_and(|r| r.exe == self.exe && r.build == build && r.elapsed_us >= elapsed_us)
        {
            return;
        }
        let record = Record {
            exe: self.exe,
            build,
            guest,
            offset,
            elapsed_us,
        };
        match record.write(&self.path) {
            Ok(()) => {
                *saved = Some(record);
                tracing::info!(target: "victory_gpu::memory_watch", guest = %blake3::Hash::from(guest),
                    %error, elapsed_us, "saved deterministic shader refusal");
            }
            Err(error) => tracing::warn!(%error, "cannot save shader refusal cache"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_inputs_replay_only_the_canonical_refusal() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("refusal");
        let guest = [3; 32];
        let build = [2; 32];
        let exe = [1; 32];
        let cache = Cache::open(path.clone(), exe);
        for other in [
            "file not found",
            "GFX10 code at 0x1eb4: another construct is not supported",
            "GFX10 code at 0x01eb4: a lane mask exchanged where not every lane runs is not supported",
        ] {
            cache.observe(build, guest, other, 50);
            assert!(!path.exists());
        }
        let expected = message(0x1eb4);
        cache.observe(build, guest, &expected, 500);
        assert_eq!(std::fs::metadata(&path).unwrap().len(), SIZE as u64);
        let reopened = Cache::open(path.clone(), exe);
        assert_eq!(reopened.lookup(build, guest), Some(expected.clone()));
        let mut different = guest;
        different[31] ^= 1;
        assert!(reopened.lookup(build, different).is_none());
        let mut new_build = build;
        new_build[31] ^= 1;
        assert!(reopened.lookup(new_build, guest).is_none());
        let mut new_exe = exe;
        new_exe[31] ^= 1;
        assert!(
            Cache::open(path.clone(), new_exe)
                .lookup(build, guest)
                .is_none()
        );
        reopened.observe(build, different, &message(8), 20);
        assert_eq!(reopened.lookup(build, guest), Some(expected));
        reopened.observe(build, different, &message(8), 700);
        assert_eq!(
            Cache::open(path, exe).lookup(build, different),
            Some(message(8))
        );
    }

    #[test]
    fn damaged_entries_are_ignored_and_can_be_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("refusal");
        let expected = message(16);
        let cache = Cache::open(path.clone(), [1; 32]);
        cache.observe([2; 32], [3; 32], &expected, 1);
        let bytes = std::fs::read(&path).unwrap();
        for mut bad in [
            bytes[..SIZE - 1].to_vec(),
            [bytes.as_slice(), &[0]].concat(),
            bytes.clone(),
        ] {
            if bad.len() == SIZE {
                bad[104] ^= 1;
            }
            std::fs::write(&path, bad).unwrap();
            let fresh = Cache::open(path.clone(), [1; 32]);
            assert!(fresh.lookup([2; 32], [3; 32]).is_none());
            fresh.observe([2; 32], [3; 32], &expected, 1);
            assert_eq!(std::fs::read(&path).unwrap(), bytes);
        }
    }
}
