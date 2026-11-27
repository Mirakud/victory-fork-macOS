//! Bindless texture indices sampled by the facial Ped pixel program. Its
//! heap lookups are invisible to the ordinary explicit table bindings.
use super::{HostList, HostPipeline, Root, State, This, View, readable};
use crate::descriptor::{self, host::HostHeap};
use std::{collections::HashSet, sync::OnceLock};
use victory_shader::ResourceType;

const MESH: &str = "f841ad74ec6e7367";
const PIXEL: &str = "6695d7f5e3f6d351";

pub(super) fn prepare(state: &State, list: &HostList, pipeline: &HostPipeline) {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    if !pipeline.mesh
        || !*ENABLED.get_or_init(|| {
            std::env::var_os("VICTORY_PED_MATERIAL_TEXTURES").is_some_and(|v| v == "1")
        })
    {
        return;
    }
    let [mesh, pixel] = pipeline.stages.as_slice() else {
        return;
    };
    if mesh.shader.hash != MESH || pixel.shader.hash != PIXEL {
        return;
    }
    if state.heaps[0] == 0 {
        return;
    }
    // State::set_heaps puts the bound non-sampler heap in slot zero.
    let heap = descriptor::heap(state.heaps[0] as This);
    let Some(host) = heap.host() else { return };
    let view = |slot: usize, dwords: u32| {
        let Root::Table(start) = state.graphics.get(slot)? else {
            return None;
        };
        let address = start.checked_add(u64::from(dwords) * 4)?;
        // This record must be a complete descriptor in the bound view heap.
        if heap.index(address).and_then(|i| heap.handle(i)) != Some(address) {
            return None;
        }
        descriptor::view(address)
    };
    let mut seen = HashSet::new();
    let offset = match state.graphics.get(4) {
        Some(Root::Constants(words)) => words.get(1).copied(),
        _ => None,
    };
    material_indices(view, offset, |word| {
        // The translated shader shifts left by 5 then right by 5 to obtain
        // its descriptor index. The reserved host null slot stays untouched.
        let index = word & 0x07ff_ffff;
        if index < host.count()
            && seen.insert(index)
            && let Some(handle) = heap.handle(index)
        {
            prepare_index(list, host, index, handle);
        }
    });
}

fn material_indices(
    view: impl Fn(usize, u32) -> Option<View>,
    offset: Option<u32>,
    mut prepare_slot: impl FnMut(u32),
) {
    let buffer = |slot, dwords| descriptor::host::buffer_range(&view(slot, dwords)?);
    let constants = |dwords| match view(1, dwords)? {
        View::Constant(base, size) => Some((base, u64::from(size))),
        _ => None,
    };
    if let Some(offset) = offset
        && let Some(record) = buffer(0, 688)
        && let Some(indices) = read_words::<9>(record, offset)
    {
        indices.into_iter().for_each(&mut prepare_slot);
    }
    for (dword, offset, fields) in [(88, 0, &[0, 2][..]), (104, 36, &[0, 1, 2][..])] {
        if let Some(view) = constants(dword)
            && let Some(words) = read_words::<3>(view, offset)
        {
            for &field in fields {
                prepare_slot(words[field]);
            }
        }
    }
    if let Some(count) = constants(96).and_then(|v| read_words::<1>(v, 0))
        && let Some(records) = buffer(1, 16)
    {
        // The shader's loop reads these fields of 128-byte records. Its
        // count is a separate constant. The DXIL also caps the loop at 128.
        for i in 0..u64::from(count[0]).min(128).min(records.1 / 128) as u32 {
            if let Some(words) = read_words::<32>(records, i * 128) {
                for field in [3, 7, 8, 24, 25, 31] {
                    prepare_slot(words[field]);
                }
            }
        }
    }
    if let Some(layers) = buffer(1, 40) {
        layer_indices(layers, prepare_slot);
    }
}

type Range = (u64, u64);

fn read_words<const N: usize>((base, size): Range, offset: u32) -> Option<[u32; N]> {
    let mut words = [[0u8; 4]; N];
    let bytes = words.as_flattened_mut();
    let off = u64::from(offset);
    if base == 0 || !offset.is_multiple_of(4) || off + bytes.len() as u64 > size {
        return None;
    }
    let address = base.checked_add(off)?;
    address.checked_add(bytes.len() as u64)?;
    crate::host::read_into(address as *const u8, bytes).then(|| words.map(u32::from_le_bytes))
}

/// Five layer families with a nonempty header and missing sampled views in
/// the captured face draws. Header words pack count:4, byte stride:12, and
/// byte offset:16. Only the shader's sampled fields are treated as indices.
fn layer_indices(range: Range, mut emit: impl FnMut(u32)) {
    let Some(headers) = read_words::<8>(range, 0) else {
        return;
    };
    let groups: &[(usize, &[u32])] = &[
        (3, &[0]),
        (4, &[72, 76, 80, 84]),
        (5, &[0, 4, 8]),
        (6, &[4, 40]),
        (7, &[0, 4, 8, 12]),
    ];
    for &(g, fields) in groups {
        let header = headers[g];
        let base = header & 0xffff;
        let stride = (header >> 16) & 0xfff;
        let count = header >> 28;
        if !base.is_multiple_of(4)
            || !stride.is_multiple_of(4)
            || fields.last().is_none_or(|last| last + 4 > stride)
        {
            continue;
        }
        for i in 0..count {
            let record = base + i * stride;
            if u64::from(record + stride) > range.1 {
                break;
            }
            for &field in fields {
                if let Some([value]) = read_words::<1>(range, record + field) {
                    emit(value);
                }
            }
        }
    }
}

fn mapped(view: &View) -> bool {
    let (address, size) = match view {
        View::Placed(false, shape, desc) if shape[0] == 3 => {
            let address = u64::from(desc[10]) | (u64::from(desc[11]) << 32);
            (address, crate::resource::placed_bytes(shape))
        }
        View::Shader(resource, _) if crate::resource::title_fills(*resource as This) => {
            let Some((address, desc)) = crate::resource::texture_memory(*resource as This) else {
                return false;
            };
            (
                address as u64,
                crate::resource::texture_layout(&desc).map(|t| t.size),
            )
        }
        _ => return false,
    };
    // The generic residency scan also skips a texture if both ends are
    // absent. We only need evidence of some mapped bytes, not a page count.
    let Some(last) = size
        .and_then(|n| n.checked_sub(1))
        .and_then(|n| address.checked_add(n))
    else {
        return false;
    };
    address != 0
        && (crate::texture::committed(address as *const u8, 1)
            || crate::texture::committed(last as *const u8, 1))
}

/// Returns true only when this call made a previously unavailable host view.
fn prepare_index(list: &HostList, host: &HostHeap, index: u32, handle: u64) -> bool {
    let Some(view) = descriptor::view(handle) else {
        // A slot that previously named a resource now reads null.
        host.mirror(index, handle, true);
        return false;
    };
    if !descriptor::host::is_texture_view(&view) {
        return false;
    }
    let ready = host.texture_slot_made_from(index, handle);
    if !ready && !mapped(&view) {
        return false;
    }
    let Some(current) = host.mirror(index, handle, true) else {
        return false;
    };
    if !descriptor::host::is_texture_view(&current) {
        return false;
    }
    // Mirror rechecks the current descriptor generation. Use the same upload
    // and resource barriers as a texture explicitly bound through a table.
    // upload deduplicates each resource per command list. Leave speculative
    // queue and mapping notifications intact, notably for partly mapped mips.
    readable(list, ResourceType::Srv, &current);
    !ready && host.texture_slot_made_from(index, handle)
}

#[cfg(all(test, feature = "diagnostics"))]
mod tests {
    use super::*;
    use std::ptr::null;

    #[test]
    fn follows_only_sampled_indices_and_bounded_layer_records() {
        fn cbv(words: &[u32]) -> View {
            View::Constant(words.as_ptr() as u64, size_of_val(words) as u32)
        }
        fn srv(words: &[u32]) -> View {
            let address = words.as_ptr() as u64;
            let mut shape = [0; 14];
            shape[0] = 1;
            let mut desc = [0; 16];
            desc[1] = 1;
            desc[6] = words.len() as u32;
            desc[7] = 4;
            desc[10] = address as u32;
            desc[11] = (address >> 32) as u32;
            View::Placed(false, shape, desc)
        }
        let constants88 = [21, 999, 22];
        let mut constants104 = [999; 12];
        constants104[9..].copy_from_slice(&[23, 24, 25]);
        let records: [u32; 64] = std::array::from_fn(|i| i as u32 + 1000);
        // Count greater than the two records covered by the bound SRV.
        let count = [u32::MAX];
        let mut layers: [u32; 24] = std::array::from_fn(|i| i as u32 + 2000);
        layers[..8].fill(0);
        // Two 32-byte records starting at byte 32: field 0 only.
        layers[3] = 2 << 28 | 32 << 16 | 32;
        // Out-of-bounds third record and too-short record for family four.
        layers[3] += 1 << 28;
        layers[4] = 1 << 28 | 4 << 16 | 32;
        let input = |slot, dword| match (slot, dword) {
            (1, 88) => Some(cbv(&constants88)),
            (1, 104) => Some(cbv(&constants104)),
            (1, 96) => Some(cbv(&count)),
            (1, 16) => Some(srv(&records)),
            (1, 40) => Some(srv(&layers)),
            _ => None,
        };
        let mut values = vec![];
        material_indices(input, None, |i| values.push(i));
        assert_eq!(
            values,
            [
                21, 22, 23, 24, 25, 1003, 1007, 1008, 1024, 1025, 1031, 1035, 1039, 1040, 1056,
                1057, 1063, 2008, 2016
            ]
        );
        let mut result = vec![];
        material_indices(
            |slot, dword| match (slot, dword) {
                (1, 88) => Some(cbv(&constants88[..2])),
                (1, 104) => Some(srv(&constants104)),
                _ => None,
            },
            None,
            |i| result.push(i),
        );
        assert!(result.is_empty());
    }

    #[test]
    #[ignore = "requires a D3D12 device; uses only data generated by the test"]
    fn loads_mapped_ped_textures_at_a_full_budget() {
        assert_eq!(
            std::env::var("VICTORY_TEXTURE_BUDGET_MB").as_deref(),
            Ok("0")
        );
        let host = crate::host::host().unwrap();
        let list = host.new_list().unwrap();
        let heap = HostHeap::new(false, 32).unwrap();
        assert!(!crate::resource::room());
        let mut record = [0u32; 12];
        record[2..11].copy_from_slice(&[5, 6, 5, 3, 5, 5, 5, 0, 0x0800_0003]);
        let base = record.as_ptr() as u64;
        assert_eq!(
            read_words::<9>((base, 44), 8),
            Some([5, 6, 5, 3, 5, 5, 5, 0, 0x0800_0003])
        );
        assert!(read_words::<9>((base, 43), 8).is_none());
        let shape = [3, 0, 0, 0, 64, 0, 64, 65537, 28, 1, 0, 0, 0, 0];
        let data = vec![0x7b_u8; crate::resource::placed_bytes(&shape).unwrap() as usize + 65535];
        let address = (data.as_ptr() as usize).next_multiple_of(65536) as u64;
        let mut handles = [[0_u64; 4]; 3];
        let base_handle = handles.as_mut_ptr() as usize;
        let slot = |i: usize| (base_handle + i * size_of::<[u64; 4]>()) as u64;
        let desc = |address: u64| {
            [
                28,
                4,
                5768,
                0,
                0,
                1,
                0,
                0,
                0,
                0,
                address as u32,
                (address >> 32) as u32,
                0,
                0,
                0,
                0,
            ]
        };
        let good = desc(address);
        let absent = desc(0);
        descriptor::create_placed_srv(null(), &shape, &good, slot(0));
        descriptor::create_placed_srv(null(), &shape, &absent, slot(1));
        assert!(!prepare_index(&list, &heap, 7, slot(2)));
        assert!(
            !prepare_index(&list, &heap, 6, slot(1)) && !heap.texture_slot_made_from(6, slot(1))
        );
        assert!(prepare_index(&list, &heap, 5, slot(0)));
        assert!(
            !prepare_index(&list, &heap, 5, slot(0)) && heap.texture_slot_made_from(5, slot(0))
        );
        let view = descriptor::view(slot(0)).unwrap();
        let Some(View::Shader(resource, _)) = descriptor::host::placed_texture(&view) else {
            panic!("made texture")
        };
        let texture = crate::resource::host_resource(resource as This).unwrap();
        crate::list::ensure_state(
            &list,
            resource as This,
            super::super::D3D12_RESOURCE_STATE_COPY_SOURCE.0 as u32,
        );
        let (buffer, layout) =
            crate::capture::read_back(&host.device, &list.list, texture).unwrap();
        list.close();
        host.execute(&[&list]);
        assert_eq!((layout.Footprint.Width, layout.Footprint.Height), (64, 64));
        let mut ptr = std::ptr::null_mut();
        // SAFETY: execute has fenced the copy of the 64×64 RGBA8 texture.
        unsafe {
            buffer.Map(0, None, Some(&mut ptr)).unwrap();
            for y in 0..64 {
                let row = ptr
                    .cast::<u8>()
                    .add(layout.Offset as usize + y * layout.Footprint.RowPitch as usize);
                assert!(
                    std::slice::from_raw_parts(row, 64 * 4)
                        .iter()
                        .all(|&b| b == 0x7b)
                );
            }
            buffer.Unmap(0, None);
        }
        println!(
            "4096 RGBA8 texels read back from the mapped texture at budget 0; duplicate slot retained; empty and unmapped views skipped"
        );
        handles[0][0] = 0;
        assert!(!prepare_index(
            &host.new_list().unwrap(),
            &heap,
            5,
            handles[0].as_ptr() as u64
        ));
        assert!(!heap.made(5));
    }
}
