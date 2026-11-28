//! Presented frames written to PNG files, to see what reaches the screen:
//! with `VICTORY_CAPTURE=<folder>`, every frame whose number is a power of
//! two (the frames the log reports) is saved as `frame-<n>.png`, at half
//! size. Only in builds with diagnostics.

mod cadence;
pub(crate) mod lights;
mod request;
pub(crate) mod planes;

use std::path::PathBuf;
use std::sync::{
    LazyLock,
    atomic::{AtomicU64, Ordering},
};

use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;

/// The folder frames go to, if capturing.
pub(crate) fn folder() -> Option<&'static PathBuf> {
    static FOLDER: std::sync::OnceLock<Option<PathBuf>> = std::sync::OnceLock::new();
    FOLDER
        .get_or_init(|| std::env::var_os("VICTORY_CAPTURE").map(PathBuf::from))
        .as_ref()
}

/// Frames listed in `VICTORY_CAPTURE_FRAMES` (comma-separated): saved
/// too, and their draws traced at debug level.
pub(crate) fn listed(n: u64) -> bool {
    static LIST: std::sync::OnceLock<Vec<u64>> = std::sync::OnceLock::new();
    let named = LIST
        .get_or_init(|| {
            let list = std::env::var("VICTORY_CAPTURE_FRAMES").unwrap_or_default();
            list.split(',')
                .filter_map(|f| f.trim().parse().ok())
                .collect()
        })
        .contains(&n);
    // `VICTORY_CAPTURE_WORLD`: also the 2nd and the 6th frame after the first
    // one that ran 500 draws or more (a title's first 3D frames: their
    // numbers depend on how long the menus were shown), and the 30th and
    // the 80th (the title opens with a cutscene made in the
    // engine: later shots of it, in case the first is dark by design).
    //
    // `VICTORY_CAPTURE_WORLD_ALSO` (comma-separated): more frames counted
    // from that first one (the title's blue patches show from
    // about the 118th frame on, when the window's model has arrived and
    // the reflections hold values: none of the four above has them).
    static ALSO: std::sync::OnceLock<Vec<u64>> = std::sync::OnceLock::new();
    let also = ALSO.get_or_init(|| {
        let list = std::env::var("VICTORY_CAPTURE_WORLD_ALSO").unwrap_or_default();
        list.split(',')
            .filter_map(|f| f.trim().parse().ok())
            .collect()
    });
    let first = WORLD.load(std::sync::atomic::Ordering::Relaxed);
    let after = n.wrapping_sub(first);
    named || first != u64::MAX && (also.contains(&after)
        || !WORLD_CADENCE.png_only && [2, 6, 30, 80].contains(&after))
}

static INDIRECT_FRAMES: LazyLock<u64> = LazyLock::new(|| {
    std::env::var("VICTORY_TRACE_FIRST_INDIRECT")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|n| (1..=16).contains(n))
        .unwrap_or(0)
});
static FIRST_INDIRECT_FRAME: AtomicU64 = AtomicU64::new(u64::MAX);

pub(crate) fn first_indirect() {
    if *INDIRECT_FRAMES != 0 && folder().is_some() {
        let frame = crate::queue::presented() + 1;
        FIRST_INDIRECT_FRAME.store(frame, Ordering::Relaxed);
        tracing::warn!(frame, frames = *INDIRECT_FRAMES, "tracing first indirect commands");
    }
}

/// With a capture folder, for a listed or explicitly requested frame: `what` is added to
/// the frame's `frame-<n>-ops.txt`, one line per draw, dispatch, copy or
/// drop in the order recorded (the title records on several threads, so
/// lines of different lists interleave; each names its list). The file
/// says which pass writes which target, to find the pass whose output
/// stays empty.
#[cfg(feature = "diagnostics")]
pub(crate) fn op(list: usize, what: impl FnOnce() -> String) {
    use std::io::Write;
    let frame = crate::queue::presented() + 1;
    let first_indirect = FIRST_INDIRECT_FRAME.load(Ordering::Relaxed);
    let trace_indirect = first_indirect != u64::MAX
        && frame.wrapping_sub(first_indirect) < *INDIRECT_FRAMES;
    if !trace_indirect && !listed(frame) && !target_frame(frame) {
        // Before the world: the last lines are kept, and written out when
        // it starts (`before-world-ops.txt`): what the title drew just
        // before (the title shows a title card there, whose text
        // did not show).
        if world_on() && !world_only() && WORLD.load(std::sync::atomic::Ordering::Relaxed) == u64::MAX {
            // At most 300 lines of a frame: the world's first frame alone
            // (thousands of operations, recorded before it is known to be
            // the world's) pushed everything before it out.
            static IN_FRAME: std::sync::Mutex<(u64, u32)> = std::sync::Mutex::new((0, 0));
            {
                let mut in_frame = IN_FRAME.lock().unwrap_or_else(|e| e.into_inner());
                if in_frame.0 != frame {
                    *in_frame = (frame, 0);
                }
                in_frame.1 += 1;
                if in_frame.1 > 300 {
                    return;
                }
            }
            let what = what();
            let mut ring = RING.lock().unwrap_or_else(|e| e.into_inner());
            if ring.len() >= RING_LINES {
                ring.pop_front();
            }
            ring.push_back(format!("frame {frame} list {list:#x} {what}"));
        }
        return;
    }
    let Some(folder) = folder() else {
        return;
    };
    static FILE: std::sync::Mutex<Option<(u64, u64, std::fs::File)>> = std::sync::Mutex::new(None);
    let what = what();
    let mut file = FILE.lock().unwrap_or_else(|e| e.into_inner());
    if file.as_ref().is_none_or(|f| f.0 != frame) {
        let Ok(made) = std::fs::File::create(folder.join(format!("frame-{frame}-ops.txt"))) else {
            return;
        };
        *file = Some((frame, 0, made));
    }
    if let Some((_, n, f)) = file.as_mut() {
        *n += 1;
        let _ = writeln!(f, "{n} list {list:#x} {what}");
    }
}

/// The last lines of [`op`] before the world.
#[cfg(feature = "diagnostics")]
static RING: std::sync::Mutex<std::collections::VecDeque<String>> =
    std::sync::Mutex::new(std::collections::VecDeque::new());
#[cfg(feature = "diagnostics")]
const RING_LINES: usize = 6000;

/// Whether `VICTORY_CAPTURE_WORLD` is set.
fn world_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_CAPTURE_WORLD").is_some())
}

fn world_only() -> bool {
    static ONLY: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ONLY.get_or_init(|| std::env::var_os("VICTORY_CAPTURE_WORLD").is_some_and(|v| v == "only"))
}

// With VICTORY_CAPTURE_WORLD=only, select a PNG every 1–64 world presentations.
static WORLD_CADENCE: LazyLock<cadence::WorldCadence> = LazyLock::new(|| {
    cadence::WorldCadence::new(
        world_only(),
        std::env::var("VICTORY_CAPTURE_WORLD_INTERVAL").ok().as_deref(),
    )
});

/// The first frame that ran 500 draws or more (`VICTORY_CAPTURE_WORLD`).
static WORLD: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(u64::MAX);

/// At the present of `frame`: the draws run on the host so far.
pub(crate) fn frame_drew(frame: u64, drawn: u64) {
    use std::sync::atomic::Ordering::Relaxed;
    static LAST: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let before = LAST.swap(drawn, Relaxed);
    if world_on()
        && drawn.saturating_sub(before) >= 500
        && WORLD.compare_exchange(u64::MAX, frame, Relaxed, Relaxed).is_ok()
    {
        tracing::info!(frame, png_interval = WORLD_CADENCE.interval, "the first frame of 500 draws or more");
        #[cfg(feature = "diagnostics")]
        if !world_only() && let Some(folder) = folder() {
            let lines: Vec<String> = std::mem::take(&mut *RING.lock().unwrap_or_else(|e| e.into_inner())).into();
            let _ = std::fs::write(folder.join("before-world-ops.txt"), lines.join("
"));
        }
    }
}

/// Whether frame `n` is saved.
pub(crate) fn wanted(n: u64) -> bool {
    if folder().is_none() {
        return false;
    }
    let first = WORLD.load(Ordering::Relaxed);
    let loading = world_on() && !world_only() && first == u64::MAX && n.is_multiple_of(16);
    let regular = !WORLD_CADENCE.png_only && (n.is_power_of_two() || loading);
    regular || target_frame(n) || listed(n) || world_on() && WORLD_CADENCE.scheduled(n, first) || capture_requested(n)
}

/// Separate input planes are only recorded with the opt-in on an already requested world PNG.
pub(crate) fn present_planes(frame: u64) -> bool {
    static ON: LazyLock<bool> = LazyLock::new(|| {
        std::env::var_os("VICTORY_CAPTURE_PRESENT_PLANES").is_some_and(|v| v == "1")
    });
    *ON && frame >= WORLD.load(Ordering::Relaxed)
}

fn capture_requested(frame: u64) -> bool {
    use std::sync::LazyLock;
    use std::sync::atomic::{AtomicU64, Ordering};

    static REQUEST: LazyLock<Option<PathBuf>> =
        LazyLock::new(|| std::env::var_os("VICTORY_CAPTURE_FILE").map(PathBuf::from));
    static SELECTED: AtomicU64 = AtomicU64::new(0);
    if SELECTED.load(Ordering::Relaxed) == frame {
        return true;
    }
    if frame.is_multiple_of(8) && REQUEST.as_ref().is_some_and(|p| std::fs::remove_file(p).is_ok()) {
        SELECTED.store(frame, Ordering::Relaxed);
        return true;
    }
    false
}

/// Saves 8-bit RGBA or BGRA rows (`pitch` bytes apart) as frame `n`.
pub(crate) fn save(n: u64, width: u32, height: u32, pitch: usize, bgra: bool, rows: &[u8]) {
    save_as(&format!("frame-{n}"), width, height, pitch, bgra, rows);
}

/// Saves 8-bit RGBA or BGRA rows as `<name>.png`, at half size.
fn save_as(name: &str, width: u32, height: u32, pitch: usize, bgra: bool, rows: &[u8]) -> bool {
    let Some(folder) = folder() else { return false };
    let (w, h) = ((width / 2).max(1), (height / 2).max(1));
    // Filter byte 0 per row, then RGB of every other pixel of every other row.
    let mut raw = Vec::with_capacity((w as usize * 3 + 1) * h as usize);
    for y in 0..h as usize {
        raw.push(0);
        let row = &rows[(2 * y).min(height as usize - 1) * pitch..];
        for x in 0..w as usize {
            let p = &row[(2 * x).min(width as usize - 1) * 4..][..3];
            if bgra {
                raw.extend([p[2], p[1], p[0]]);
            } else {
                raw.extend_from_slice(p);
            }
        }
    }
    let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut header = Vec::new();
    header.extend(w.to_be_bytes());
    header.extend(h.to_be_bytes());
    header.extend([8, 2, 0, 0, 0]); // 8-bit RGB
    chunk(&mut png, b"IHDR", &header);
    let mut compressed = vec![0u8; zlib_rs::compress_bound(raw.len())];
    let (idat, rc) = zlib_rs::compress_slice(&mut compressed, &raw, zlib_rs::DeflateConfig::new(3));
    debug_assert_eq!(rc, zlib_rs::ReturnCode::Ok);
    chunk(&mut png, b"IDAT", idat);
    chunk(&mut png, b"IEND", &[]);
    let path = folder.join(format!("{name}.png"));
    match std::fs::write(&path, png) {
        Ok(()) => {
            tracing::info!(path = %path.display(), "frame saved");
            true
        }
        Err(e) => {
            tracing::warn!(error = %e, path = %path.display(), "frame not saved");
            false
        }
    }
}

fn chunk(png: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    png.extend((data.len() as u32).to_be_bytes());
    let start = png.len();
    png.extend(kind);
    png.extend(data);
    let crc = crc32(&png[start..]);
    png.extend(crc.to_be_bytes());
}

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &b in bytes {
        crc ^= u32::from(b);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb8_8320 & (crc & 1).wrapping_neg());
        }
    }
    !crc
}

/// Records a copy of `texture` (in the copy-source state) into a
/// readback buffer.
pub(crate) fn read_back(
    device: &ID3D12Device,
    list: &ID3D12GraphicsCommandList,
    texture: &ID3D12Resource,
) -> Option<(ID3D12Resource, D3D12_PLACED_SUBRESOURCE_FOOTPRINT)> {
    read_back_of(device, list, texture, 0)
}

/// As [`read_back`], of subresource `sub` (the stencil plane of a
/// one-level, one-slice depth-stencil texture is subresource 1).
pub(crate) fn read_back_of(
    device: &ID3D12Device,
    list: &ID3D12GraphicsCommandList,
    texture: &ID3D12Resource,
    sub: u32,
) -> Option<(ID3D12Resource, D3D12_PLACED_SUBRESOURCE_FOOTPRINT)> {
    let mut footprint = D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default();
    let mut total = 0;
    // SAFETY: a live texture; outputs for one subresource.
    unsafe {
        let desc = texture.GetDesc();
        device.GetCopyableFootprints(
            &desc,
            sub,
            1,
            0,
            Some(&mut footprint),
            None,
            None,
            Some(&mut total),
        );
    }
    let buffer = readback_buffer(device, total).ok()?;
    // SAFETY: an open list and locations naming live resources.
    unsafe {
        let to = D3D12_TEXTURE_COPY_LOCATION {
            pResource: std::mem::transmute_copy(&buffer),
            Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                PlacedFootprint: footprint,
            },
        };
        let from = D3D12_TEXTURE_COPY_LOCATION {
            pResource: std::mem::transmute_copy(texture),
            Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                SubresourceIndex: sub,
            },
        };
        list.CopyTextureRegion(&to, 0, 0, 0, &from, None);
        Some((buffer, footprint))
    }
}

fn readback_buffer(device: &ID3D12Device, size: u64) -> windows::core::Result<ID3D12Resource> {
    let heap = D3D12_HEAP_PROPERTIES {
        Type: D3D12_HEAP_TYPE_READBACK,
        ..Default::default()
    };
    let desc = D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
        Width: size,
        Height: 1,
        DepthOrArraySize: 1,
        MipLevels: 1,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
        ..Default::default()
    };
    let mut buffer = None;
    // SAFETY: plain creation of a readback buffer.
    unsafe {
        device.CreateCommittedResource(
            &heap, D3D12_HEAP_FLAG_NONE, &desc, D3D12_RESOURCE_STATE_COPY_DEST, None, &mut buffer,
        )?;
    }
    buffer.ok_or_else(|| windows::core::Error::from_hresult(windows::Win32::Foundation::E_POINTER))
}

/// Saves a frame read back by [`read_back`] (8-bit color only).
pub(crate) fn save_read_back(
    frame: u64,
    buffer: &ID3D12Resource,
    footprint: &D3D12_PLACED_SUBRESOURCE_FOOTPRINT,
) {
    let f = footprint.Footprint;
    let bgra = match f.Format {
        DXGI_FORMAT_R8G8B8A8_UNORM | DXGI_FORMAT_R8G8B8A8_UNORM_SRGB => false,
        DXGI_FORMAT_B8G8R8A8_UNORM | DXGI_FORMAT_B8G8R8A8_UNORM_SRGB => true,
        other => {
            tracing::warn!(format = other.0, "frame not saved: not 8-bit color");
            return;
        }
    };
    let mut data = std::ptr::null_mut();
    // SAFETY: the copy finished (every submission is waited for); the
    // mapping spans the footprint's rows.
    unsafe {
        if buffer.Map(0, None, Some(&mut data)).is_err() {
            return;
        }
        // The last row of the footprint can end before its row padding.
        let rows = std::slice::from_raw_parts(data.cast::<u8>(), buffer.GetDesc().Width as usize);
        save(frame, f.Width, f.Height, f.RowPitch as usize, bgra, rows);
        buffer.Unmap(0, None);
    }
}

pub(crate) fn fenced_mesh_target_enabled() -> bool {
    static ENABLED: LazyLock<bool> = LazyLock::new(|| {
        std::env::var_os("VICTORY_FENCED_MESH_TARGET").is_some_and(|v| v == "1")
            && folder().is_some()
    });
    *ENABLED
}

static MESH_TARGET_TAKEN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub(crate) fn fenced_mesh_target_pending() -> bool {
    fenced_mesh_target_enabled() && !MESH_TARGET_TAKEN.load(Ordering::Relaxed)
}

/// At most one pair of readbacks per process, even across recording threads.
pub(crate) fn take_fenced_mesh_target() -> bool {
    !MESH_TARGET_TAKEN.swap(true, Ordering::Relaxed)
}

pub(crate) fn take_direct_mesh_target() -> bool {
    static ENABLED: LazyLock<bool> = LazyLock::new(|| {
        std::env::var_os("VICTORY_CAPTURE_DIRECT_MESH_COLOUR").is_some_and(|v| v == "1")
            && folder().is_some()
    });
    static TAKEN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    *ENABLED && !TAKEN.swap(true, Ordering::Relaxed)
}

pub(crate) fn fenced_mesh_occlusion_enabled() -> bool {
    static ENABLED: LazyLock<bool> = LazyLock::new(|| {
        std::env::var_os("VICTORY_FENCED_MESH_OCCLUSION").is_some_and(|v| v == "1")
    });
    *ENABLED
}

/// One binary occlusion query on the host's direct command list.
pub(crate) struct MeshOcclusion {
    heap: ID3D12QueryHeap,
    readback: ID3D12Resource,
}

impl MeshOcclusion {
    pub(crate) fn new(device: &ID3D12Device) -> windows::core::Result<Self> {
        let mut heap = None;
        // SAFETY: plain creation of one query slot.
        unsafe {
            device.CreateQueryHeap(
                &D3D12_QUERY_HEAP_DESC { Type: D3D12_QUERY_HEAP_TYPE_OCCLUSION, Count: 1, NodeMask: 0 },
                &mut heap,
            )?;
        }
        let heap = heap.ok_or_else(|| windows::core::Error::from_hresult(windows::Win32::Foundation::E_POINTER))?;
        Ok(Self { heap, readback: readback_buffer(device, size_of::<u64>() as u64)? })
    }

    /// Begins slot zero on an open direct list.
    pub(crate) fn begin(&self, list: &ID3D12GraphicsCommandList) {
        // SAFETY: the query heap has a live occlusion slot at index zero.
        unsafe { list.BeginQuery(&self.heap, D3D12_QUERY_TYPE_BINARY_OCCLUSION, 0) };
    }

    /// Ends the query on the same list and resolves it to the readback buffer.
    pub(crate) fn end(&self, list: &ID3D12GraphicsCommandList) {
        // SAFETY: the same query is active; its eight bytes fit this COPY_DEST buffer.
        unsafe {
            list.EndQuery(&self.heap, D3D12_QUERY_TYPE_BINARY_OCCLUSION, 0);
            list.ResolveQueryData(&self.heap, D3D12_QUERY_TYPE_BINARY_OCCLUSION, 0, 1, &self.readback, 0);
        }
    }

    /// Reads the result after the caller has checked the submission fence.
    pub(crate) fn read(&self) -> windows::core::Result<u64> {
        let mut data = std::ptr::null_mut();
        let range = D3D12_RANGE { Begin: 0, End: size_of::<u64>() };
        // SAFETY: the caller checked the fence, and the range contains the one result.
        unsafe {
            self.readback.Map(0, Some(&range), Some(&mut data))?;
            let value = (!data.is_null()).then(|| data.cast::<u64>().read_unaligned());
            self.readback.Unmap(0, Some(&D3D12_RANGE::default()));
            value.ok_or_else(|| windows::core::Error::from_hresult(windows::Win32::Foundation::E_POINTER))
        }
    }

    pub(crate) fn keep(&self, list: &crate::host::HostList) {
        list.keep(&self.heap);
        list.keep(&self.readback);
    }
}

/// Records a read of one bound, non-MSAA color target without changing its
/// state at the next draw.
pub(crate) fn read_render_target(
    list: &crate::host::HostList,
    texture: &ID3D12Resource,
) -> Option<(ID3D12Resource, D3D12_PLACED_SUBRESOURCE_FOOTPRINT)> {
    use crate::host::{host, transition};
    let host = host()?;
    let render = D3D12_RESOURCE_STATE_RENDER_TARGET;
    let copy = D3D12_RESOURCE_STATE_COPY_SOURCE;
    // SAFETY: the caller prepared the draw, leaving this live RTV in RENDER_TARGET.
    unsafe {
        list.list.ResourceBarrier(&[transition(texture, render, copy)]);
        let sample = read_back(&host.device, &list.list, texture);
        list.list.ResourceBarrier(&[transition(texture, copy, render)]);
        sample
    }
}

pub(crate) struct MeshTargetSample {
    pub(crate) frame: u64,
    pub(crate) arguments: u64,
    pub(crate) direct: bool,
    pub(crate) slot: usize,
    pub(crate) resource: usize,
    pub(crate) before: ID3D12Resource,
    pub(crate) after: ID3D12Resource,
    pub(crate) occlusion: Option<MeshOcclusion>,
    pub(crate) footprint: D3D12_PLACED_SUBRESOURCE_FOOTPRINT,
    pub(crate) reported: bool,
}

impl MeshTargetSample {
    /// Saves RGB previews and compares all four channels at full resolution.
    ///
    /// # Safety
    /// The fence for both copies must have completed.
    pub(crate) unsafe fn save(&self, stem: &str) -> Option<(usize, usize, u64, bool)> {
        let f = self.footprint.Footprint;
        let width = f.Width as usize;
        let pitch = f.RowPitch as usize;
        // An RGBA8 row of this width is already aligned for the copy.
        if self.footprint.Offset != 0 || (f.Width, f.Height) != (1280, 720) || pitch != width * 4 {
            return None;
        }
        let len = pitch * f.Height as usize;
        with_readback_rows(&self.before, len, |before| {
            with_readback_rows(&self.after, len, |after| {
                let (pixels, rgb_pixels, bits) = rgba_differences(width, pitch, before, after);
                let before_saved = save_as(&format!("{stem}-before"), f.Width, f.Height, pitch, false, before);
                let after_saved = save_as(&format!("{stem}-after"), f.Width, f.Height, pitch, false, after);
                (pixels, rgb_pixels, bits, before_saved && after_saved)
            })
        })?
    }
}

fn with_readback_rows<T>(buffer: &ID3D12Resource, len: usize, read: impl FnOnce(&[u8]) -> T) -> Option<T> {
    let range = D3D12_RANGE { Begin: 0, End: len };
    let mut data = std::ptr::null_mut();
    // SAFETY: the copies finished, and the range covers only the stored rows.
    unsafe {
        buffer.Map(0, Some(&range), Some(&mut data)).ok()?;
        let result = (!data.is_null()).then(|| read(std::slice::from_raw_parts(data.cast::<u8>(), len)));
        buffer.Unmap(0, Some(&D3D12_RANGE::default()));
        result
    }
}

fn rgba_differences(width: usize, pitch: usize, before: &[u8], after: &[u8]) -> (usize, usize, u64) {
    let mut changed_pixels = 0;
    let mut changed_rgb_pixels = 0;
    let mut changed_bits = 0;
    for (old, new) in before.chunks(pitch).zip(after.chunks(pitch)) {
        for (a, b) in old[..width * 4].chunks_exact(4).zip(new[..width * 4].chunks_exact(4)) {
            if a != b {
                changed_pixels += 1;
                changed_rgb_pixels += usize::from(a[..3] != b[..3]);
                changed_bits += a.iter().zip(b).map(|(&x, &y)| u64::from((x ^ y).count_ones())).sum::<u64>();
            }
        }
    }
    (changed_pixels, changed_rgb_pixels, changed_bits)
}

/// With `VICTORY_DUMP_TARGETS` set: title textures the frames listed in
/// `VICTORY_CAPTURE_FRAMES` render to or write, in first-use order, saved
/// at that frame's present as `frame-<n>-<i>-<resource>-<w>x<h>-f<format>.png`
/// (what each holds at the end of the frame; formats normalized to show
/// structure: floats scaled by their largest value, depth by its range).
/// The host copies are held: the title may release a target before the
/// frame is presented.
type HeldTargets = Vec<(usize, std::sync::Arc<crate::resource::HostTexture>)>;
static TARGETS: std::sync::Mutex<std::collections::BTreeMap<u64, HeldTargets>> =
    std::sync::Mutex::new(std::collections::BTreeMap::new());

static TARGET_REQUEST: LazyLock<Option<request::TargetRequest>> = LazyLock::new(|| {
    folder()?;
    let path = std::env::var_os("VICTORY_CAPTURE_TARGETS_FILE")?;
    Some(request::TargetRequest::new(path.into()))
});

pub(crate) fn target_frame(frame: u64) -> bool {
    TARGET_REQUEST.as_ref().is_some_and(|r| r.selects(frame))
}

/// With VICTORY_CAPTURE_TARGETS_FILE, touching that path requests a deferred
/// render-target capture plus its matching composited PNG on a full frame.
pub(crate) fn poll_targets(finished: u64) {
    if let Some(frame) = TARGET_REQUEST.as_ref().and_then(|r| r.after_present(finished, crate::queue::presented)) {
        tracing::info!(frame, "armed one-frame render target capture");
    }
}

fn dump_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| folder().is_some() && std::env::var_os("VICTORY_DUMP_TARGETS").is_some())
}

fn hold_targets(frame: u64, resources: impl Iterator<Item = usize>) {
    let mut pending = TARGETS.lock().unwrap_or_else(|e| e.into_inner());
    // The recording thread may have crossed a present while taking the lock.
    if crate::queue::presented() >= frame {
        return;
    }
    let targets = pending.entry(frame).or_default();
    for resource in resources {
        if !targets.iter().any(|&(id, _)| id == resource)
            && let Some(copy) = crate::resource::host_copy(resource as crate::object::This)
        {
            targets.push((resource, copy));
        }
    }
}

/// Hold the exact same frame's render targets, depth and texture UAVs.
/// The one-shot path also captures single-target lighting and exposure passes.
pub(crate) fn note_target(resource: crate::object::This) {
    if resource.is_null() { return; }
    let frame = crate::queue::presented() + 1;
    if target_frame(frame) || dump_on() && listed(frame) {
        hold_targets(frame, std::iter::once(resource as usize));
    }
}

/// At the present of frame `frame`: saves only the targets noted for that frame.
pub(crate) fn dump_targets(frame: u64) {
    use std::sync::atomic::Ordering::Relaxed;
    let targets = TARGETS.lock().unwrap_or_else(|e| e.into_inner()).remove(&frame).unwrap_or_default();
    if target_frame(frame) {
        tracing::info!(frame, count = targets.len(), "reading captured render targets");
    }
    let Some(host) = crate::host::host() else { return };
    for (i, (resource, copy)) in targets.iter().enumerate() {
        let texture = &copy.texture;
        // SAFETY: a live resource.
        let d = unsafe { texture.GetDesc() };
        if d.SampleDesc.Count > 1 || d.Dimension != D3D12_RESOURCE_DIMENSION_TEXTURE2D {
            continue;
        }
        // Keep a lamp comparison below the title's progress watchdog. The four
        // per-draw readbacks are recorded separately on the rendering list.
        static ROOM_ONLY: LazyLock<bool> = LazyLock::new(|| std::env::var_os("VICTORY_CAPTURE_ROOM_TARGETS").is_some());
        if *ROOM_ONLY && target_frame(frame) {
            let size = (d.Width, d.Height, d.Format.0);
            if !matches!(size, (1280, 720, 27) | (1920, 1080, 67) | (1, 1, 2 | 10)) {
                continue;
            }
        }
        // Every submission has run; the dump's list moves it there and back.
        let state = copy.state.load(Relaxed);
        dump_one(host, texture, state, frame, i, *resource as crate::object::This, &d);
    }
}

fn dump_one(
    host: &crate::host::Host,
    texture: &ID3D12Resource,
    state: u32,
    frame: u64,
    i: usize,
    resource: crate::object::This,
    d: &D3D12_RESOURCE_DESC,
) {
    let Ok(list) = host.new_list() else { return };
    let state = D3D12_RESOURCE_STATES(crate::host::legal_state(texture, state) as i32);
    let copy = D3D12_RESOURCE_STATE_COPY_SOURCE;
    // SAFETY: an open list of ours and a live texture.
    let read = unsafe {
        if state != copy {
            list.list
                .ResourceBarrier(&[crate::host::transition(texture, state, copy)]);
        }
        let read = read_back(&host.device, &list.list, texture);
        // The stencil plane of a depth-stencil target, which the title's
        // lighting reads (material class in the low bits; 0x08 "inside a
        // room", which the lamps of a room are multiplied by): the plane
        // after the depth plane of a one-level, one-slice texture.
        let stencil = (matches!(d.Format.0, 19..=22 | 44..=47)
            && d.MipLevels == 1
            && d.DepthOrArraySize == 1)
            .then(|| read_back_of(&host.device, &list.list, texture, 1))
            .flatten();
        if state != copy {
            list.list
                .ResourceBarrier(&[crate::host::transition(texture, copy, state)]);
        }
        (read, stencil)
    };
    let (read, stencil) = read;
    list.close();
    host.execute(&[&list]);
    if let Some((buffer, footprint)) = stencil {
        stencil_values(&buffer, &footprint, frame, resource);
    }
    let Some((buffer, footprint)) = read else { return };
    let f = footprint.Footprint;
    let mut data = std::ptr::null_mut();
    // SAFETY: the copy finished; the mapping spans the footprint's rows.
    unsafe {
        if buffer.Map(0, None, Some(&mut data)).is_err() {
            return;
        }
        let pitch = f.RowPitch as usize;
        // The last row of the footprint can end before its row padding.
        let rows = std::slice::from_raw_parts(data.cast::<u8>(), buffer.GetDesc().Width as usize);
        // A 24-bit depth plane comes back as 32-bit words: which depth values
        // (low 24 bits) and which top bytes it holds, the most common first.
        if matches!(d.Format.0, 44..=47) {
            let mut depths = std::collections::HashMap::<u32, u32>::new();
            let mut tops = std::collections::HashMap::<u32, u32>::new();
            for y in 0..f.Height as usize {
                for x in 0..f.Width as usize {
                    let at = y * pitch + x * 4;
                    let w = u32::from_le_bytes(rows[at..at + 4].try_into().unwrap_or([0; 4]));
                    *depths.entry(w & 0xff_ffff).or_default() += 1;
                    *tops.entry(w >> 24).or_default() += 1;
                }
            }
            let most = |m: std::collections::HashMap<u32, u32>| {
                let mut v: Vec<(u32, u32)> = m.into_iter().collect();
                v.sort_by_key(|e| std::cmp::Reverse(e.1));
                (v.len(), v.into_iter().take(12).collect::<Vec<_>>())
            };
            tracing::info!(?resource, depths = ?most(depths), top_bytes = ?most(tops), "24-bit depth plane values (value, texels)");
        }
        if let Some(rgba) = to_rgba(f.Format.0 as u32, f.Width as usize, f.Height as usize, pitch, rows) {
            let name = format!(
                "frame-{frame}-{i:02}-{resource:?}-{}x{}-f{}",
                d.Width, d.Height, d.Format.0
            );
            save_as(&name, f.Width, f.Height, f.Width as usize * 4, false, &rgba);
            if f.Format.0 == 67 && target_frame(frame) {
                save_luminance(&name, 67, f.Width as usize, f.Height as usize, pitch, rows);
            }
        } else {
            tracing::info!(format = f.Format.0, ?resource, "target not dumped: format");
        }
        buffer.Unmap(0, None);
    }
}

/// Logs which values a stencil plane read back by [`read_back_of`] holds
/// (the most common first), how many texels have each of its bits, and the
/// box of the texels with bit 3 (0x08).
fn stencil_values(
    buffer: &ID3D12Resource,
    footprint: &D3D12_PLACED_SUBRESOURCE_FOOTPRINT,
    frame: u64,
    resource: crate::object::This,
) {
    let f = footprint.Footprint;
    let (w, h, pitch) = (f.Width as usize, f.Height as usize, f.RowPitch as usize);
    let mut data = std::ptr::null_mut();
    // SAFETY: the copy finished; the mapping spans the footprint's rows of
    // one byte a texel, the last row without its padding.
    unsafe {
        if h == 0 || w == 0 || buffer.Map(0, None, Some(&mut data)).is_err() {
            return;
        }
        let rows = std::slice::from_raw_parts(data.cast::<u8>(), pitch * (h - 1) + w);
        let mut values = [0u32; 256];
        let mut bits = [0u32; 8];
        // Left, top, right, bottom of the texels with bit 3.
        let mut inside = [u32::MAX, u32::MAX, 0, 0];
        for y in 0..h {
            for (x, &v) in rows[y * pitch..y * pitch + w].iter().enumerate() {
                values[usize::from(v)] += 1;
                for (bit, count) in bits.iter_mut().enumerate() {
                    *count += u32::from(v >> bit & 1);
                }
                if v & 8 != 0 {
                    inside = [
                        inside[0].min(x as u32),
                        inside[1].min(y as u32),
                        inside[2].max(x as u32),
                        inside[3].max(y as u32),
                    ];
                }
            }
        }
        let mut most: Vec<(usize, u32)> = values
            .iter()
            .copied()
            .enumerate()
            .filter(|v| v.1 != 0)
            .collect();
        most.sort_by_key(|v| std::cmp::Reverse(v.1));
        most.truncate(16);
        tracing::info!(
            frame,
            ?resource,
            width = w,
            height = h,
            values = ?most,
            texels_with_bit = ?bits,
            bit3_box = ?inside,
            "stencil plane values (value, texels)"
        );
        buffer.Unmap(0, None);
    }
}

fn half(h: u16) -> f32 {
    let (sign, exp, man) = ((h >> 15) as u32, ((h >> 10) & 0x1f) as i32, (h & 0x3ff) as f32);
    let v = match exp {
        0 => man * 2f32.powi(-24),
        31 => f32::INFINITY,
        e => (1.0 + man / 1024.0) * 2f32.powi(e - 15),
    };
    if sign == 1 { -v } else { v }
}

/// A float packed in `bits` with a 5-bit exponent and `man` mantissa bits
/// (the 11- and 10-bit floats of R11G11B10).
fn small_float(bits: u32, man: u32) -> f32 {
    let (exp, m) = ((bits >> man) & 0x1f, (bits & ((1 << man) - 1)) as f32);
    match exp {
        0 => m / (1 << man) as f32 * 2f32.powi(-14),
        31 => f32::INFINITY,
        e => (1.0 + m / (1 << man) as f32) * 2f32.powi(e as i32 - 15),
    }
}

/// DXGI R9G9B9E5_SHAREDEXP, linear channels in scene units.
fn rgb9e5(v: u32) -> [f32; 3] {
    let scale = 2f32.powi((v >> 27) as i32 - 24);
    [0, 9, 18].map(|shift| ((v >> shift) & 511) as f32 * scale)
}

/// Netpbm floating point greyscale. Sample the same coordinates as the PNG;
/// PFM stores rows from the bottom and a negative scale means little-endian.
fn linear_luminance(format: i32, w: usize, h: usize, pitch: usize, rows: &[u8]) -> Option<Vec<u8>> {
    let bytes = match format { 10 => 8, 67 => 4, _ => return None };
    let (sw, sh) = ((w / 2).max(1), (h / 2).max(1));
    let mut out = format!("Pf\n{sw} {sh}\n-1.0\n").into_bytes();
    out.reserve(sw * sh * 4);
    for y in (0..sh).rev() {
        for x in 0..sw {
            let p = &rows[2 * y * pitch + 2 * x * bytes..];
            let [r, g, b] = if format == 67 {
                rgb9e5(u32::from_le_bytes(p[..4].try_into().ok()?))
            } else {
                [0, 2, 4].map(|i| half(u16::from_le_bytes([p[i], p[i + 1]])))
            };
            out.extend_from_slice(&(0.2126 * r + 0.7152 * g + 0.0722 * b).to_le_bytes());
        }
    }
    Some(out)
}

fn save_luminance(name: &str, format: i32, w: usize, h: usize, pitch: usize, rows: &[u8]) {
    if let Some(folder) = folder()
        && let Some(data) = linear_luminance(format, w, h, pitch, rows)
        && let Err(e) = std::fs::write(folder.join(format!("{name}-luma.pfm")), data) {
        tracing::warn!(%name, error = %e, "linear luminance not saved");
    }
}

/// Rows of a host format as 8-bit RGBA: 8-bit color as is, everything else
/// as channel values scaled so the largest finite one is white (single
/// channels: lowest black, highest white, as grey). `None` for formats not
/// handled.
fn to_rgba(format: u32, w: usize, h: usize, pitch: usize, rows: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(w * h * 4);
    let px = |x: usize, y: usize, n: usize| &rows[y * pitch + x * n..][..n];
    let u16_at = |b: &[u8], i: usize| u16::from_le_bytes([b[2 * i], b[2 * i + 1]]);
    let u32_at = |b: &[u8], i: usize| u32::from_le_bytes(b[4 * i..4 * i + 4].try_into().unwrap_or([0; 4]));
    // Channel values as floats, then scaled.
    type Decode = Box<dyn Fn(&[u8]) -> [f32; 3]>;
    let (n, decode): (usize, Decode) = match format {
        27..=32 => {
            for y in 0..h {
                for x in 0..w {
                    let p = px(x, y, 4);
                    out.extend([p[0], p[1], p[2], 255]);
                }
            }
            return Some(out);
        }
        // B8G8R8A8 and B8G8R8X8 (89 is R10G10B10_XR_BIAS_A2, below).
        87 | 88 | 90..=93 => {
            for y in 0..h {
                for x in 0..w {
                    let p = px(x, y, 4);
                    out.extend([p[2], p[1], p[0], 255]);
                }
            }
            return Some(out);
        }
        23..=25 | 89 => (4, Box::new(move |b| {
            let v = u32_at(b, 0);
            [(v & 0x3ff) as f32, (v >> 10 & 0x3ff) as f32, (v >> 20 & 0x3ff) as f32]
        })),
        26 => (4, Box::new(move |b| {
            let v = u32_at(b, 0);
            [small_float(v & 0x7ff, 6), small_float(v >> 11 & 0x7ff, 6), small_float(v >> 22, 5)]
        })),
        67 => (4, Box::new(move |b| rgb9e5(u32_at(b, 0)))),
        9..=10 => (8, Box::new(move |b| [half(u16_at(b, 0)), half(u16_at(b, 1)), half(u16_at(b, 2))])),
        11..=14 => (8, Box::new(move |b| [0, 1, 2].map(|i| f32::from(u16_at(b, i))))),
        33..=34 => (4, Box::new(move |b| [half(u16_at(b, 0)), half(u16_at(b, 1)), 0.0])),
        1..=2 => (16, Box::new(move |b| {
            [0, 1, 2].map(|i| f32::from_bits(u32_at(b, i)))
        })),
        15..=16 => (8, Box::new(move |b| [f32::from_bits(u32_at(b, 0)), f32::from_bits(u32_at(b, 1)), 0.0])),
        35..=38 => (4, Box::new(move |b| [f32::from(u16_at(b, 0)), f32::from(u16_at(b, 1)), 0.0])),
        48..=52 => (2, Box::new(move |b| [f32::from(b[0]), f32::from(b[1]), 0.0])),
        // Single channels (and depth planes): shown as grey.
        39..=43 => (4, Box::new(move |b| [f32::from_bits(u32_at(b, 0)); 3])),
        19..=22 => (8, Box::new(move |b| [f32::from_bits(u32_at(b, 0)); 3])),
        44..=47 => (4, Box::new(move |b| [(u32_at(b, 0) & 0xff_ffff) as f32; 3])),
        53..=54 => (2, Box::new(move |b| [half(u16_at(b, 0)); 3])),
        55..=59 => (2, Box::new(move |b| [f32::from(u16_at(b, 0)); 3])),
        60..=65 => (1, Box::new(move |b| [f32::from(b[0]); 3])),
        _ => return None,
    };
    let values: Vec<[f32; 3]> = (0..h)
        .flat_map(|y| (0..w).map(move |x| (x, y)))
        .map(|(x, y)| decode(px(x, y, n)))
        .collect();
    let grey = matches!(format, 19..=22 | 39..=47 | 53..=65);
    let finite = values.iter().flatten().copied().filter(|v| v.is_finite());
    let (low, high) = if grey {
        finite.fold((f32::MAX, f32::MIN), |(l, h), v| (l.min(v), h.max(v)))
    } else {
        (0.0, finite.fold(0.0f32, f32::max))
    };
    let range = (high - low).max(1e-20);
    // How much of the target holds anything: a lit scene and one bright
    // lamp on black have the same highest value.
    let above = values.iter().filter(|v| v.iter().any(|c| c.is_finite() && *c > low)).count();
    let nonzero_percent = above as f32 * 100.0 / values.len().max(1) as f32;
    tracing::info!(format, low, high, nonzero_percent, "target values");
    // Small float colour targets (the 1x1 luminance and exposure targets):
    // every texel's four channels as numbers. The picture has no alpha and
    // eight bits a channel.
    if matches!(format, 1..=2 | 9..=10) && w * h <= 16 {
        for y in 0..h {
            for x in 0..w {
                let b = px(x, y, n);
                let texel: [f32; 4] = if n == 16 {
                    [0, 1, 2, 3].map(|i| f32::from_bits(u32_at(b, i)))
                } else {
                    [0, 1, 2, 3].map(|i| half(u16_at(b, i)))
                };
                tracing::info!(format, x, y, ?texel, "target texel");
            }
        }
    }
    // R32_UINT targets hold packed light (the title reads them back as
    // R9G9B9E5): as floats their low, high and share above the lowest say
    // nothing. How many texels hold any bit, and the highest channel value
    // of the packed reading.
    if format == 42 {
        let (mut set, mut packed_high) = (0usize, 0f32);
        for y in 0..h {
            for x in 0..w {
                let v = u32_at(px(x, y, 4), 0);
                set += usize::from(v != 0);
                packed_high = rgb9e5(v).into_iter().fold(packed_high, f32::max);
            }
        }
        let nonzero_bits_percent = set as f32 * 100.0 / (w * h).max(1) as f32;
        tracing::info!(format, nonzero_bits_percent, packed_high, "target values as packed light");
        // The same words as plain numbers, by class. Tables of indices and
        // masks say nothing as floats or as packed light: in the title's
        // shadow-mask table 0 is a block in full shadow, a word up to 255
        // one value for the block, above that a block of the atlas, and all
        // ones a texel never written.
        let mut classes = [0usize; 6];
        let mut highest_below_ones = 0u32;
        for y in 0..h {
            for x in 0..w {
                let v = u32_at(px(x, y, 4), 0);
                classes[match v {
                    0 => 0,
                    1..=15 => 1,
                    16..=255 => 2,
                    256..=0x00ff_ffff => 3,
                    u32::MAX => 5,
                    _ => 4,
                }] += 1;
                if v != u32::MAX {
                    highest_below_ones = highest_below_ones.max(v);
                }
            }
        }
        tracing::info!(
            format,
            width = w,
            height = h,
            zero = classes[0],
            one_to_15 = classes[1],
            to_255 = classes[2],
            to_24_bits = classes[3],
            larger = classes[4],
            all_ones = classes[5],
            highest_below_ones,
            "target values as words"
        );
    }
    // Float colours (scene light) go through a cube root: scaled linearly,
    // a night scene under one lamp of 50 units came out all black.
    let curved = matches!(format, 1..=2 | 9..=10 | 26 | 67);
    for v in values {
        let c = v.map(|c| {
            let t = ((c - low) / range).clamp(0.0, 1.0);
            ((if curved { t.cbrt() } else { t }) * 255.0) as u8
        });
        out.extend([c[0], c[1], c[2], 255]);
    }
    Some(out)
}

/// Pixels per side of the grid [`probe`] reads.
const GRID: u32 = 4;
/// Bytes between the grid's pixels in the readback buffer (placement
/// alignment).
const SLOT: u64 = 512;

/// Records copies of a grid of pixels of `texture` (8-bit color, in the
/// copy-source state) into a readback buffer, to tell black frames.
pub(crate) fn probe(
    device: &ID3D12Device,
    list: &ID3D12GraphicsCommandList,
    texture: &ID3D12Resource,
) -> Option<ID3D12Resource> {
    // SAFETY: a live resource.
    let d = unsafe { texture.GetDesc() };
    if !matches!(d.Format.0, 27..=29 | 87 | 90 | 91) {
        return None;
    }
    let desc = D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
        Width: SLOT * u64::from(GRID * GRID),
        Height: 1,
        DepthOrArraySize: 1,
        MipLevels: 1,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
        ..Default::default()
    };
    let heap = D3D12_HEAP_PROPERTIES {
        Type: D3D12_HEAP_TYPE_READBACK,
        ..Default::default()
    };
    let mut buffer: Option<ID3D12Resource> = None;
    // SAFETY: plain creation; an open list; locations naming live resources.
    unsafe {
        device
            .CreateCommittedResource(
                &heap,
                D3D12_HEAP_FLAG_NONE,
                &desc,
                D3D12_RESOURCE_STATE_COPY_DEST,
                None,
                &mut buffer,
            )
            .ok()?;
        let buffer = buffer?;
        for i in 0..GRID * GRID {
            let (x, y) = (
                (2 * (i % GRID) + 1) * d.Width as u32 / (2 * GRID),
                (2 * (i / GRID) + 1) * d.Height / (2 * GRID),
            );
            let to = D3D12_TEXTURE_COPY_LOCATION {
                pResource: std::mem::transmute_copy(&buffer),
                Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                    PlacedFootprint: D3D12_PLACED_SUBRESOURCE_FOOTPRINT {
                        Offset: SLOT * u64::from(i),
                        Footprint: D3D12_SUBRESOURCE_FOOTPRINT {
                            Format: d.Format,
                            Width: 1,
                            Height: 1,
                            Depth: 1,
                            RowPitch: 256,
                        },
                    },
                },
            };
            let from = D3D12_TEXTURE_COPY_LOCATION {
                pResource: std::mem::transmute_copy(texture),
                Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                    SubresourceIndex: 0,
                },
            };
            let pixel = D3D12_BOX {
                left: x,
                top: y,
                front: 0,
                right: x + 1,
                bottom: y + 1,
                back: 1,
            };
            list.CopyTextureRegion(&to, 0, 0, 0, &from, Some(&pixel));
        }
        Some(buffer)
    }
}

/// Whether every pixel [`probe`] read is (near) black.
pub(crate) fn black(buffer: &ID3D12Resource) -> bool {
    let mut data = std::ptr::null_mut();
    // SAFETY: the copies finished (every submission is waited for).
    unsafe {
        if buffer.Map(0, None, Some(&mut data)).is_err() {
            return false;
        }
        let bytes =
            std::slice::from_raw_parts(data.cast::<u8>(), (SLOT * u64::from(GRID * GRID)) as usize);
        let level = bytes
            .chunks(SLOT as usize)
            .flat_map(|pixel| pixel[..3].iter().copied())
            .max()
            .unwrap_or(0);
        buffer.Unmap(0, None);
        let mut levels = LEVELS.lock().unwrap_or_else(|e| e.into_inner());
        if levels.len() == 12 {
            levels.pop_front();
        }
        levels.push_back(level);
        level < 8
    }
}

/// The brightest probed channel of the last frames, oldest first: a fade
/// darkens over several, a glitch drops at once.
static LEVELS: std::sync::Mutex<std::collections::VecDeque<u8>> =
    std::sync::Mutex::new(std::collections::VecDeque::new());

pub(crate) fn recent_levels() -> Vec<u8> {
    let levels = LEVELS.lock().unwrap_or_else(|e| e.into_inner());
    levels.iter().copied().collect()
}

#[cfg(test)]
mod tests {
    #[test]
    fn shared_exponent_and_linear_wall_luminance() {
        assert_eq!(super::rgb9e5(0), [0.0; 3]);
        assert_eq!(super::rgb9e5(1), [2f32.powi(-24), 0.0, 0.0]);
        assert_eq!(super::rgb9e5(u32::MAX), [65408.0; 3]);
        let channels = 24 << 27 | 7 << 18 | 2 << 9 | 19;
        assert_eq!(super::rgb9e5(channels), [19.0, 2.0, 7.0]);
        let mut padded = [0u8; 3 * 256 + 8];
        padded[..4].copy_from_slice(&channels.to_le_bytes());
        padded[512..516].copy_from_slice(&(24u32 << 27 | 2 << 9).to_le_bytes());
        let rgba = super::to_rgba(67, 2, 4, 256, &padded).unwrap();
        assert_eq!(rgba[0], 255);
        assert!(rgba[2] > rgba[1] && rgba[1] > 0);
        assert_eq!(&rgba[4..8], &[0, 0, 0, 255]);
        let pfm = super::linear_luminance(67, 2, 4, 256, &padded).unwrap();
        let header = b"Pf\n1 2\n-1.0\n";
        assert_eq!(&pfm[..header.len()], header);
        let values: Vec<_> = pfm[header.len()..].chunks_exact(4)
            .map(|v| f32::from_le_bytes(v.try_into().unwrap())).collect();
        assert!((values[0] - 1.4304).abs() < 1e-5);
        assert!((values[1] - (0.2126 * 19.0 + 0.7152 * 2.0 + 0.0722 * 7.0)).abs() < 1e-5);
        let mut rgba16 = [0u8; 8];
        rgba16[2..4].copy_from_slice(&0x4000u16.to_le_bytes());
        let pfm = super::linear_luminance(10, 1, 1, 8, &rgba16).unwrap();
        assert!((f32::from_le_bytes(pfm[pfm.len()-4..].try_into().unwrap()) - 1.4304).abs() < 1e-5);
    }

    #[test]
    fn crc_matches_the_png_check_value() {
        assert_eq!(super::crc32(b"IEND"), 0xae42_6082);
    }

    /// R16G16B16A16_UNORM is integers, not half floats; 89 is 10:10:10:2,
    /// not B8G8R8A8.
    #[test]
    fn unorm16_and_xr_bias_targets_decode_as_their_formats() {
        // Red 0x8000 (a half-float -0), green 0x4000, blue 0.
        let unorm16 = [0x00, 0x80, 0x00, 0x40, 0, 0, 0xff, 0xff];
        assert_eq!(super::to_rgba(11, 1, 1, 8, &unorm16), Some(vec![255, 127, 0, 255]));
        // Red 0x3ff, green 0x1ff, blue 0, alpha 3.
        let xr = (0x3ffu32 | 0x1ff << 10 | 3 << 30).to_le_bytes();
        assert_eq!(super::to_rgba(89, 1, 1, 4, &xr), Some(vec![255, 127, 0, 255]));
        // B8G8R8A8 still swaps to RGBA.
        assert_eq!(super::to_rgba(87, 1, 1, 4, &[1, 2, 3, 4]), Some(vec![3, 2, 1, 255]));
    }
}
