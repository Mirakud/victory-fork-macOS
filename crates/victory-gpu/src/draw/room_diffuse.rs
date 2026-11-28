//! Diffuse transfer from the measured ceiling fixtures of the prison cell.
#[path = "room_frustum.rs"]
mod room_frustum;
use super::{State, View};
use crate::host::HostList;
use crate::object::This;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    LazyLock, OnceLock,
};
use windows::core::Result;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;

static MARKER: LazyLock<Option<std::path::PathBuf>> =
    LazyLock::new(|| std::env::var_os("VICTORY_ROOM_DIFFUSE_FILE").map(Into::into));
// Each record is independent of the camera. Half axes are in game world units.
// Their cross product points towards the emitting side. Each light is limited
// to the measured room AABB so shared stencil classes cannot illuminate another room.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct WorldEmitter {
    center: [f32; 4],
    half_u: [f32; 4],
    half_v: [f32; 4],
    radiance: [f32; 4],
    room_min: [f32; 4],
    room_max: [f32; 4],
}

fn parse_emitters(text: &str) -> Option<Vec<WorldEmitter>> {
    let mut result = Vec::new();
    for line in text
        .lines()
        .map(|s| s.split('#').next().unwrap().trim())
        .filter(|s| !s.is_empty())
    {
        if result.len() >= 16 {
            return None;
        }
        let mut fields = line.split_whitespace();
        let mut vector = || {
            let mut xyz = [0.0; 4];
            for value in &mut xyz[..3] {
                *value = fields
                    .next()?
                    .parse::<f32>()
                    .ok()
                    .filter(|v| v.is_finite())?;
            }
            Some(xyz)
        };
        let e = WorldEmitter {
            center: vector()?,
            half_u: vector()?,
            half_v: vector()?,
            radiance: vector()?,
            room_min: vector()?,
            room_max: vector()?,
        };
        let u = e.half_u;
        let v = e.half_v;
        let cross = [
            u[1] * v[2] - u[2] * v[1],
            u[2] * v[0] - u[0] * v[2],
            u[0] * v[1] - u[1] * v[0],
        ];
        let area = cross.iter().map(|n| n * n).sum::<f32>();
        if fields.next().is_some()
            || !area.is_finite()
            || area < 1e-10
            || (0..3).any(|i| e.radiance[i] < 0.0 || e.room_min[i] >= e.room_max[i])
        {
            return None;
        }
        result.push(e);
    }
    (!result.is_empty()).then_some(result)
}

static WORLD_EMITTERS: LazyLock<Vec<WorldEmitter>> = LazyLock::new(|| {
    let Some(path) = std::env::var_os("VICTORY_ROOM_EMITTERS_FILE") else {
        return Vec::new();
    };
    let data = std::fs::read_to_string(&path).ok();
    match data.as_deref().and_then(parse_emitters) {
        Some(sources) => {
            tracing::info!(count = sources.len(), "Using calibrated world emitters");
            sources
        }
        None => {
            tracing::warn!("Calibrated world emitters unavailable");
            Vec::new()
        }
    }
});

static FRAME: AtomicU64 = AtomicU64::new(0);
static PIPELINE: OnceLock<Option<(ID3D12RootSignature, ID3D12PipelineState)>> = OnceLock::new();

fn pipeline(
    device: &ID3D12Device,
    bytes: &[u8],
) -> Result<(ID3D12RootSignature, ID3D12PipelineState)> {
    // SAFETY: the compiled shader includes its own matching root signature.
    unsafe {
        let root: ID3D12RootSignature = device.CreateRootSignature(0, bytes)?;
        let pso = device.CreateComputePipelineState(&D3D12_COMPUTE_PIPELINE_STATE_DESC {
            pRootSignature: std::mem::transmute_copy(&root),
            CS: D3D12_SHADER_BYTECODE {
                pShaderBytecode: bytes.as_ptr().cast(),
                BytecodeLength: bytes.len(),
            },
            ..Default::default()
        })?;
        Ok((root, pso))
    }
}

fn camera(s: &State, emitters: &[WorldEmitter]) -> Option<[f32; 20]> {
    let View::Constant(addr, size) = s.table_view(false, 1, 64)? else {
        return None;
    };
    if addr == 0 || size < 208 {
        return None;
    }
    let mut data = [0u8; 208];
    if !crate::host::read_into(addr as *const u8, &mut data) {
        return None;
    }
    let v = |i: usize| f32::from_le_bytes(data[4 * i..4 * i + 4].try_into().unwrap());
    let constants = [
        v(0),
        v(1),
        v(2),
        0.,
        v(4),
        v(5),
        v(6),
        0.,
        v(8),
        v(9),
        v(10),
        0.,
        v(16),
        v(21),
        v(26),
        v(30),
        -v(48),
        -v(49),
        -v(50),
        0.0,
    ];
    if !(constants[12] > 0. && constants[13] > 0. && constants[15] > 0.) {
        return None;
    }
    let projection = std::array::from_fn(|i| v(16 + i));
    let frustum = room_frustum::ReceiverFrustum::new(&constants, &projection)?;
    emitters
        .iter()
        .any(|e| frustum.sees(&e.room_min, &e.room_max))
        .then_some(constants)
}

fn material(s: &State, offset: u32) -> Option<(usize, i32)> {
    let View::Shader(id, desc) = s.table_view(false, 0, offset)? else {
        return None;
    };
    let format = desc
        .map(|d| d[0] as i32)
        .filter(|&f| matches!(f, 28 | 29))
        .unwrap_or(28);
    (id != 0).then_some((id, format))
}

fn set_state(list: &HostList, id: usize, state: D3D12_RESOURCE_STATES) -> Option<u32> {
    let (texture, old) = list.transition(id as This, state.0 as u32)?;
    // SAFETY: this is the exact source state on the open list; restored below.
    unsafe {
        list.list.ResourceBarrier(&[crate::host::transition(
            texture,
            D3D12_RESOURCE_STATES(old as i32),
            state,
        )])
    };
    Some(old)
}

fn view(format: i32, plane: u32) -> D3D12_SHADER_RESOURCE_VIEW_DESC {
    D3D12_SHADER_RESOURCE_VIEW_DESC {
        Format: DXGI_FORMAT(format),
        ViewDimension: D3D12_SRV_DIMENSION_TEXTURE2D,
        Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
        Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
            Texture2D: D3D12_TEX2D_SRV {
                MipLevels: 1,
                PlaneSlice: plane,
                ..Default::default()
            },
        },
    }
}

fn output(device: &ID3D12Device, mut desc: D3D12_RESOURCE_DESC) -> Option<ID3D12Resource> {
    desc.Flags = D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS;
    let heap = D3D12_HEAP_PROPERTIES {
        Type: D3D12_HEAP_TYPE_DEFAULT,
        ..Default::default()
    };
    let mut texture = None;
    // SAFETY: a private target with exactly the original texture's dimensions.
    unsafe {
        device
            .CreateCommittedResource(
                &heap,
                D3D12_HEAP_FLAG_NONE,
                &desc,
                D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                None,
                &mut texture,
            )
            .ok()?;
    }
    texture
}

pub(super) fn after_light(s: &mut State, list: &HostList) {
    let Some(marker) = MARKER.as_ref() else {
        return;
    };
    let frame = crate::queue::presented() + 1;
    if FRAME.load(Ordering::Relaxed) >= frame
        || !crate::pipeline::uses_shader_stage(s.pipeline as This, "pixel", "c94c4c7b5775cc2b")
        || !crate::pipeline::is_prison_light_volume(s.pipeline as This)
        || !marker.is_file()
    {
        return;
    }
    let emitters = WORLD_EMITTERS.as_slice();
    if emitters.is_empty() {
        return;
    }
    let Some(constants) = camera(s, emitters) else {
        return;
    };
    let [scene] = s.targets.as_slice() else {
        return;
    };
    let Some((albedo, af)) = material(s, 584) else {
        return;
    };
    let Some((normal, nf)) = material(s, 1168) else {
        return;
    };
    let ids = [*scene, albedo, normal, s.depth];
    if FRAME.fetch_max(frame, Ordering::Relaxed) >= frame {
        return;
    }
    let mut sample = if crate::capture::lights::enabled() && crate::capture::target_frame(frame) {
        crate::resource::host_made(ids[0] as This).and_then(|texture| {
            crate::capture::lights::Sample::snapshot(
                list,
                frame,
                format!("room-diffuse-f{frame}-rtv{:x}", ids[0]),
                texture,
                ids[3] as This,
                8,
            )
        })
    } else {
        None
    };
    if let Some(probe) = sample.as_mut() {
        probe.room_inputs(list, ids[1] as This, ids[2] as This, ids[3] as This);
        if let Some(View::Constant(addr, size)) = s.table_view(false, 1, 64) {
            probe.input_buffer("camera-s1-64", addr, u64::from(size));
        }
        if let Some((addr, size)) = s
            .table_view(false, 1, 8)
            .as_ref()
            .and_then(crate::descriptor::host::buffer_range)
        {
            probe.input_buffer("spots-s1-8", addr, size);
        }
    }
    if record(list, ids, af, nf, constants, emitters) {
        if let Some(mut sample) = sample.take() {
            sample.after(list, false);
            list.note_light_sample(sample);
        }
        s.replay(list);
        list.note_op(|| {
            format!(
                "room diffuse frame {frame}, world emitters {}",
                emitters.len()
            )
        });
    }
}

fn record(
    list: &HostList,
    ids: [usize; 4],
    af: i32,
    nf: i32,
    mut constants: [f32; 20],
    emitters: &[WorldEmitter],
) -> bool {
    let [Some(colour), Some(mat), Some(norm), Some(depth)] =
        ids.map(|id| crate::resource::host_made(id as This))
    else {
        return false;
    };
    // SAFETY: these are the four live, already prepared surfaces of this draw.
    let [c, m, n, d] = [colour, mat, norm, depth].map(|t| unsafe { t.GetDesc() });
    if c.Format.0 != 10
        || !matches!(m.Format.0, 27..=29)
        || !matches!(n.Format.0, 27..=29)
        || !matches!(d.Format.0, 19..=22)
        || [c, m, n, d].iter().any(|q| {
            q.Dimension != D3D12_RESOURCE_DIMENSION_TEXTURE2D
                || q.Width != 1280
                || q.Height != 720
                || q.SampleDesc.Count != 1
                || q.DepthOrArraySize != 1
                || q.MipLevels != 1
        })
    {
        return false;
    }
    let Some(host) = crate::host::host() else {
        return false;
    };
    let Some((root, pso)) = PIPELINE.get_or_init(|| {
        pipeline(&host.device, include_bytes!("room_world.dxil"))
            .map_err(|e| tracing::warn!(%e,"room diffuse pipeline unavailable"))
            .ok()
    }) else {
        return false;
    };
    let Some(out) = output(&host.device, c) else {
        return false;
    };
    let byte_len = std::mem::size_of_val(emitters);
    let Some((world_address, dst)) = list.scratch(byte_len as u64) else {
        return false;
    };
    // SAFETY: WorldEmitter contains six float4s. This list owns the complete upload.
    unsafe {
        std::ptr::copy_nonoverlapping(emitters.as_ptr().cast::<u8>(), dst, byte_len);
    }
    constants[19] = emitters.len() as f32;
    // Immutable descriptors and private output belong to this recording until
    // reset; concurrent and resubmitted lists cannot overwrite another draw.
    let heap: ID3D12DescriptorHeap = match unsafe {
        host.device
            .CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                Type: D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
                NumDescriptors: 6,
                Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
                NodeMask: 0,
            })
    } {
        Ok(heap) => heap,
        Err(_) => return false,
    };

    // SAFETY: valid typed plane SRVs, including the stencil at component G;
    // every object named by the recorded list is retained until its reset.
    unsafe {
        let start = heap.GetCPUDescriptorHandleForHeapStart().ptr;
        let step = host
            .device
            .GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV)
            as usize;
        for (i, (src, fmt, plane)) in [
            (colour, 10, 0),
            (mat, af, 0),
            (norm, nf, 0),
            (depth, 21, 0),
            (depth, 22, 1),
        ]
        .into_iter()
        .enumerate()
        {
            host.device.CreateShaderResourceView(
                src,
                Some(&view(fmt, plane)),
                D3D12_CPU_DESCRIPTOR_HANDLE {
                    ptr: start + i * step,
                },
            );
        }
        host.device.CreateUnorderedAccessView(
            &out,
            None::<&ID3D12Resource>,
            None,
            D3D12_CPU_DESCRIPTOR_HANDLE {
                ptr: start + 5 * step,
            },
        );
        let old = ids.map(|id| set_state(list, id, D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE));
        list.keep(&heap);
        list.keep(&out);
        list.keep(root);
        list.keep(pso);
        list.list.SetDescriptorHeaps(&[Some(heap.clone())]);
        list.list.SetComputeRootSignature(root);
        list.list.SetPipelineState(pso);
        list.list
            .SetComputeRootDescriptorTable(0, heap.GetGPUDescriptorHandleForHeapStart());
        list.list
            .SetComputeRoot32BitConstants(1, 20, constants.as_ptr().cast(), 0);
        list.list.SetComputeRootShaderResourceView(2, world_address);
        list.list.Dispatch(80, 45, 1);
        list.list.ResourceBarrier(&[crate::host::transition(
            &out,
            D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
            D3D12_RESOURCE_STATE_COPY_SOURCE,
        )]);
        set_state(list, ids[0], D3D12_RESOURCE_STATE_COPY_DEST);
        list.list.CopyResource(colour, &out);
        for (id, prior) in ids.into_iter().zip(old) {
            if let Some(state) = prior {
                crate::list::ensure_state(list, id as This, state);
            }
        }
        // Even if the colour began in NON_PIXEL_SHADER_RESOURCE, it temporarily
        // became COPY_DEST. The original indexed draw left it in RENDER_TARGET.
        crate::list::ensure_state(
            list,
            ids[0] as This,
            D3D12_RESOURCE_STATE_RENDER_TARGET.0 as u32,
        );
    }
    true
}
