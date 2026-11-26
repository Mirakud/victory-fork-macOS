//! Read back both planes after the same draw used to restore a depth twin.

use super::*;

const STEPS: [u16; 4] = [0, 385, 32768, 65535];
const FLOATS: [f32; 4] = [0.125, 0.375, 0.625, 0.875];
const STENCIL: u8 = 0x5a;

fn texture(
    device: &ID3D12Device,
    format: DXGI_FORMAT,
    flags: D3D12_RESOURCE_FLAGS,
    state: D3D12_RESOURCE_STATES,
) -> ID3D12Resource {
    let mut resource = None;
    // SAFETY: all descriptors and the output live through the call.
    unsafe {
        device
            .CreateCommittedResource(
                &D3D12_HEAP_PROPERTIES {
                    Type: D3D12_HEAP_TYPE_DEFAULT,
                    ..Default::default()
                },
                D3D12_HEAP_FLAG_NONE,
                &D3D12_RESOURCE_DESC {
                    Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
                    Width: 2,
                    Height: 2,
                    DepthOrArraySize: 1,
                    MipLevels: 1,
                    Format: format,
                    SampleDesc: DXGI_SAMPLE_DESC {
                        Count: 1,
                        Quality: 0,
                    },
                    Flags: flags,
                    ..Default::default()
                },
                state,
                None,
                &mut resource,
            )
            .unwrap();
    }
    resource.unwrap()
}

fn pixels<const N: usize>(
    (resource, footprint): &(ID3D12Resource, D3D12_PLACED_SUBRESOURCE_FOOTPRINT),
) -> [[u8; N]; 4] {
    let mut mapped = std::ptr::null_mut();
    // SAFETY: the submitted copy finished. Only the four active pixels are read.
    unsafe {
        resource.Map(0, None, Some(&mut mapped)).unwrap();
        let bytes = mapped.cast::<u8>().add(footprint.Offset as usize);
        let pitch = footprint.Footprint.RowPitch as usize;
        let pixels = std::array::from_fn(|i| {
            bytes
                .add(i / 2 * pitch + i % 2 * N)
                .cast::<[u8; N]>()
                .read()
        });
        resource.Unmap(0, None);
        pixels
    }
}

fn run(steps: bool) {
    let host = host().expect("D3D12 adapter required for the plane readback");
    let (input_format, depth_format, view_format) = if steps {
        (
            DXGI_FORMAT_R16_TYPELESS,
            DXGI_FORMAT_R24G8_TYPELESS,
            DXGI_FORMAT_D24_UNORM_S8_UINT,
        )
    } else {
        (
            DXGI_FORMAT_R32_TYPELESS,
            DXGI_FORMAT_R32G8X24_TYPELESS,
            DXGI_FORMAT_D32_FLOAT_S8X24_UINT,
        )
    };
    plane_pass()
        .expect("plane descriptors")
        .pipeline(steps, view_format)
        .expect("plane PSO");
    let input = texture(
        &host.device,
        input_format,
        D3D12_RESOURCE_FLAG_NONE,
        D3D12_RESOURCE_STATE_COPY_DEST,
    );
    let depth = texture(
        &host.device,
        depth_format,
        D3D12_RESOURCE_FLAG_ALLOW_DEPTH_STENCIL,
        D3D12_RESOURCE_STATE_DEPTH_WRITE,
    );
    let (upload, mapped) = host.upload_buffer(512).unwrap();
    let commands = host.new_list().unwrap();
    let list = &commands.list;
    // SAFETY: the synthetic inputs and both outputs stay live through execute.
    let (depth_read, stencil_read) = unsafe {
        for i in 0..4 {
            let row = mapped.add(i / 2 * 256);
            if steps {
                row.cast::<u16>().add(i % 2).write(STEPS[i]);
            } else {
                row.cast::<f32>().add(i % 2).write(FLOATS[i]);
            }
        }
        upload.Unmap(0, None);
        let source = D3D12_TEXTURE_COPY_LOCATION {
            pResource: std::mem::transmute_copy(&upload),
            Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                PlacedFootprint: D3D12_PLACED_SUBRESOURCE_FOOTPRINT {
                    Offset: 0,
                    Footprint: D3D12_SUBRESOURCE_FOOTPRINT {
                        Format: input_format,
                        Width: 2,
                        Height: 2,
                        Depth: 1,
                        RowPitch: 256,
                    },
                },
            },
        };
        let destination = D3D12_TEXTURE_COPY_LOCATION {
            pResource: std::mem::transmute_copy(&input),
            Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                SubresourceIndex: 0,
            },
        };
        list.CopyTextureRegion(&destination, 0, 0, 0, &source, None);
        list.ResourceBarrier(&[transition(
            &input,
            D3D12_RESOURCE_STATE_COPY_DEST,
            D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE,
        )]);
        let view = D3D12_DEPTH_STENCIL_VIEW_DESC {
            Format: view_format,
            ViewDimension: D3D12_DSV_DIMENSION_TEXTURE2D,
            ..Default::default()
        };
        list.ClearDepthStencilView(
            host.dsv(&depth, Some(&view)),
            D3D12_CLEAR_FLAG_DEPTH | D3D12_CLEAR_FLAG_STENCIL,
            0.25,
            STENCIL,
            None,
        );
        record_plane_pass(list, &depth, &input);
        list.ResourceBarrier(&[transition(
            &depth,
            D3D12_RESOURCE_STATE_DEPTH_WRITE,
            D3D12_RESOURCE_STATE_COPY_SOURCE,
        )]);
        (
            crate::capture::read_back_of(&host.device, list, &depth, 0).unwrap(),
            crate::capture::read_back_of(&host.device, list, &depth, 1).unwrap(),
        )
    };
    commands.close();
    host.execute(&[&commands]);
    let depth = pixels::<4>(&depth_read).map(u32::from_le_bytes);
    let stencil = pixels::<1>(&stencil_read).map(|v| v[0]);
    eprintln!(
        "steps={steps} DSV={} depth={depth:08x?} stencil={stencil:02x?} footprints={:?}/{:?}",
        view_format.0, depth_read.1.Footprint.Format, stencil_read.1.Footprint.Format
    );
    if steps {
        for (value, step) in depth.into_iter().zip(STEPS) {
            let normalized = f32::from(step) * f32::from_bits(0x37800080);
            let expected = (f64::from(normalized) * 16_777_215.0).round() as u32;
            assert!(
                (value & 0xff_ffff).abs_diff(expected) <= 1,
                "step {step}: {value:08x}, expected {expected:06x}"
            );
        }
    } else {
        assert_eq!(depth.map(f32::from_bits), FLOATS);
    }
    assert_eq!(stencil, [STENCIL; 4]);
}

#[test]
#[ignore = "requires a D3D12 adapter with D24S8"]
fn depth_steps_reach_d24_and_preserve_stencil() {
    run(true);
}

#[test]
#[ignore = "requires a D3D12 adapter with D32S8"]
fn depth_floats_reach_d32_and_preserve_stencil() {
    run(false);
}

/// Exercises FXC and version-1 root serialization without creating a device.
#[test]
fn depth_twin_shader_and_table_contract() {
    use victory_shader::{Container, FourCC};
    use windows::Win32::Graphics::Direct3D::ID3DBlob;
    use windows::core::{Interface, s};

    for (source, name, profile) in [
        (PLANE_CORNERS, s!("corner"), s!("vs_5_1")),
        (PLANE_STEPS, s!("plane"), s!("ps_5_1")),
        (PLANE_FLOATS, s!("plane"), s!("ps_5_1")),
    ] {
        let bytes = compile(source, name, profile).expect("compile the production shader");
        let container = Container::parse(&bytes).unwrap();
        assert!(
            container
                .parts
                .iter()
                .any(|p| p.fourcc == FourCC(*b"SHEX") || p.fourcc == FourCC(*b"SHDR"))
        );
        assert!(container.parts.iter().all(|p| p.fourcc != FourCC::RTS0));
    }
    let ranges = [crate::over::range(D3D12_DESCRIPTOR_RANGE_TYPE_SRV, 0)];
    let parameter = crate::over::table(&ranges, D3D12_SHADER_VISIBILITY_PIXEL);
    let desc = D3D12_ROOT_SIGNATURE_DESC {
        NumParameters: 1,
        pParameters: &parameter,
        ..Default::default()
    };
    let mut blob: Option<ID3DBlob> = None;
    // SAFETY: the input table and the output COM objects live through their uses.
    unsafe {
        D3D12SerializeRootSignature(&desc, D3D_ROOT_SIGNATURE_VERSION_1, &mut blob, None).unwrap();
        let blob = blob.unwrap();
        let mut raw = std::ptr::null_mut();
        D3D12CreateRootSignatureDeserializer(
            blob.GetBufferPointer(),
            blob.GetBufferSize(),
            &ID3D12RootSignatureDeserializer::IID,
            &mut raw,
        )
        .unwrap();
        let decoder = ID3D12RootSignatureDeserializer::from_raw(raw);
        let root = &*decoder.GetRootSignatureDesc();
        assert_eq!(root.NumParameters, 1);
        assert_eq!(root.NumStaticSamplers, 0);
        assert_eq!(root.Flags, D3D12_ROOT_SIGNATURE_FLAG_NONE);
        let parameter = &*root.pParameters;
        assert_eq!(parameter.ShaderVisibility, D3D12_SHADER_VISIBILITY_PIXEL);
        assert_eq!(
            parameter.ParameterType,
            D3D12_ROOT_PARAMETER_TYPE_DESCRIPTOR_TABLE
        );
        assert_eq!(parameter.Anonymous.DescriptorTable.NumDescriptorRanges, 1);
        let range = &*parameter.Anonymous.DescriptorTable.pDescriptorRanges;
        assert_eq!(range.RangeType, D3D12_DESCRIPTOR_RANGE_TYPE_SRV);
        assert_eq!(
            (
                range.NumDescriptors,
                range.BaseShaderRegister,
                range.RegisterSpace,
                range.OffsetInDescriptorsFromTableStart
            ),
            (1, 0, 0, 0)
        );
    }
}
