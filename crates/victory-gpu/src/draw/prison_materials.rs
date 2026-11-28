//! Prefetch the sampled texture descriptors referenced indirectly by Series S
//! material records. The byte offsets below were derived from DXIL texture
//! handles in the original translator's output, including the index mask.
use super::{HostList, State, This, View, readable};
use crate::descriptor::{self, host::HostHeap};
use std::sync::{
    OnceLock,
    atomic::{AtomicU64, Ordering::Relaxed},
};
use victory_shader::{ResourceType, RootType};

fn sampled_fields(hash: &str) -> &'static [usize] {
    match hash {
        "05dffa358c51a83e" | "09eb0a73df63b87d" | "43e24028d4a1faef" | "47ec43235084505f"
        | "6590393ed93ff071" | "8a5351822b1a4929" | "b8e4a3f98d0d271e" | "c10e7d95d8266d2a"
        | "ee78da6862364ddd" => &[0, 1, 3, 4, 5],
        "271a87c54ac648a5" => &[0, 1, 3, 4],
        "3711fdad456538a1" | "3ec62ecabb36f1d2" | "428832e0a62f137f" | "659fd97066bea4fc"
        | "de41d84ca748cc29" => &[0, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11],
        "398971d1222d8f95" => &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11],
        "43fc2af1f70920f1" | "d9fcb8a4425fc59b" => &[0, 1, 2],
        "4a45195a99ac14f7" | "4f98bfae2e053df9" | "57a27c5e2e1d62a9" | "63051e97862e7dad"
        | "e5e9af2bbd2c006a" | "fac9df8d21a00b68" => &[0, 1, 2, 3, 4],
        "508693cae88bae04" | "c9443dc523f51e29" | "f95c2dcd205a0f5f" => {
            &[0, 1, 2, 3, 4, 5, 6, 7, 9, 10, 11]
        }
        "573d1857e903fab5" | "68cf7088c42fc83f" | "794198ca2f7c9b67" | "8e9b3db8a3eeba51"
        | "909d7b7af414d9e0" | "b5640b1075483a54" | "b8df138593588098" | "efc089fe381c43bf" => {
            &[0, 1, 3, 4, 5, 6]
        }
        "6695d7f5e3f6d351" => &[0, 1, 2, 3, 4, 5, 6, 7, 8],
        "7bdefdfde461f6e3" => &[0, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15],
        "7fe75645c406cac6" | "9106dcae9c22bb51" => &[0, 1, 2, 3, 4, 5, 6],
        "818a9a0ec6a53abb" | "94d281151ca58b4f" => &[2],
        "8429a5ab281af82a" => &[0, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16],
        "e9a5de89d65dbeee" => &[0, 1, 2, 3, 4, 5],
        _ => &[],
    }
}

pub(super) fn prepare(state: &State, list: &HostList, stage: &crate::pipeline::host::Stage) {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    if state.heaps[0] == 0
        || stage.shader.stage != "pixel"
        || !*ENABLED.get_or_init(|| {
            std::env::var_os("VICTORY_PRISON_MATERIAL_TEXTURES").is_some_and(|v| v == "1")
        })
    {
        return;
    }
    let fields = sampled_fields(&stage.shader.hash);
    if fields.is_empty() {
        return;
    }
    let user = &stage.translated().host.user_data;
    let Some(table) = user
        .iter()
        .find(|u| u.index == 8 && u.root == RootType::Table)
    else {
        return;
    };
    let Some(offset) = user
        .iter()
        .find(|u| u.index == 14 && u.root == RootType::Constants)
    else {
        return;
    };
    let Some(view) = state.table_view(false, table.slot, 688) else {
        return;
    };
    let Some((base, size)) = descriptor::host::buffer_range(&view) else {
        return;
    };
    let byte = u64::from(state.user_value(false, offset.slot, offset.root, offset.dword));
    let heap = descriptor::heap(state.heaps[0] as This);
    let Some(host) = heap.host() else {
        return;
    };
    for_sampled_indices((base, size), byte, fields, host.count(), |index| {
        if let Some(handle) = heap.handle(index)
            && prepare_index(list, host, index, handle)
        {
            static MADE: AtomicU64 = AtomicU64::new(0);
            let made = MADE.fetch_add(1, Relaxed) + 1;
            if made.is_power_of_two() {
                tracing::info!(target: "victory_gpu::memory_watch", made, index, shader = %stage.shader.hash,
                               "prepared a material's sampled texture before its draw");
            }
        }
    });
}

fn for_sampled_indices(
    (base, size): (u64, u64),
    byte: u64,
    fields: &[usize],
    count: u32,
    mut sampled: impl FnMut(u32),
) {
    let Some(&last) = fields.last() else {
        return;
    };
    let mut record = [0u8; 68];
    let Some(bytes) = record.get_mut(..(last + 1) * 4) else {
        return;
    };
    let Some(end) = byte.checked_add(bytes.len() as u64) else {
        return;
    };
    let Some(src) = base.checked_add(byte) else {
        return;
    };
    if end > size || !crate::host::read_into(src as *const u8, bytes) {
        return;
    }
    let mut seen = [u32::MAX; 17];
    for (n, &field) in fields.iter().enumerate() {
        let at = field * 4;
        let index = u32::from_le_bytes(record[at..at + 4].try_into().unwrap()) & 0x07ff_ffff;
        if index < count && !seen[..n].contains(&index) {
            seen[n] = index;
            sampled(index);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reads_only_bound_material_fields_and_resolves_descriptor_tags() {
        let record = [99_u32, 79, 0x8800_0007, 258, 48, 7, 0xf800_0009];
        let range = (
            record.as_ptr() as u64,
            std::mem::size_of_val(&record) as u64,
        );
        let fields = sampled_fields("43e24028d4a1faef");
        let mut slots = Vec::new();
        for_sampled_indices(range, 4, fields, 100, |i| slots.push(i));
        assert_eq!(slots, [79, 7, 48, 9]);
        for_sampled_indices(range, 8, fields, 100, |_| panic!("out-of-range material"));
        assert!(sampled_fields("89069e131eabee03").is_empty());
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
