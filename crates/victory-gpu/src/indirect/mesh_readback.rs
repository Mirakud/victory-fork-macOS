//! Compile the synthetic fixture outside the repo and point `VICTORY_MESH_READBACK_FIXTURES` there.
//! `dxc -T ms_6_5 -E mesh_main -Fo mesh.dxil mesh_readback.hlsl`
//! `dxc -T ps_6_5 -E pixel_main -Fo pixel.dxil mesh_readback.hlsl`

use std::path::PathBuf;

use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_R8G8B8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::core::Interface;

use crate::capture::MeshOcclusion;

use super::*;

#[repr(C, align(8))]
struct Subobject<T>(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE, T);

#[repr(C)]
struct MeshStream {
    root: Subobject<*mut std::ffi::c_void>,
    mesh: Subobject<D3D12_SHADER_BYTECODE>,
    pixel: Subobject<D3D12_SHADER_BYTECODE>,
    targets: Subobject<D3D12_RT_FORMAT_ARRAY>,
    raster: Subobject<D3D12_RASTERIZER_DESC>,
    depth: Subobject<D3D12_DEPTH_STENCIL_DESC>,
    topology: Subobject<D3D12_PRIMITIVE_TOPOLOGY_TYPE>,
}

fn pipeline(
    device: &ID3D12Device,
    root: &ID3D12RootSignature,
    mesh: &[u8],
    pixel: &[u8],
) -> ID3D12PipelineState {
    let code = |bytes: &[u8]| D3D12_SHADER_BYTECODE {
        pShaderBytecode: bytes.as_ptr().cast(),
        BytecodeLength: bytes.len(),
    };
    let mut targets = D3D12_RT_FORMAT_ARRAY {
        NumRenderTargets: 1,
        ..Default::default()
    };
    targets.RTFormats[0] = DXGI_FORMAT_R8G8B8A8_UNORM;
    let mut stream = MeshStream {
        root: Subobject(
            D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_ROOT_SIGNATURE,
            root.as_raw(),
        ),
        mesh: Subobject(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_MS, code(mesh)),
        pixel: Subobject(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_PS, code(pixel)),
        targets: Subobject(
            D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_RENDER_TARGET_FORMATS,
            targets,
        ),
        raster: Subobject(
            D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_RASTERIZER,
            D3D12_RASTERIZER_DESC {
                FillMode: D3D12_FILL_MODE_SOLID,
                CullMode: D3D12_CULL_MODE_NONE,
                DepthClipEnable: true.into(),
                ..Default::default()
            },
        ),
        depth: Subobject(
            D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_DEPTH_STENCIL,
            D3D12_DEPTH_STENCIL_DESC {
                DepthFunc: D3D12_COMPARISON_FUNC_ALWAYS,
                ..Default::default()
            },
        ),
        topology: Subobject(
            D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_PRIMITIVE_TOPOLOGY,
            D3D12_PRIMITIVE_TOPOLOGY_TYPE_TRIANGLE,
        ),
    };
    let device = device.cast::<ID3D12Device2>().unwrap();
    // SAFETY: the stream is suitably aligned and references live DXIL and root signature data.
    unsafe {
        device.CreatePipelineState(&D3D12_PIPELINE_STATE_STREAM_DESC {
            SizeInBytes: size_of::<MeshStream>(),
            pPipelineStateSubobjectStream: std::ptr::from_mut(&mut stream).cast(),
        })
    }
    .expect("host accepts a synthetic SM 6.5 mesh pipeline")
}

#[test]
#[ignore = "requires synthetic DXIL and a mesh-capable D3D12 device"]
fn bounded_indirect_mesh_draws_the_expected_pixels() {
    bounded_mesh_pixels(false, false);
}

#[test]
#[ignore = "requires synthetic DXIL and a mesh-capable D3D12 device"]
fn fenced_mesh_scratch_keeps_its_arguments_and_pixels() {
    bounded_mesh_pixels(true, false);
}

#[test]
#[ignore = "requires synthetic DXIL and a mesh-capable D3D12 device"]
fn fenced_mesh_binary_occlusion_matches_scissor() {
    bounded_mesh_pixels(true, true);
}

fn bounded_mesh_pixels(split: bool, calibrate_occlusion: bool) {
    let fixtures = PathBuf::from(
        std::env::var_os("VICTORY_MESH_READBACK_FIXTURES").expect("synthetic DXIL directory"),
    );
    let mesh = std::fs::read(fixtures.join("mesh.dxil")).expect("synthetic mesh fixture");
    let pixel = std::fs::read(fixtures.join("pixel.dxil")).expect("synthetic pixel fixture");
    let host = host().expect("host D3D12 device");
    assert!(host.mesh_shaders(), "host must report mesh shader support");
    let _ = host.log_messages();
    let occlusion = calibrate_occlusion.then(|| {
        (
            MeshOcclusion::new(&host.device)
                .expect("host binary occlusion query for the visible draw"),
            MeshOcclusion::new(&host.device)
                .expect("host binary occlusion query for the excluded draw"),
        )
    });

    const SIZE: u32 = 16;
    const STRIDE: u32 = 12;
    let commands = host.new_list().unwrap();
    let producer = split.then(|| host.new_list().unwrap());
    let clamp = producer.as_ref().unwrap_or(&commands);
    let list = &commands.list;
    let (source, data) = host.shared_buffer(256).expect("mapped source arguments");
    // SAFETY: the mapping is writable and spans these three dispatch fields.
    unsafe { data.cast::<[u32; 3]>().write_unaligned([4, 1, 1]) };
    let (read_args, read_args_ptr) = host.shared_buffer(256).expect("mapped bounded arguments");
    let mesh_kind = D3D12_INDIRECT_ARGUMENT_TYPE_DISPATCH_MESH.0 as u32;
    let args =
        bound(clamp, (&source, 0), None, (mesh_kind, STRIDE, 1)).expect("bound mesh arguments");
    assert_eq!(
        clamp.indirect_state(&args),
        D3D12_RESOURCE_STATE_INDIRECT_ARGUMENT
    );
    let copy_args = |list: &ID3D12GraphicsCommandList| {
        // SAFETY: an open list, live buffers and a full 12-byte record.
        unsafe {
            list.ResourceBarrier(&[transition(&args, D3D12_RESOURCE_STATE_INDIRECT_ARGUMENT, D3D12_RESOURCE_STATE_COPY_SOURCE)]);
            list.CopyBufferRegion(&read_args, 0, &args, 0, u64::from(STRIDE));
            list.ResourceBarrier(&[transition(&args, D3D12_RESOURCE_STATE_COPY_SOURCE, D3D12_RESOURCE_STATE_INDIRECT_ARGUMENT)]);
        }
    };
    if let Some(producer) = &producer {
        copy_args(&producer.list);
        producer.close();
    }

    // SAFETY: all descriptors describe live resources and fit the command list.
    let (readback, footprint) = unsafe {
        let mut blob: Option<ID3DBlob> = None;
        D3D12SerializeRootSignature(
            &D3D12_ROOT_SIGNATURE_DESC::default(),
            D3D_ROOT_SIGNATURE_VERSION_1,
            &mut blob,
            None,
        )
        .unwrap();
        let blob = blob.unwrap();
        let code = std::slice::from_raw_parts(blob.GetBufferPointer().cast(), blob.GetBufferSize());
        let root: ID3D12RootSignature = host.device.CreateRootSignature(0, code).unwrap();
        let pso = pipeline(&host.device, &root, &mesh, &pixel);
        let argument = D3D12_INDIRECT_ARGUMENT_DESC {
            Type: D3D12_INDIRECT_ARGUMENT_TYPE_DISPATCH_MESH,
            ..Default::default()
        };
        let mut signature: Option<ID3D12CommandSignature> = None;
        host.device
            .CreateCommandSignature(
                &D3D12_COMMAND_SIGNATURE_DESC {
                    ByteStride: STRIDE,
                    NumArgumentDescs: 1,
                    pArgumentDescs: &argument,
                    NodeMask: 0,
                },
                None::<&ID3D12RootSignature>,
                &mut signature,
            )
            .expect("host mesh command signature");
        let signature = signature.unwrap();
        let mut texture: Option<ID3D12Resource> = None;
        host.device
            .CreateCommittedResource(
                &D3D12_HEAP_PROPERTIES {
                    Type: D3D12_HEAP_TYPE_DEFAULT,
                    ..Default::default()
                },
                D3D12_HEAP_FLAG_NONE,
                &D3D12_RESOURCE_DESC {
                    Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
                    Width: u64::from(SIZE),
                    Height: SIZE,
                    DepthOrArraySize: 1,
                    MipLevels: 1,
                    Format: DXGI_FORMAT_R8G8B8A8_UNORM,
                    SampleDesc: DXGI_SAMPLE_DESC {
                        Count: 1,
                        Quality: 0,
                    },
                    Flags: D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET,
                    ..Default::default()
                },
                D3D12_RESOURCE_STATE_RENDER_TARGET,
                None,
                &mut texture,
            )
            .unwrap();
        let texture = texture.unwrap();
        let view = host.rtv(Some(&texture), None);
        list.ClearRenderTargetView(view, &[0.0, 0.0, 0.0, 1.0], None);
        list.OMSetRenderTargets(1, Some(&view), false, None);
        list.RSSetViewports(&[D3D12_VIEWPORT {
            Width: SIZE as f32,
            Height: SIZE as f32,
            MaxDepth: 1.0,
            ..Default::default()
        }]);
        list.RSSetScissorRects(&[RECT {
            left: 0,
            top: 0,
            right: SIZE as i32,
            bottom: SIZE as i32,
        }]);
        list.SetGraphicsRootSignature(&root);
        list.SetPipelineState(&pso);
        if split {
            list.ResourceBarrier(&[transition(&args, D3D12_RESOURCE_STATE_COMMON, D3D12_RESOURCE_STATE_INDIRECT_ARGUMENT)]);
        }
        if let Some((visible, _)) = &occlusion {
            visible.begin(list);
        }
        list.ExecuteIndirect(&signature, 1, &args, 0, None::<&ID3D12Resource>, 0);
        if let Some((visible, excluded)) = &occlusion {
            visible.end(list);
            // This nonempty scissor covers the black pixel checked below, outside the triangle.
            list.RSSetScissorRects(&[RECT {
                left: 14,
                top: 14,
                right: 15,
                bottom: 15,
            }]);
            excluded.begin(list);
            list.ExecuteIndirect(&signature, 1, &args, 0, None::<&ID3D12Resource>, 0);
            excluded.end(list);
        }
        if split {
            list.ResourceBarrier(&[transition(&args, D3D12_RESOURCE_STATE_INDIRECT_ARGUMENT, D3D12_RESOURCE_STATE_COMMON)]);
        } else {
            copy_args(list);
        }
        list.ResourceBarrier(&[transition(&texture, D3D12_RESOURCE_STATE_RENDER_TARGET, D3D12_RESOURCE_STATE_COPY_SOURCE)]);
        let read = crate::capture::read_back(&host.device, list, &texture).expect("RGBA8 readback");
        commands.close();
        if let Some(producer) = &producer {
            host.execute(&[producer, &commands]);
        } else {
            host.execute(&[&commands]);
        }
        read
    };
    assert_eq!(
        host.log_messages(),
        None,
        "D3D12 accepted the mesh submission"
    );
    if let Some((visible, excluded)) = occlusion {
        let visible = visible.read().expect("resolve visible mesh occlusion");
        let excluded = excluded.read().expect("resolve scissored mesh occlusion");
        eprintln!("D3D12 binary mesh occlusion: visible={visible} outside_scissor={excluded}");
        assert_eq!(
            (visible, excluded),
            (1, 0),
            "visible and scissored mesh occlusion"
        );
    }

    let limit = mesh_group_limit_override();
    let expected = if limit == 0 { 4 } else { limit.min(4) };
    // SAFETY: execution has completed and both maps cover the reads below.
    unsafe {
        assert_eq!(read_args_ptr.cast::<[u32; 3]>().read_unaligned(), [expected, 1, 1]);
        let mut mapped = std::ptr::null_mut();
        readback.Map(0, None, Some(&mut mapped)).unwrap();
        let pitch = footprint.Footprint.RowPitch as usize;
        let pixels = std::slice::from_raw_parts(mapped.cast::<u8>(), pitch * SIZE as usize);
        let pixel = |x: usize, y: usize| &pixels[y * pitch + x * 4..][..4];
        assert_eq!(pixel(4, 4), [0, 255, 0, 255], "inside mesh triangle");
        assert_eq!(pixel(14, 14), [0, 0, 0, 255], "outside mesh triangle");
        readback.Unmap(0, None);
    }
    eprintln!("D3D12 returned {expected} bounded mesh groups, green inside the triangle and black outside");
}
