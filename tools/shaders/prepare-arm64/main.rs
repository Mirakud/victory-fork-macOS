// The cache codec and read-only lookup are the ones compiled into Victory.
#[allow(dead_code)]
#[path = "../../../crates/victory-gpu/src/shader_cache.rs"]
mod shader_cache;

use anyhow::{Context, Result, bail, ensure};
use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::thread;

const USAGE: &str = "victory-prepare-shaders extract ARCHIVE INPUTS LIMIT\n\
                     victory-prepare-shaders warm INPUTS CACHE\n\
                     victory-prepare-shaders pack INPUTS CACHE NEW.vsp\n\
                     victory-prepare-shaders verify INPUTS PACK.vsp\n\
                     victory-prepare-shaders identity";
const MAX_INPUT: usize = 4 << 20;
const MAX_PACK: usize = 256 << 20;
const MAX_ENTRIES: usize = 8192;

fn build_key() -> Result<blake3::Hash> {
    ensure!(
        std::env::var_os("VICTORY_SHADER_CACHE_KEY").as_deref() != Some("exe".as_ref()),
        "the executable-specific cache key cannot be shared with a native preparer"
    );
    // These are the device facts recorded by the D3DMetal 4 profile.
    victory_shader::emit::set_host_max_wave(32);
    victory_shader::emit::set_view_heap_limit(999_999);
    let mut h = blake3::Hasher::new();
    h.update(b"victory shader cache key 2\0");
    h.update(&victory_shader::emit::translation_fingerprint());
    h.update(include_bytes!(
        "../../../crates/victory-gpu/src/shader_cache.rs"
    ));
    Ok(h.finalize())
}

fn extract(archive: &Path, directory: &Path, limit: usize) -> Result<()> {
    ensure!(
        (1..=MAX_ENTRIES).contains(&limit),
        "limit must be 1..={MAX_ENTRIES}"
    );
    victory_core::ensure_outside_repo(directory)?;
    let data = fs::read(archive).with_context(|| archive.display().to_string())?;
    ensure!(victory_game::sgd5::is_sgd5(&data), "missing SGD5 header");
    fs::create_dir_all(directory)?;
    let mut found = std::collections::HashSet::new();
    for record in victory_game::sgd5::records(&data) {
        let Some(bytes) = record.unpack() else {
            continue;
        };
        if bytes.is_empty() || bytes.len() > MAX_INPUT {
            continue;
        }
        let guest = blake3::hash(&bytes);
        if !found.insert(guest) {
            continue;
        }
        let name = directory.join(guest.to_hex().as_str());
        if name.exists() {
            ensure!(
                fs::read(&name)? == bytes,
                "conflicting container {}",
                name.display()
            );
        } else {
            let mut file = tempfile::NamedTempFile::new_in(directory)?;
            file.write_all(&bytes)?;
            file.persist_noclobber(&name)?;
        }
        if found.len() == limit {
            break;
        }
    }
    ensure!(!found.is_empty(), "no valid containers found");
    println!("containers={}", found.len());
    Ok(())
}

fn input_ids(directory: &Path) -> Result<Vec<String>> {
    let mut ids = Vec::new();
    for entry in fs::read_dir(directory).with_context(|| directory.display().to_string())? {
        let entry = entry?;
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        if name.len() == 64
            && name
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            ensure!(
                entry.file_type()?.is_file(),
                "expected a regular captured file: {name}"
            );
            ids.push(name);
            ensure!(
                ids.len() <= MAX_ENTRIES,
                "more than {MAX_ENTRIES} input containers"
            );
        }
    }
    ensure!(
        !ids.is_empty(),
        "no hash-named containers in {}",
        directory.display()
    );
    ids.sort_unstable();
    Ok(ids)
}

fn read_input(directory: &Path, id: &str) -> Result<Vec<u8>> {
    let path = directory.join(id);
    let file = File::open(&path).with_context(|| path.display().to_string())?;
    let mut bytes = Vec::new();
    file.take(MAX_INPUT as u64 + 1).read_to_end(&mut bytes)?;
    ensure!(
        (1..=MAX_INPUT).contains(&bytes.len()),
        "invalid capture size: {id}"
    );
    ensure!(
        blake3::hash(&bytes).to_hex().as_str() == id,
        "capture hash mismatch: {id}"
    );
    Ok(bytes)
}

fn cached_path(cache: &Path, build: blake3::Hash, guest: &[u8]) -> PathBuf {
    let mut h = blake3::Hasher::new();
    h.update(b"shader.dxil\0");
    h.update(&1u32.to_le_bytes());
    h.update(build.as_bytes());
    h.update(guest);
    let key = h.finalize().to_hex();
    cache.join(&key[..2]).join(&key[2..])
}

fn verify(inputs: &Path, pack: &Path, build: blake3::Hash) -> Result<()> {
    // Only this pack may supply a hit. Offline verification has no recording side effects.
    for name in [
        "VICTORY_READONLY_SHADER_OVERLAY",
        "VICTORY_READONLY_SHADER_STORE",
        "VICTORY_SHADER_MISSES_DIR",
        "VICTORY_SHADER_REFUSAL_FILE",
    ] {
        ensure!(
            std::env::var_os(name).is_none(),
            "unset {name} for pack verification"
        );
    }
    shader_cache::readonly::preload(pack);
    let mut file = File::open(pack)?;
    let mut header = [0; 44];
    file.read_exact(&mut header)?;
    ensure!(&header[..8] == b"VSP1\r\n\x1a\n", "invalid VSP1 header");
    ensure!(
        &header[8..40] == build.as_bytes(),
        "pack build differs from preparer build {build}"
    );
    let count = u32::from_le_bytes(header[40..44].try_into()?);
    ensure!(
        (1..=MAX_ENTRIES as u32).contains(&count),
        "invalid entry count"
    );
    for index in 0..count {
        let mut id = [0; 32];
        let mut size = [0; 4];
        file.read_exact(&mut id)?;
        file.read_exact(&mut size)?;
        file.seek(SeekFrom::Current(i64::from(u32::from_le_bytes(size))))?;
        let id = blake3::Hash::from(id).to_hex();
        let bytes = read_input(inputs, id.as_str())?;
        let shader = shader_cache::host_shader(&bytes).map_err(anyhow::Error::msg)?;
        let stats = shader_cache::readonly::counters();
        ensure!(
            !shader.dxil.is_empty() && stats[0] == u64::from(index + 1) && stats[1] == 0,
            "pack did not supply a decoded translation for {id}"
        );
    }
    println!("verified={count} fallback=0 build={build}");
    Ok(())
}

fn prepare_container(
    inputs: &Path,
    id: &str,
    cache: &Path,
    build: blake3::Hash,
) -> Result<Option<PathBuf>> {
    let guest = read_input(inputs, id)?;
    match shader_cache::warm(&guest) {
        Ok(_) => Ok(Some(cached_path(cache, build, &guest))),
        Err(error) if error.starts_with("converter shader.dxil v1: ") => {
            eprintln!("refused {id}: {error}");
            Ok(None)
        }
        Err(error) => bail!("{id}: {error}"),
    }
}

fn worker_count() -> Result<usize> {
    if let Some(value) = std::env::var_os("VICTORY_SHADER_JOBS") {
        let jobs: usize = value
            .to_str()
            .context("invalid VICTORY_SHADER_JOBS")?
            .parse()?;
        ensure!(
            (1..=64).contains(&jobs),
            "VICTORY_SHADER_JOBS must be in 1..=64"
        );
        return Ok(jobs);
    }
    Ok(thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(1)
        .min(6))
}

fn prepare(inputs: &Path, cache: &Path, output: Option<&Path>, build: blake3::Hash) -> Result<()> {
    let ids = input_ids(inputs)?;
    if let Some(path) = output {
        victory_core::ensure_outside_repo(path)?;
        ensure!(
            !path.try_exists()?,
            "output already exists: {}",
            path.display()
        );
    }
    victory_pipeline::Cache::open(cache)?;
    shader_cache::use_shader_cache(cache.to_owned());
    let mut pack = Vec::new();
    if output.is_some() {
        pack.extend_from_slice(b"VSP1\r\n\x1a\n");
        pack.extend_from_slice(build.as_bytes());
        pack.extend_from_slice(&0u32.to_le_bytes());
    }
    let mut count = 0u32;
    let mut refused = 0usize;
    let jobs = worker_count()?.min(ids.len());
    // Each worker releases its decoded buffers before the next translation.
    // Join chunks in input order so worker count cannot change the VSP1 bytes.
    let prepared = thread::scope(|scope| -> Result<Vec<Option<PathBuf>>> {
        let mut workers = Vec::new();
        for chunk in ids.chunks(ids.len().div_ceil(jobs)) {
            workers.push(scope.spawn(move || {
                chunk
                    .iter()
                    .map(|id| prepare_container(inputs, id, cache, build))
                    .collect::<Result<Vec<_>>>()
            }));
        }
        let mut paths = Vec::with_capacity(ids.len());
        for worker in workers {
            paths.extend(
                worker
                    .join()
                    .map_err(|_| anyhow::anyhow!("translation worker panicked"))??,
            );
        }
        Ok(paths)
    })?;
    for (id, path) in ids.iter().zip(prepared) {
        let Some(path) = path else {
            refused += 1;
            continue;
        };
        count += 1;
        if output.is_some() {
            let blob = fs::read(&path).with_context(|| path.display().to_string())?;
            ensure!(
                pack.len() + 36 + blob.len() + 32 <= MAX_PACK,
                "pack exceeds 256 MiB"
            );
            pack.extend_from_slice(blake3::Hash::from_hex(id)?.as_bytes());
            pack.extend_from_slice(&(blob.len() as u32).to_le_bytes());
            pack.extend_from_slice(&blob);
        }
    }
    ensure!(count > 0, "no successful translations ({refused} refused)");
    let (hits, misses) = shader_cache::counts();
    println!("translated={misses} cached={hits} refused={refused} build={build}");
    if let Some(output) = output {
        pack[40..44].copy_from_slice(&count.to_le_bytes());
        pack.extend_from_slice(blake3::hash(&pack).as_bytes());
        let directory = output
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let mut temporary = tempfile::NamedTempFile::new_in(directory)?;
        temporary.write_all(&pack)?;
        drop(pack);
        temporary.flush()?;
        verify(inputs, temporary.path(), build)?;
        temporary.persist_noclobber(output)?;
        println!("{}", output.display());
    }
    Ok(())
}

fn main() -> Result<()> {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    match args.as_slice() {
        [cmd] if cmd == "--help" || cmd == "-h" => println!("{USAGE}"),
        [cmd] if cmd == "identity" => println!("{}", build_key()?),
        [cmd, archive, inputs, limit] if cmd == "extract" => extract(
            archive.as_ref(),
            inputs.as_ref(),
            limit.to_str().context("invalid limit")?.parse()?,
        )?,
        [cmd, inputs, cache] if cmd == "warm" => {
            prepare(inputs.as_ref(), cache.as_ref(), None, build_key()?)?
        }
        [cmd, inputs, cache, pack] if cmd == "pack" => prepare(
            inputs.as_ref(),
            cache.as_ref(),
            Some(pack.as_ref()),
            build_key()?,
        )?,
        [cmd, inputs, pack] if cmd == "verify" => {
            verify(inputs.as_ref(), pack.as_ref(), build_key()?)?
        }
        _ => bail!("{USAGE}"),
    }
    Ok(())
}
