//! Read-only diagnostics for the output budget of the emitted mesh shader.

use std::sync::OnceLock;

use victory_shader::dxbc::{Container, FourCC};

use super::Shader;

const LIMIT: u64 = 32 * 1024;

struct Outputs {
    max_vertices: u16,
    max_primitives: u16,
    vertex_vectors: u8,
    primitive_vectors: u8,
    vertex_components: u64,
    primitive_components: u64,
}

impl Outputs {
    fn bytes(&self) -> u64 {
        // DXIL counts every declared component and rounds both maxima to
        // groups of 32. Match the calculation in emit::container.
        let rounded = |count| u64::from(count).next_multiple_of(32);
        4 * (self.vertex_components * rounded(self.max_vertices)
            + self.primitive_components * rounded(self.max_primitives))
    }
}

fn components(signature: &[u8], expected_elements: u8) -> Option<u64> {
    let count = u32::from_le_bytes(signature.get(..4)?.try_into().ok()?) as usize;
    let start = u32::from_le_bytes(signature.get(4..8)?.try_into().ok()?) as usize;
    if count != usize::from(expected_elements) || start < 8 {
        return None;
    }
    let end = start.checked_add(count.checked_mul(32)?)?;
    Some(
        signature
            .get(start..end)?
            .chunks_exact(32)
            .map(|element| u64::from((element[24] & 0xf).count_ones()))
            .sum(),
    )
}

fn outputs(mesh: &Container<'_>) -> Option<Outputs> {
    let psv = mesh.part(FourCC::PSV0)?.data;
    let size = u32::from_le_bytes(psv.get(..4)?.try_into().ok()?) as usize;
    let info = psv.get(4..)?.get(..size)?;
    if *info.get(24)? != 13 {
        return None;
    }
    let vertex_elements = *info.get(29)?;
    let primitive_elements = *info.get(30)?;
    Some(Outputs {
        max_vertices: u16::from_le_bytes(info.get(12..14)?.try_into().ok()?),
        max_primitives: u16::from_le_bytes(info.get(14..16)?.try_into().ok()?),
        vertex_vectors: *info.get(32)?,
        primitive_vectors: *info.get(26)?,
        vertex_components: components(mesh.part(FourCC::OSG1)?.data, vertex_elements)?,
        primitive_components: match mesh.part(FourCC::PSG1) {
            Some(part) => components(part.data, primitive_elements)?,
            None if primitive_elements == 0 => 0,
            None => return None,
        },
    })
}

fn words(bytes: [u8; 16]) -> [u32; 4] {
    std::array::from_fn(|i| u32::from_le_bytes(bytes[i * 4..i * 4 + 4].try_into().unwrap()))
}

fn program_hash(shader: &Container<'_>) -> Option<[u32; 4]> {
    Some(words(
        shader
            .part(FourCC::HASH)?
            .data
            .get(4..20)?
            .try_into()
            .ok()?,
    ))
}

/// Two translated pairs seen to fail the Apple M3 driver's 32 KiB check.
/// Match the complete container digests, since the driver's PSO IDs vary.
pub(super) fn omit_refused_metal_pair(mesh: &Shader, pixel: &Shader) -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        std::env::var_os("VICTORY_OMIT_REFUSED_METAL_PAIRS").is_some_and(|value| value == "1")
    }) && refused_metal_pair(mesh, pixel)
}

fn refused_metal_pair(mesh: &Shader, pixel: &Shader) -> bool {
    let digest = |shader: &Shader| {
        let translated = shader.translated()?;
        Container::parse(&translated.host.dxil).ok().map(|dxil| words(dxil.digest))
    };
    const MESH: [u32; 4] = [2235147182, 2529915979, 3592216216, 3314303643];
    const PIXELS: [[u32; 4]; 2] = [
        [1636054245, 3304255990, 875445274, 2239753525],
        [2598007678, 255655236, 1542759517, 1175540793],
    ];
    digest(mesh) == Some(MESH) && digest(pixel).is_some_and(|digest| PIXELS.contains(&digest))
}

/// Pairs whose original bytecode GPTK 4.0b3 on the M3 recorded as a no-op.
/// Only used in a diagnostic run and only when the pipeline still uses
/// the original bytes. The same stage can also occur in a working pair.
pub(super) fn recorded_noop_pair(mesh: &Shader, pixel: &Shader) -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    if !*ENABLED.get_or_init(|| std::env::var_os("VICTORY_DECLINE_RECORDED_NOOPS").is_some_and(|v| v == "1")) {
        return false;
    }
    const PAIRS: &[([u32; 4], [u32; 4])] = &[
        ([0x84360b04, 0x472bb8be, 0x44c99d88, 0xbad9a953], [0xf0aa0337, 0x8a3f0937, 0xebf6a793, 0x044817b8]),
        ([0x841af170, 0x29c44219, 0xae74fe88, 0x21f170ac], [0xbf4b566e, 0x1588aaa5, 0x107287ae, 0x2091263d]),
        ([0x6b604c86, 0xfdf7388f, 0x58f674da, 0x74219111], [0x9fb21008, 0x12309461, 0xd9774500, 0x50cf4775]),
        ([0xbc95d58a, 0x250b0a7f, 0xb0750056, 0x416dddce], [0x866d14df, 0x0efc9a55, 0x72176aa5, 0x7efa5e47]),
        ([0xbde0d7b5, 0x895633da, 0x85469fe1, 0x3e56e673], [0xa4127390, 0x840bed7e, 0xafa33a3b, 0xf8d56140]),
        ([0xbde0d7b5, 0x895633da, 0x85469fe1, 0x3e56e673], [0xc3c0ffde, 0x0e4abb47, 0x738b6d91, 0x163c2fbb]),
        ([0x306ed3c8, 0x75c893a8, 0x00d3af8c, 0x9975cdeb], [0x5ba67a1e, 0x22eb49b2, 0x23ebe980, 0x2bc57a25]),
        ([0xc48ddaea, 0x5dae48bf, 0x26d6fe63, 0x2ce9d1fe], [0x1ecd5df3, 0x589f3554, 0xdf4b06e0, 0x6edb2bb1]),
    ];
    let digest = |shader: &Shader| {
        let host = shader.translated()?;
        Container::parse(&host.host.dxil).ok().map(|dxil| words(dxil.digest))
    };
    digest(mesh).zip(digest(pixel)).is_some_and(|pair| PAIRS.contains(&pair))
}

#[derive(Clone, Copy)]
enum Packing {
    ClipVaryings,
    SharedRows,
}

/// Identify the guest containers. Two distinct guest meshes can lower to the
/// same DXBC, and a pixel stage is only valid with its matching mesh layout.
fn repack_guest_pair(mesh: &Shader, pixel: &Shader) -> Option<Packing> {
    const CLIPS: &str = "da22b2adfd4f76d3c60ec4c677a5bcf425ebd56efab5bb6bfaadc865cda827e7";
    const SKIN_A: &str = "093c64546c55554eeab33ae185cc4706eaaa7ade462421a469320cca715f1700";
    const SKIN_B: &str = "ef5409053e14aa82cf76f49933349d63e92f7cc7c882e445d7a7c970d9e6e000";
    const PAIRS: [(&str, &str, Packing); 11] = [
        (CLIPS, "d4f7cbbbf181e45291ac22a0d244c1aa2e75f8e82678ac94631a2f217b34b50f", Packing::ClipVaryings),
        (CLIPS, "f87e29908986f4d8d3c34df4cb2f18bbd986662a23fe5ee5fa29c324f904f6d6", Packing::ClipVaryings),
        (SKIN_A, "818a9a0ec6a53abbb3c7a0256b7cc3facd3661845e18625577435985a4936fcb", Packing::SharedRows),
        (SKIN_A, "1b76b94a7050f18419932e0ed9139d0c1946a0bf9d734e4ad654e16aa9e3900b", Packing::SharedRows),
        (SKIN_B, "94d281151ca58b4ffccf2cec90b3be4fa95e6ba898f48118f3da34be8d4ab4e5", Packing::SharedRows),
        (SKIN_A, "55cf1a9737f7672f062a392e92488e128f9275a1d873038d7833d0bb811499a9", Packing::SharedRows),
        (SKIN_B, "06d8cea6aafd5613af6bd922cf15b206a29d92d7a4c9182f5ca5dd796f97fb3d", Packing::SharedRows),
        ("0d9099a35c000feba3e1b93a70a26884508d6d4907d46ac1c90057958719c4bd", "818a9a0ec6a53abbb3c7a0256b7cc3facd3661845e18625577435985a4936fcb", Packing::SharedRows),
        ("fc2e14d6971785dd9cb8a749c685c401361d7bb61186b6433727c6ba5ac08acf", "9452ffb5434e4e157ad159f351c11cfc06c004655aee9d43c60b74d647253e6f", Packing::SharedRows),
        ("958c32b0c087f618d9b75f732a4ad5cfb678d7d69249944e0efe5c4cdbbc79f9", "df3b213194206d75f409cf78ba4e6039f1dc4b2c4f6f6b421fa54ebd08bd4e5b", Packing::SharedRows),
        ("c5687f9ec96848e8c438d7c736eb901d4adc27c2bbe84788cfbb69682ef9d4f1", "3847458f4cebb0ba79820cc9c52fedd8439a25aba443e9d15aec796d7a37ae12", Packing::SharedRows),
    ];
    let &(ms, ps, packing) = PAIRS.iter().find(|(ms, ps, _)| mesh.hash == ms[..16] && pixel.hash == ps[..16])?;
    (blake3::hash(&mesh.container).to_hex().as_str() == ms
        && blake3::hash(&pixel.container).to_hex().as_str() == ps).then_some(packing)
}

// The prison profile also uses this alpha-clipped pedestrian material with
// another mesh. Several guest IDs produce these same two host shaders.
// Match the complete original DXIL recorded by the driver for this PSO.
fn prison_tint_packing(mesh: &Shader, pixel: &Shader) -> Option<Packing> {
    let mesh = &mesh.translated()?.host.dxil;
    let pixel = &pixel.translated()?.host.dxil;
    if mesh.len() != 30_880 || pixel.len() != 84_585 {
        return None;
    }
    (blake3::hash(mesh).to_hex().as_str() == "4a7e60a13213e1876c9bbb2ae6b5c61d15cf216fd492fd568f957320b78e688e"
        && blake3::hash(pixel).to_hex().as_str() == "c05169b64caf683ca1ae1c4a6c5dbe72d963946455c755d12b6f8e1bc9455d8b")
        .then_some(Packing::SharedRows)
}

pub(super) fn repack_refused_metal_pair(mesh: &Shader, pixel: &Shader) -> Option<[Vec<u8>; 2]> {
    use victory_shader::emit::{repack_mesh_varying_pair, repack_mesh_varying_pair_keeping_clips, repack_shared_mesh_varying_rows};

    static ENABLED: OnceLock<bool> = OnceLock::new();
    if !*ENABLED.get_or_init(|| {
        std::env::var_os("VICTORY_REPACK_METAL_MESH_VARYINGS").is_some_and(|value| value == "1")
    }) {
        return None;
    }
    let packing = repack_guest_pair(mesh, pixel).or_else(|| prison_tint_packing(mesh, pixel))?;
    static KEEP_CLIPS: OnceLock<bool> = OnceLock::new();
    let keep_clips = matches!(packing, Packing::SharedRows) || *KEEP_CLIPS.get_or_init(|| {
        std::env::var_os("VICTORY_MESH_REPACK_KEEP_CLIPS").is_some_and(|value| value == "1")
    });
    let repack = match packing {
        Packing::SharedRows => repack_shared_mesh_varying_rows,
        Packing::ClipVaryings if keep_clips => repack_mesh_varying_pair_keeping_clips,
        Packing::ClipVaryings => repack_mesh_varying_pair,
    };
    let (mesh_host, pixel_host) = match repack(&mesh.container, &pixel.container) {
        Ok(pair) => pair,
        Err(reason) => {
            tracing::warn!(mesh_guest = %mesh.hash, pixel_guest = %pixel.hash, keep_clips, %reason,
                "mesh varying repack declined");
            return None;
        }
    };
    if !mesh.translated()?.host.has_same_bindings(&mesh_host)
        || !pixel.translated()?.host.has_same_bindings(&pixel_host)
    {
        tracing::warn!(mesh_guest = %mesh.hash, pixel_guest = %pixel.hash, keep_clips,
            "mesh varying repack changed root bindings");
        return None;
    }
    tracing::info!(target: "victory_gpu::memory_watch", mesh_guest = %mesh.hash, pixel_guest = %pixel.hash, keep_clips,
        "using repacked mesh varying pair");
    Some([mesh_host.dxil, pixel_host.dxil])
}

/// Opt-in: 1 logs over-budget shaders. Comma-separated guest shader hashes
/// also select those pipelines, even when their mesh outputs fit.
pub(super) fn trace(mesh: &Shader, pixel: Option<&Shader>, key: &str) {
    static FILTER: OnceLock<Option<Vec<String>>> = OnceLock::new();
    let Some(filter) = FILTER.get_or_init(|| {
        let value = std::env::var("VICTORY_TRACE_MESH_BUDGET").ok()?;
        if value.trim() == "1" {
            return Some(Vec::new());
        }
        let hashes: Vec<_> = value
            .split(',')
            .map(str::trim)
            .filter(|hash| hash.len() == 16 && hash.bytes().all(|b| b.is_ascii_hexdigit()))
            .map(str::to_ascii_lowercase)
            .collect();
        (!hashes.is_empty()).then_some(hashes)
    }) else {
        return;
    };
    if !tracing::enabled!(target: "victory_gpu::memory_watch", tracing::Level::INFO) {
        return;
    }

    let pixel_guest = pixel.map(|shader| shader.hash.as_str());
    let selected = filter
        .iter()
        .any(|hash| hash == &mesh.hash || pixel_guest == Some(hash.as_str()));
    let mesh_budget = mesh
        .translated()
        .and_then(|shader| Container::parse(&shader.host.dxil).ok())
        .and_then(|dxil| {
            let out = outputs(&dxil)?;
            Some((dxil, out))
        });
    let Some((mesh_dxil, out)) = mesh_budget else {
        if selected {
            tracing::info!(target: "victory_gpu::memory_watch", mesh_guest = %mesh.hash, ?pixel_guest, key,
                "mesh output budget unavailable");
        }
        return;
    };
    let bytes = out.bytes();
    if bytes <= LIMIT && !selected {
        return;
    }
    let pixel_dxil = pixel
        .and_then(|shader| shader.translated())
        .and_then(|shader| Container::parse(&shader.host.dxil).ok());
    tracing::info!(target: "victory_gpu::memory_watch",
        frame = crate::frame::current(), mesh_guest = %mesh.hash, ?pixel_guest, key,
        max_vertices = out.max_vertices, max_primitives = out.max_primitives,
        vertex_vectors = out.vertex_vectors, primitive_vectors = out.primitive_vectors,
        vertex_components = out.vertex_components, primitive_components = out.primitive_components,
        bytes, limit = LIMIT, over_limit = bytes > LIMIT,
        mesh_dxbc = ?words(mesh_dxil.digest), mesh_hash = ?program_hash(&mesh_dxil),
        pixel_dxbc = ?pixel_dxil.as_ref().map(|dxil| words(dxil.digest)),
        pixel_hash = ?pixel_dxil.as_ref().and_then(program_hash),
        "mesh output budget");
}
