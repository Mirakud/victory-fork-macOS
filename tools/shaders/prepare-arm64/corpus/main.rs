mod merge;
#[path = "../../../../crates/victory-gpu/src/shader_cache.rs"]
mod shader_cache;
use std::{
    collections::HashSet,
    env, fs,
    io::{self, Write},
    path::{Path, PathBuf},
    thread,
    time::Instant,
};

fn ids(path: &str) -> io::Result<HashSet<String>> {
    Ok(fs::read_to_string(path)?
        .lines()
        .map(str::to_owned)
        .collect())
}

enum Prepared {
    Missing,
    Failed(String),
    Cached {
        made: bool,
        key: String,
        guest: blake3::Hash,
        path: PathBuf,
        size: u64,
        micros: u128,
    },
}

fn worker_count() -> io::Result<usize> {
    match env::var_os("VICTORY_SHADER_JOBS") {
        Some(raw) => raw
            .to_str()
            .and_then(|v| v.parse::<usize>().ok())
            .filter(|v| (1..=64).contains(v))
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "VICTORY_SHADER_JOBS must be in 1..=64",
                )
            }),
        None => Ok(thread::available_parallelism()
            .map(usize::from)
            .unwrap_or(1)
            .min(6)),
    }
}

fn prepare(id: &str, source: &Path, root: &Path, build: blake3::Hash) -> io::Result<Prepared> {
    let bytes = match fs::read(source.join(id)) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Prepared::Missing),
        Err(e) => return Err(e),
    };
    let guest = blake3::hash(&bytes);
    if env::var_os("VICTORY_PREPARE_VERIFY_IDS").is_some() && guest.to_hex().as_str() != id {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("container digest differs for {id}"),
        ));
    }
    let mut h = blake3::Hasher::new();
    h.update(b"shader.dxil\0");
    h.update(&1u32.to_le_bytes());
    h.update(build.as_bytes());
    h.update(&bytes);
    let key = h.finalize().to_hex().to_string();
    let began = Instant::now();
    match shader_cache::readonly::warm(&bytes) {
        Ok(made) => {
            let micros = began.elapsed().as_micros();
            let path = root.join(&key[..2]).join(&key[2..]);
            let size = fs::metadata(&path)?.len();
            Ok(Prepared::Cached {
                made,
                key,
                guest,
                path,
                size,
                micros,
            })
        }
        Err(err) => Ok(Prepared::Failed(err)),
    }
}

fn write_report(path: &Path, body: &[u8]) -> io::Result<()> {
    let dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let mut output = tempfile::NamedTempFile::new_in(dir)?;
    output.write_all(body)?;
    output.flush()?;
    output.persist(path)?;
    Ok(())
}

fn main() -> io::Result<()> {
    let args: Vec<_> = env::args().collect();
    if let [_, command, fixed, current, delta, output] = args.as_slice()
        && command == "compose"
    {
        return merge::compose(
            Path::new(fixed),
            Path::new(current),
            Path::new(delta),
            Path::new(output),
        );
    }
    let usage = "prepare-ro extract IDS OUTPUT_DIR ARCHIVE | warm IDS INPUTS CACHE | pack IDS INPUTS CACHE PACK | verify IDS INPUTS CACHE PACK | compose FIXED CURRENT DELTA OUTPUT";
    if args.get(1).is_some_and(|a| a == "--help" || a == "-h") {
        println!("{usage}");
        return Ok(());
    }
    if !matches!(
        (args.get(1).map(String::as_str), args.len()),
        (Some("extract" | "warm"), 5) | (Some("pack" | "verify"), 6)
    ) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, usage));
    }
    if args[1] == "extract" {
        let wanted = ids(&args[2])?;
        let root = Path::new(&args[3]);
        fs::create_dir_all(root)?;
        let data = fs::read(&args[4])?;
        let mut seen = HashSet::new();
        let mut n = 0;
        for rec in victory_game::sgd5::records(&data) {
            let Some(bytes) = rec.unpack() else { continue };
            n += 1;
            let digest = blake3::hash(&bytes).to_hex().to_string();
            let id = &digest[..16];
            if wanted.contains(id) && seen.insert(digest.clone()) {
                fs::write(root.join(id), &bytes)?;
                println!("{id} {digest} {:x} {}", rec.offset, bytes.len());
            }
        }
        eprintln!(
            "{n} records, {} selected containers of {}",
            seen.len(),
            wanted.len()
        );
        return Ok(());
    }
    let mut wanted: Vec<_> = ids(&args[2])?.into_iter().collect();
    wanted.sort();
    let source = Path::new(&args[3]);
    let root = Path::new(&args[4]);
    victory_shader::emit::set_host_max_wave(32);
    victory_shader::emit::set_view_heap_limit(999999);
    let fingerprint = victory_shader::emit::translation_fingerprint();
    let mut h = blake3::Hasher::new();
    h.update(b"victory shader cache key 2\0");
    h.update(&fingerprint);
    h.update(include_bytes!(
        "../../../../crates/victory-gpu/src/shader_cache.rs"
    ));
    let build = h.finalize();
    eprintln!(
        "shader source {} build {} diagnostics={} debug_assertions={}",
        String::from_utf8_lossy(&fingerprint[..32]),
        build,
        fingerprint[32],
        fingerprint[33]
    );
    shader_cache::use_shader_cache(root.to_owned());
    if args[1] == "verify" {
        shader_cache::readonly::preload(Path::new(&args[5]));
        for id in &wanted {
            let guest = fs::read(source.join(id))?;
            let actual = shader_cache::host_shader(&guest).expect("decode from original pack");
            assert!(!actual.dxil.is_empty());
        }
        let stats = shader_cache::readonly::counters();
        eprintln!(
            "pack verification: {} hits, {} misses, {} microseconds lookup",
            stats[0], stats[1], stats[2]
        );
        assert_eq!(stats[0] as usize, wanted.len());
        assert_eq!(stats[1], 0);
        assert_eq!(stats[4], 0, "verification must read the specified pack");
        return Ok(());
    }
    let mut pack = Vec::new();
    pack.extend(b"VSP1\r\n\x1a\n");
    pack.extend(build.as_bytes());
    pack.extend(0u32.to_le_bytes());
    let mut total = 0u32;
    let jobs = worker_count()?.min(wanted.len()).max(1);
    let chunk_size = wanted.len().div_ceil(jobs).max(1);
    // Workers never print or retain a shader blob. Join their metadata in ID
    // order, preserving the rows and byte layout consumed by the launcher.
    let rows = thread::scope(|scope| -> io::Result<Vec<Prepared>> {
        let mut workers = Vec::new();
        for chunk in wanted.chunks(chunk_size) {
            workers.push(scope.spawn(move || {
                chunk
                    .iter()
                    .map(|id| prepare(id, source, root, build))
                    .collect::<io::Result<Vec<_>>>()
            }));
        }
        let mut rows = Vec::with_capacity(wanted.len());
        for worker in workers {
            rows.extend(
                worker
                    .join()
                    .map_err(|_| io::Error::other("shader preparation worker panicked"))??,
            );
        }
        Ok(rows)
    })?;
    let mut refused = Vec::new();
    for (id, row) in wanted.iter().zip(rows) {
        match row {
            Prepared::Missing => println!("{id} MISSING"),
            Prepared::Failed(error) => {
                println!("{id} FAILED {error}");
                writeln!(refused, "{id}\t{}", error.replace(['\t', '\r', '\n'], " "))?;
            }
            Prepared::Cached {
                made,
                key,
                guest,
                path,
                size,
                micros,
            } => {
                if args[1] == "pack" {
                    let blob = fs::read(path)?;
                    let size = u32::try_from(blob.len()).map_err(io::Error::other)?;
                    pack.extend_from_slice(guest.as_bytes());
                    pack.extend_from_slice(&size.to_le_bytes());
                    pack.extend_from_slice(&blob);
                    total += 1;
                }
                println!(
                    "{id} {} {key} {size} {micros}",
                    if made { "MADE" } else { "HIT" }
                );
            }
        }
    }
    if let Some(path) = env::var_os("VICTORY_PREPARE_REFUSALS_FILE") {
        write_report(Path::new(&path), &refused)?;
    }
    if args[1] == "pack" {
        pack[40..44].copy_from_slice(&total.to_le_bytes());
        pack.extend(blake3::hash(&pack).as_bytes());
        write_report(Path::new(&args[5]), &pack)?;
        eprintln!("wrote {} bytes and {} entries", pack.len(), total);
    }
    Ok(())
}
