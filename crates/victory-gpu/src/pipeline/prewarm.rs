//! User-selected guest pipeline keys, independent of the shader disk cache.
use std::collections::{HashMap, HashSet};
use std::sync::atomic::AtomicBool;
use std::io::{self, Read};

const MAX_KEYS: usize = 2048;
const MAX_MANIFEST_BYTES: usize = 96 * 1024;

#[cfg(windows)]
pub(super) fn contains(key: &str) -> bool {
    use std::{env, fs::File, sync::OnceLock};

    static KEYS: OnceLock<HashSet<String>> = OnceLock::new();
    KEYS.get_or_init(|| {
        let mut keys = HashSet::new();
        if let Some(value) = env::var_os("VICTORY_PREWARM_PIPELINE_KEYS") {
            if let Some(explicit) = value.to_str().and_then(explicit_keys) {
                keys = explicit;
            } else {
                tracing::warn!(target: "victory_gpu::memory_watch",
                    "VICTORY_PREWARM_PIPELINE_KEYS expects one to eight comma-separated 32-digit lowercase hex keys");
            }
        }
        if let Some(path) = env::var_os("VICTORY_PREWARM_PIPELINE_MANIFEST") {
            match File::open(path).and_then(|file| with_manifest(&keys, file)) {
                Ok(merged) => {
                    tracing::info!(target: "victory_gpu::memory_watch", unique_keys = merged.len(),
                        "loaded explicit pipeline prewarm selection");
                    keys = merged;
                }
                Err(error) => tracing::warn!(target: "victory_gpu::memory_watch", %error,
                    "could not load VICTORY_PREWARM_PIPELINE_MANIFEST"),
            }
        }
        keys
    }).contains(key)
}

/// Each required key owns a single first-request diagnostic, including when
/// the title creates several pipeline objects with the same content.
#[cfg(windows)]
pub(super) fn required(key: &str) -> Option<&'static AtomicBool> {
    use std::{env, sync::OnceLock};

    static KEYS: OnceLock<HashMap<String, AtomicBool>> = OnceLock::new();
    KEYS.get_or_init(|| {
        let Some(value) = env::var_os("VICTORY_REQUIRED_PIPELINE_KEYS") else {
            return HashMap::new();
        };
        match value.to_str().and_then(required_keys) {
            Some(keys) => keys,
            None => {
                tracing::warn!(target: "victory_gpu::memory_watch",
                    "VICTORY_REQUIRED_PIPELINE_KEYS expects one to eight comma-separated 32-digit lowercase hex keys");
                HashMap::new()
            }
        }
    }).get(key)
}

fn required_keys(value: &str) -> Option<HashMap<String, AtomicBool>> {
    Some(explicit_keys(value)?.into_iter().map(|key| (key, AtomicBool::new(false))).collect())
}

fn valid_key(key: &str) -> bool {
    key.len() == 32 && key.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

fn explicit_keys(value: &str) -> Option<HashSet<String>> {
    let mut keys = HashSet::new();
    for (index, key) in value.split(',').enumerate() {
        if index >= 8 || !valid_key(key) {
            return None;
        }
        keys.insert(key.to_owned());
    }
    Some(keys)
}

/// Commit the manifest only if every line and the union fit the limits.
/// The caller can still use its explicit keys when the file is invalid.
fn with_manifest(explicit: &HashSet<String>, reader: impl Read) -> io::Result<HashSet<String>> {
    let mut data = String::new();
    reader
        .take(MAX_MANIFEST_BYTES as u64 + 1)
        .read_to_string(&mut data)?;
    if data.is_empty() || data.len() > MAX_MANIFEST_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("manifest must contain 1 to {MAX_MANIFEST_BYTES} bytes"),
        ));
    }
    let mut keys = explicit.clone();
    for key in data.lines() {
        if !valid_key(key) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "expected a 32-digit lowercase hex key on each line",
            ));
        }
        keys.insert(key.to_owned());
        if keys.len() > MAX_KEYS {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("manifest and explicit selection exceed {MAX_KEYS} unique keys"),
            ));
        }
    }
    Ok(keys)
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "0123456789abcdef0123456789abcdef";

    #[test]
    fn explicit_list_retains_eight_entry_limit() {
        let expected = HashSet::from([KEY.to_owned()]);
        assert_eq!(explicit_keys(KEY), Some(expected.clone()));
        assert_eq!(explicit_keys(&[KEY; 8].join(",")), Some(expected));
        for bad in [
            &[KEY; 9].join(","),
            &KEY.to_ascii_uppercase(),
            &format!("{KEY},"),
            &format!(" {KEY}"),
            "",
        ] {
            assert!(explicit_keys(bad).is_none());
        }
    }

    #[test]
    fn required_selection_is_exact_bounded_and_reported_once_per_key() {
        use std::sync::atomic::Ordering;
        let other = "a".repeat(32);
        let keys = required_keys(&format!("{KEY},{other},{KEY}")).unwrap();
        assert_eq!(keys.len(), 2);
        assert!(!keys[KEY].swap(true, Ordering::Relaxed));
        assert!(keys[KEY].load(Ordering::Relaxed));
        assert!(!keys[&other].load(Ordering::Relaxed));
        assert!(!keys.contains_key("0123456789abcdef"));
        assert!(required_keys(&[KEY; 8].join(",")).is_some());
        assert!(required_keys(&[KEY; 9].join(",")).is_none());
        for bad in [format!("{KEY},A"), format!("{KEY},"), String::new()] {
            assert!(required_keys(&bad).is_none());
        }
    }

    #[test]
    fn reads_lf_crlf_and_overlap_with_explicit_keys() {
        let other = "a".repeat(32);
        let explicit = explicit_keys(KEY).unwrap();
        let lines = format!("{other}\r\n{KEY}\n{other}");
        let expected = HashSet::from([KEY.to_owned(), other]);
        assert_eq!(
            with_manifest(&explicit, lines.as_bytes()).unwrap(),
            expected
        );
    }

    #[test]
    fn rejects_invalid_lines_atomically() {
        let explicit = explicit_keys(KEY).unwrap();
        for bad in [
            "".to_owned(),
            format!("{KEY}\n\n"),
            format!("{KEY}\r"),
            KEY.to_ascii_uppercase(),
            "a".repeat(31),
            "a".repeat(33),
            "g".repeat(32),
            format!("{KEY} "),
        ] {
            assert!(with_manifest(&explicit, bad.as_bytes()).is_err());
        }
        assert!(with_manifest(&explicit, &[0xff][..]).is_err());
        assert_eq!(explicit, HashSet::from([KEY.to_owned()]));
    }

    #[test]
    fn file_byte_limit_applies_before_deduplication() {
        // Identical valid keys with LF or CRLF build files exactly on
        // either side of the byte limit, well inside the distinct-key cap.
        let line = format!("{KEY}\n");
        let crlf = format!("{KEY}\r\n");
        let lines = MAX_MANIFEST_BYTES / line.len();
        let extra = MAX_MANIFEST_BYTES % line.len();
        let mut text = crlf.repeat(extra) + &line.repeat(lines - extra);
        assert_eq!(text.len(), MAX_MANIFEST_BYTES);
        let explicit = HashSet::from([KEY.to_owned()]);
        assert_eq!(with_manifest(&explicit, text.as_bytes()).unwrap(), explicit);

        text.insert(text.len() - 1, '\r');
        assert_eq!(text.len(), MAX_MANIFEST_BYTES + 1);
        assert!(with_manifest(&explicit, text.as_bytes()).is_err());
    }

    #[test]
    fn limit_counts_distinct_keys_from_both_sources() {
        let explicit = explicit_keys(KEY).unwrap();
        let mut lines: String = (0..MAX_KEYS - 1).map(|i| format!("{i:032x}\n")).collect();
        lines.push_str(KEY);
        assert_eq!(
            with_manifest(&explicit, lines.as_bytes()).unwrap().len(),
            MAX_KEYS
        );
        let full_file: String = (0..MAX_KEYS).map(|i| format!("{i:032x}\n")).collect();
        assert_eq!(
            with_manifest(&HashSet::new(), full_file.as_bytes())
                .unwrap()
                .len(),
            MAX_KEYS
        );
        assert!(with_manifest(&explicit, full_file.as_bytes()).is_err());
        let too_many = format!("{full_file}{:032x}", MAX_KEYS);
        assert!(with_manifest(&HashSet::new(), too_many.as_bytes()).is_err());
    }
}
