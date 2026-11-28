use super::*;
use crate::host::HostList;
use crate::object::This;
use std::sync::atomic::Ordering::Relaxed;

const HASHES: [&str; 4] = ["c94c4c7b5775cc2b", "55e52a40b5d79b20", "a885ba5d98ce46bf", "1407289e381df336"];
static LAST: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];
static ENABLED: LazyLock<bool> = LazyLock::new(|| std::env::var_os("VICTORY_CAPTURE_TARGET_COMMANDS").is_some_and(|s| s == "1"));

pub(crate) fn enabled() -> bool { *ENABLED }

type Read = (ID3D12Resource, D3D12_PLACED_SUBRESOURCE_FOOTPRINT);

/// One colour copy before and after the original light, plus the optional
/// room replay. All GPU commands go into that very same list.
pub(crate) struct Sample {
    stem: String,
    frame: u64,
    reference: u32,
    depth: usize,
    texture: ID3D12Resource,
    before: Read,
    original: Option<Read>,
    replay: Option<Read>,
    stencil: Option<Read>,
    room_inputs: Vec<(&'static str, Read, usize)>,
    room_constants: Vec<(&'static str, Vec<u8>)>,
}

impl Sample {
    pub(crate) fn begin(list: &HostList, frame: u64, pipeline: This, prefixes: &[String], slots: &[usize], depth: This, reference: u32) -> Option<Self> {
        let [colour] = slots else { return None };
        if !crate::pipeline::is_prison_light_volume(pipeline) { return None; }
        let pass = crate::pipeline::host_pipeline(pipeline)?;
        let (idx, &hash) = HASHES.iter().enumerate().find(|(_, hash)| {
            prefixes.iter().any(|p| hash.starts_with(p)) && pass.stages.iter().any(|s| s.shader.hash == **hash)
        })?;
        let texture = crate::resource::host_made(*colour as This)?;
        // SAFETY: the completed draw preparation retains its colour target.
        let desc = unsafe { texture.GetDesc() };
        if desc.Format.0 != 10 || desc.SampleDesc.Count != 1 || desc.DepthOrArraySize != 1
            || desc.MipLevels != 1 || (desc.Width, desc.Height) != (1280, 720)
            || !list.tracks_render_target(&texture) || LAST[idx].fetch_max(frame, Relaxed) >= frame {
            return None;
        }
        Self::snapshot(list, frame, format!("light-f{frame}-{hash}-rtv{colour:x}"), texture, depth, reference)
    }

    /// Copy the true colour and stencil at a known recorded light operation.
    pub(crate) fn snapshot(list: &HostList, frame: u64, stem: String, texture: &ID3D12Resource, depth: This, reference: u32) -> Option<Self> {
        let before = read_render_target(list, texture)?;
        let stencil = Self::read_stencil(list, depth);
        // A closed command list can be submitted again after the diagnostic was
        // saved. D3D12 needs the copy destinations to live until its reset.
        list.keep(&before.0);
        if let Some((buffer, _)) = &stencil { list.keep(buffer); }
        Some(Self { stem, frame, reference, depth: depth as usize,
            texture: texture.clone(), before, original: None, replay: None, stencil, room_inputs: Vec::new(), room_constants: Vec::new() })
    }

    /// Capture the actual material, normal and reverse-Z input of this light.
    /// Only the explicitly selected room draw calls this, after its fence the
    /// raw rows can be reprojected with the camera constants of the same draw.
    pub(crate) fn room_inputs(&mut self, list: &HostList, mat: This, normal: This, depth: This) {
        let Some(host) = crate::host::host() else { return };
        for (name, id) in [("material", mat), ("normal", normal), ("depth", depth)] {
            let Some(texture) = crate::resource::host_made(id) else { continue };
            let desc = unsafe { texture.GetDesc() };
            if desc.Width != 1280 || desc.Height != 720 || desc.DepthOrArraySize != 1
                || desc.MipLevels != 1 || desc.SampleDesc.Count != 1 { continue; }
            let mut row = 0;
            // SAFETY: subresource zero is present and row is a live output.
            unsafe { host.device.GetCopyableFootprints(&desc, 0, 1, 0, None, None, Some(&mut row), None) };
            if row % desc.Width != 0 || !(1..=16).contains(&(row / desc.Width)) { continue; }
            let copy = D3D12_RESOURCE_STATE_COPY_SOURCE;
            let Some((texture, old)) = list.transition(id, copy.0 as u32) else { continue };
            unsafe { list.list.ResourceBarrier(&[crate::host::transition(texture, D3D12_RESOURCE_STATES(old as i32), copy)]) };
            let read = read_back_of(&host.device, &list.list, texture, 0);
            crate::list::ensure_state(list, id, old);
            if let Some(read) = read {
                list.keep(&read.0);
                self.room_inputs.push((name, read, (row / desc.Width) as usize));
            }
        }
    }

    pub(crate) fn input_buffer(&mut self, name: &'static str, addr: u64, size: u64) {
        if addr == 0 || !(1..=1048576).contains(&size) { return; }
        let mut bytes = vec![0; size as usize];
        if crate::host::read_into(addr as *const u8, &mut bytes) {
            self.room_constants.push((name, bytes));
        }
    }

    fn save_raw(&self, name: &str, read: &Read, stride: usize) {
        let Some(folder) = folder() else { return };
        Self::rows(read, stride, |rows,w,h,pitch| {
            if pitch < w * stride { return; }
            let mut data = Vec::with_capacity(w*h*stride);
            for y in 0..h { data.extend_from_slice(&rows[y*pitch..y*pitch+w*stride]); }
            let fmt = read.1.Footprint.Format.0;
            let file = folder.join(format!("{}-{name}-{w}x{h}-fmt{fmt}-bpp{stride}.bin", self.stem));
            if let Err(error) = std::fs::write(file, data) { tracing::warn!(%error, "room input not saved"); }
        });
    }

    fn read_stencil(list: &HostList, depth: This) -> Option<Read> {
        let host = crate::host::host()?;
        let texture = crate::resource::host_made(depth)?;
        // SAFETY: draw preparation bound this live single-slice DSV. Its actual
        // previous state can include combined shader-read flags.
        let desc = unsafe { texture.GetDesc() };
        if !matches!(desc.Format.0, 19..=22 | 44..=47) || desc.SampleDesc.Count != 1
            || desc.DepthOrArraySize != 1 || desc.MipLevels != 1 { return None; }
        let copy = D3D12_RESOURCE_STATE_COPY_SOURCE;
        let (texture, before) = list.transition(depth, copy.0 as u32)?;
        // SAFETY: this list's tracked prior state is the exact state being left.
        unsafe { list.list.ResourceBarrier(&[crate::host::transition(texture, D3D12_RESOURCE_STATES(before as i32), copy)]) };
        let read = read_back_of(&host.device, &list.list, texture, 1);
        crate::list::ensure_state(list, depth, before);
        read
    }

    pub(crate) fn after(&mut self, list: &HostList, replay: bool) {
        let r = read_render_target(list, &self.texture);
        if replay { self.replay = r } else { self.original = r }
    }

    fn rows<T>(sample: &Read, bytes: usize, inspect: impl FnOnce(&[u8], usize, usize, usize) -> T) -> Option<T> {
        let (buf, footprint) = sample;
        let f = footprint.Footprint;
        if footprint.Offset != 0 || f.Height == 0 || f.Width == 0 { return None; }
        let (w, h, pitch) = (f.Width as usize, f.Height as usize, f.RowPitch as usize);
        let len = (h - 1) * pitch + w * bytes;
        with_readback_rows(buf, len, |rows| inspect(rows, w, h, pitch))
    }

    /// Called by read_backs after the same submission's fence, for sync and
    /// asynchronous lists. PFM luminance uses the same sampling grid as PNG.
    pub(crate) fn landed(self) {
        if !self.room_inputs.is_empty() {
            self.save_raw("emission", &self.before, 8);
            if let Some(folder) = folder() {
                for (name, bytes) in &self.room_constants {
                    if let Err(error) = std::fs::write(folder.join(format!("{}-{name}.bin", self.stem)), bytes) {
                        tracing::warn!(%error, "room buffer not saved");
                    }
                }
            }
            for (name, plane, stride) in &self.room_inputs { self.save_raw(name, plane, *stride); }
        }
        if let Some(stencil) = &self.stencil {
            stencil_values(&stencil.0, &stencil.1, self.frame, self.depth as This);
            Self::rows(stencil, 1, |rows, w, h, pitch| {
                let Some(folder) = folder() else { return };
                let mut pgm = format!("P5\n{w} {h}\n255\n").into_bytes();
                for y in 0..h { pgm.extend_from_slice(&rows[y * pitch..y * pitch + w]); }
                if let Err(error) = std::fs::write(folder.join(format!("{}-stencil.pgm", self.stem)), pgm) {
                    tracing::warn!(%error, "light draw stencil not saved");
                }
            });
        }
        let record = |part: &str, sample: &Read, previous: Option<&Read>, reference| {
            let name = format!("{}-{part}", self.stem);
            Self::rows(sample, 8, |rows, w, h, pitch| {
                if let Some(rgba) = to_rgba(10, w, h, pitch, rows) {
                    save_as(&name, w as u32, h as u32, w * 4, false, &rgba);
                }
                save_luminance(&name, 10, w, h, pitch, rows);
                let changed_rgb = previous.and_then(|a| Self::rows(a, 8, |old, _, _, old_pitch| {
                    (0..h).map(|y| {
                        let count = w * 8;
                        old[y * old_pitch..y * old_pitch + count].chunks_exact(8)
                            .zip(rows[y * pitch..y * pitch + count].chunks_exact(8))
                            .filter(|(a, b)| a[..6] != b[..6]).count()
                    }).sum::<usize>()
                }));
                tracing::info!(%name, reference, ?changed_rgb, "light draw HDR sample landed");
            });
        };
        record("before", &self.before, None, self.reference);
        if let Some(after) = &self.original { record("original", after, Some(&self.before), self.reference); }
        if let Some(after) = &self.replay { record("room", after, self.original.as_ref(), 8); }
    }
}
