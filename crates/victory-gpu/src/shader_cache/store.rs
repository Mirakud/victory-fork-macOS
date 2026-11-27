//! Reads individual immutable VSH1 records published by the ordinary shader converter.
use super::invalid;
use std::fs::File;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;
use victory_pipeline::CacheKey;
use victory_shader::dxbc::{Container, FourCC};
use victory_shader::dxil::DxilProgram;
use victory_shader::emit::HostShader;

const MAX_RECORD: u64 = 32 << 20;

pub(super) fn configured() -> Option<PathBuf> {
    std::env::var_os("VICTORY_READONLY_SHADER_STORE")
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
}

// The converter identity is shared with shader_cache::Translate. Both cache
// layouts are derived by CacheKey. The guest includes its entire container.
pub(super) fn key(build: &[u8; 32], guest: &[u8]) -> CacheKey {
    CacheKey::from_parts("shader.dxil", 1, [build.as_slice(), guest])
}

fn read(path: &Path) -> io::Result<Vec<u8>> {
    let mut file = File::open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || !(4..=MAX_RECORD).contains(&metadata.len()) {
        return Err(invalid("invalid VSH1 store entry size"));
    }
    let mut bytes = vec![0; metadata.len() as usize];
    file.read_exact(&mut bytes)?;
    if file.read(&mut [0; 1])? != 0 {
        return Err(invalid("VSH1 store entry grew during its read"));
    }
    Ok(bytes)
}

pub(super) fn lookup(root: &Path, build: &[u8; 32], guest: &[u8], io_us: &AtomicU64) -> io::Result<HostShader> {
    let path = key(build, guest).path_under(root);
    let start = Instant::now();
    let bytes = read(&path);
    io_us.fetch_add(start.elapsed().as_micros() as u64, Ordering::Relaxed);
    let shader = super::super::decode(&bytes?).ok_or_else(|| invalid("malformed VSH1 store entry"))?;
    let container = Container::parse(&shader.dxil).map_err(|_| invalid("malformed DXBC in VSH1 store entry"))?;
    let program = container.part(FourCC::DXIL).ok_or_else(|| invalid("missing DXIL in VSH1 store entry"))?;
    DxilProgram::parse(program.data).map_err(|_| invalid("malformed DXIL program in VSH1 store entry"))?;
    Ok(shader)
}
