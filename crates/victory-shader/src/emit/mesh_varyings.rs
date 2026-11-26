//! Link mesh and pixel varyings without changing the values sent between stages.
//!
//! Apply after lowering so the old element IDs and columns still name the
//! original DXIL accesses, with their actual interpolation and usage.

use super::{
    bitcode::{MeshVaryingSlot, MeshVaryingStage},
    container::{MESH_OUTPUT_LIMIT, SigElement, mesh_output_bytes},
    shader::Shader,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Component {
    pub id: usize,
    pub col: u8,
}

#[derive(Debug)]
pub(super) struct SignaturePlan {
    pub elements: Vec<SigElement>,
    /// Old element ID and relative column to new element ID and relative column.
    pub map: Vec<Vec<Component>>,
}

#[derive(Debug)]
pub(super) struct Plan {
    pub mesh_outputs: SignaturePlan,
    pub pixel_inputs: SignaturePlan,
}

const MERGES: [(u32, &str, u32); 4] = [
    (2, "TEXCOORD", 16),
    (3, "COLOR", 3),
    (4, "TEXCOORD", 15),
    (5, "TEXCOORD", 10),
];

/// Returns both signatures together. No input is consumed or changed on refusal.
pub(super) fn plan(
    mesh_outputs: &[SigElement],
    pixel_inputs: &[SigElement],
) -> Result<Plan, String> {
    Ok(Plan {
        mesh_outputs: signature(mesh_outputs, true).map_err(|e| format!("MS: {e}"))?,
        pixel_inputs: signature(pixel_inputs, false).map_err(|e| format!("PS: {e}"))?,
    })
}

/// Relinks a private pair after lowering; either error discards both programs.
pub(super) fn apply(mesh: &mut Shader, pixel: &mut Shader, keep_clips: bool) -> Result<(), String> {
    let plan = plan(&mesh.outputs, &pixel.inputs)?;
    apply_plan(mesh, pixel, plan, keep_clips)
}

/// Merge adjacent xyz/w TEXCOORDs that already share a register into a single
/// float4. Both stages use the same named anchor and preserve the original lanes.
/// This reduces the number of distinct attributes passed to the host compiler.
pub(super) fn apply_shared_rows(mesh: &mut Shader, pixel: &mut Shader) -> Result<(), String> {
    let plan = Plan {
        mesh_outputs: shared_rows(&mesh.outputs).map_err(|e| format!("MS: {e}"))?,
        pixel_inputs: shared_rows(&pixel.inputs).map_err(|e| format!("PS: {e}"))?,
    };
    apply_plan(mesh, pixel, plan, true)
}

const SHARED_ROWS: [(u32, u32); 5] = [(4, 7), (5, 8), (6, 14), (12, 15), (13, 16)];

fn shared_rows(elements: &[SigElement]) -> Result<SignaturePlan, String> {
    let mut new: Vec<_> = elements.iter().cloned().map(Some).collect();
    let mut map = identity_map(elements);
    for (xyz, w) in SHARED_ROWS {
        let xyz_id = varying(elements, "TEXCOORD", xyz, 3)?;
        let w_id = varying(elements, "TEXCOORD", w, 1)?;
        let (a, b) = (&elements[xyz_id], &elements[w_id]);
        if a.row != b.row || a.col != 0 || b.col != 3 {
            return Err(format!("TEXCOORD{xyz}.xyz and TEXCOORD{w}.w do not share a row"));
        }
        // Untouched rows can include built-ins packed beside a user varying.
        validate_rows(elements.iter().filter(|e| e.row == a.row))?;
        same_interpolation(a, b)?;
        let anchor = new[xyz_id].as_mut().unwrap();
        anchor.cols = 4;
        anchor.usage |= b.usage << 3;
        map[w_id][0] = Component { id: xyz_id, col: 3 };
        new[w_id] = None;
    }
    Ok(retained_elements(new, map))
}

fn apply_plan(mesh: &mut Shader, pixel: &mut Shader, plan: Plan, keep_clips: bool) -> Result<(), String> {
    if keep_clips {
        let limits = mesh.mesh.ok_or("MS: missing mesh output limits")?;
        let bytes = mesh_output_bytes(
            &plan.mesh_outputs.elements,
            &mesh.prims,
            limits.max_vertices,
            limits.max_primitives,
        );
        if bytes > MESH_OUTPUT_LIMIT {
            return Err("MS: packed outputs exceed the DXIL 32 KiB limit".into());
        }
    }
    relink(
        mesh,
        plan.mesh_outputs,
        MeshVaryingStage::MeshOutput,
        keep_clips,
    )?;
    relink(
        pixel,
        plan.pixel_inputs,
        MeshVaryingStage::PixelInput,
        keep_clips,
    )?;
    Ok(())
}

fn relink(
    shader: &mut Shader,
    plan: SignaturePlan,
    stage: MeshVaryingStage,
    keep_clips: bool,
) -> Result<(), String> {
    let SignaturePlan { mut elements, map } = plan;
    let writes_mesh = matches!(stage, MeshVaryingStage::MeshOutput);
    let original = match stage {
        MeshVaryingStage::MeshOutput => &mut shader.outputs,
        MeshVaryingStage::PixelInput => &mut shader.inputs,
    };
    let mut clip = None;
    if !keep_clips {
        for (id, element) in original.iter().enumerate().filter(|(_, e)| e.kind == 6) {
            if clip.is_some()
                || element.index != 0
                || element.col != 0
                || element.comp != 9
                || (!writes_mesh && element.usage != 0)
            {
                return Err("unsupported clip signature for mesh varying repack".into());
            }
            clip = Some((id, map[id][0].id));
        }
    }
    if let Some((_, new)) = clip {
        if writes_mesh {
            elements[new].cols = 1;
            elements[new].usage &= 1;
        } else {
            elements.remove(new);
            compact_rows(&mut elements);
        }
    }
    let map: Vec<_> = map
        .iter()
        .enumerate()
        .map(|(old, row)| {
            std::array::from_fn(|col| {
                let Some(component) = row.get(col) else {
                    return MeshVaryingSlot::OutsideWidth;
                };
                let mut new = component.id;
                if let Some((old_clip, new_clip)) = clip {
                    if old == old_clip && (!writes_mesh || col != 0) {
                        return MeshVaryingSlot::RemovedClip;
                    }
                    if !writes_mesh && new > new_clip {
                        new -= 1;
                    }
                }
                MeshVaryingSlot::Remap(new as u32, component.col)
            })
        })
        .collect();
    let stage_name = if writes_mesh { "MS" } else { "PS" };
    shader
        .m
        .remap_mesh_varying_calls(stage, &map)
        .map_err(|e| format!("{stage_name}: {e}"))?;
    *original = elements;
    Ok(())
}

fn signature(elements: &[SigElement], mesh: bool) -> Result<SignaturePlan, String> {
    validate_rows(elements.iter())?;
    let mut new: Vec<_> = elements.iter().cloned().map(Some).collect();
    let mut map = identity_map(elements);

    for (texcoord, donor_name, donor_index) in MERGES {
        let split = texcoord == 5;
        let target_id = varying(elements, "TEXCOORD", texcoord, 3)?;
        let donor_id = varying(elements, donor_name, donor_index, if split { 2 } else { 1 })?;
        let (target, donor) = (&elements[target_id], &elements[donor_id]);
        same_interpolation(target, donor)?;
        if mesh && (target.usage != 0b111 || donor.usage != (1 << donor.cols) - 1) {
            return Err(format!(
                "TEXCOORD{texcoord} or {donor_name}{donor_index} has an unwritten component"
            ));
        }

        let packed = new[target_id].as_mut().unwrap();
        packed.col = 0;
        packed.cols = 4;
        packed.usage |= (donor.usage & 1) << 3;
        map[donor_id][0] = Component {
            id: target_id,
            col: 3,
        };
        if !split {
            new[donor_id] = None;
            continue;
        }

        // TEXCOORD10.y remains its own semantic. The two removed scalars
        // leave room beside TEXCOORD1 in the fixture's signature.
        let vec2_id = varying(elements, "TEXCOORD", 1, 2)?;
        let vec2 = &elements[vec2_id];
        same_interpolation(vec2, donor)?;
        new[vec2_id].as_mut().unwrap().col = 0;
        let tail = new[donor_id].as_mut().unwrap();
        tail.row = vec2.row;
        tail.col = 2;
        tail.cols = 1;
        tail.usage = donor.usage >> 1;
        map[donor_id][1].col = 0;
    }

    validate_rows(new.iter().flatten())?;
    let mut plan = retained_elements(new, map);
    compact_rows(&mut plan.elements);
    Ok(plan)
}

fn identity_map(elements: &[SigElement]) -> Vec<Vec<Component>> {
    elements.iter().enumerate()
        .map(|(id, e)| (0..e.cols).map(|col| Component { id, col }).collect())
        .collect()
}

fn retained_elements(new: Vec<Option<SigElement>>, mut map: Vec<Vec<Component>>) -> SignaturePlan {
    let mut ids = Vec::with_capacity(new.len());
    let mut packed = Vec::new();
    for e in new {
        ids.push(e.map(|e| {
            let id = packed.len();
            packed.push(e);
            id
        }));
    }
    for component in map.iter_mut().flatten() {
        component.id = ids[component.id].expect("each mapped component has a retained element");
    }
    SignaturePlan { elements: packed, map }
}

fn varying(elements: &[SigElement], name: &str, index: u32, width: u8) -> Result<usize, String> {
    let mut found = elements
        .iter()
        .enumerate()
        .filter(|(_, e)| e.name.eq_ignore_ascii_case(name) && e.index == index);
    let (id, e) = found
        .next()
        .ok_or_else(|| format!("missing {name}{index}"))?;
    if found.next().is_some() {
        return Err(format!("ambiguous {name}{index}"));
    }
    // `half` marks the guest's representation. Lowering has already
    // emitted its conversions, so the host type is the effective `comp`.
    if e.kind != 0 || e.comp != 9 || e.cols != width || e.row.is_none() {
        return Err(format!("{name}{index} is not a placed user float{width}"));
    }
    Ok(id)
}

fn same_interpolation(a: &SigElement, b: &SigElement) -> Result<(), String> {
    if a.interp == b.interp && (1..=7).contains(&a.interp) {
        Ok(())
    } else {
        Err(format!(
            "{}.{} and {}.{} have incompatible interpolation",
            a.name, a.index, b.name, b.index
        ))
    }
}

/// Check the old and proposed row placements before any ID is renumbered.
fn validate_rows<'a>(elements: impl Iterator<Item = &'a SigElement>) -> Result<(), String> {
    let mut rows: [Option<(u8, &SigElement)>; 32] = [None; 32];
    for e in elements {
        let semantic = || format!("{}{}", e.name, e.index);
        if e.cols == 0 || u16::from(e.col) + u16::from(e.cols) > 4 {
            return Err(format!("{} has invalid column bounds", semantic()));
        }
        let components = (1u8 << e.cols) - 1;
        if e.usage & !components != 0 {
            return Err(format!(
                "{} has usage outside its declared components",
                semantic()
            ));
        }
        let Some(row) = e.row else { continue };
        let slot = rows
            .get_mut(row as usize)
            .ok_or_else(|| format!("{} has invalid row {row}", semantic()))?;
        let mask = components << e.col;
        if let Some((used, first)) = slot {
            if *used & mask != 0 {
                return Err(format!("{} overlaps row {row}", semantic()));
            }
            if (first.kind == 0 || e.kind == 0)
                && (first.kind, first.comp, first.interp) != (e.kind, e.comp, e.interp)
            {
                return Err(format!(
                    "{} has incompatible neighbours in row {row}",
                    semantic()
                ));
            }
            *used |= mask;
        } else {
            *slot = Some((mask, e));
        }
    }
    Ok(())
}

/// Close gaps left by the merges, without moving rows containing system values.
fn compact_rows(elements: &mut [SigElement]) {
    let mut occupied = [false; 32];
    let mut fixed = [false; 32];
    for e in elements.iter() {
        if let Some(row) = e.row {
            let row = row as usize;
            occupied[row] = true;
            fixed[row] |= e.kind != 0;
        }
    }
    let mut next = 0;
    let mut rows = [0; 32];
    for (row, destination) in rows.iter_mut().enumerate() {
        if fixed[row] {
            next = row as u32;
        }
        if occupied[row] {
            *destination = next;
            next += 1;
        }
    }
    for e in elements {
        if let Some(row) = e.row.as_mut() {
            *row = rows[*row as usize];
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn element(name: &str, index: u32, kind: u8, row: u32, col: u8, cols: u8) -> SigElement {
        SigElement {
            name: name.to_owned(),
            index,
            kind,
            comp: 9,
            interp: if kind == 3 { 4 } else { 2 },
            row: Some(row),
            col,
            cols,
            usage: (1u8 << cols) - 1,
            half: false,
        }
    }

    // OSG1 of mesh-clip1 and ISG1 of pixel-clip1-no-input from the fixture.
    fn fixture(mesh: bool) -> Vec<SigElement> {
        let mut elements = vec![element("SV_Position", 0, 3, 0, 0, 4)];
        if mesh {
            elements.push(element("SV_ClipDistance", 0, 6, 1, 0, 1));
        } else {
            elements[0].usage = 0;
        }
        for (name, index, row, col, cols) in [
            ("COLOR", 0, 2, 0, 4),
            ("COLOR", 1, 3, 0, 4),
            ("COLOR", 2, 4, 0, 4),
            ("COLOR", 3, 5, 0, 1),
            ("TEXCOORD", 1, 5, 1, 2),
            ("TEXCOORD", 15, 5, 3, 1),
            ("TEXCOORD", 0, 6, 0, 4),
            ("TEXCOORD", 2, 7, 0, 3),
            ("TEXCOORD", 16, 7, 3, 1),
            ("TEXCOORD", 3, 8, 0, 3),
            ("TEXCOORD", 4, 9, 0, 3),
            ("TEXCOORD", 5, 10, 0, 3),
            ("TEXCOORD", 6, 11, 0, 3),
            ("TEXCOORD", 7, 12, 0, 3),
            ("TEXCOORD", 8, 13, 0, 3),
            ("TEXCOORD", 10, 14, 0, 2),
            ("TEXCOORD", 14, 15, 0, 4),
        ] {
            elements.push(element(name, index, 0, row - u32::from(!mesh), col, cols));
        }
        elements
    }

    fn id(elements: &[SigElement], name: &str, index: u32) -> usize {
        elements
            .iter()
            .position(|e| e.name == name && e.index == index)
            .unwrap()
    }

    #[test]
    fn shared_xyz_and_w_keep_rows_masks_and_system_values() {
        let mut elements = vec![element("SV_ClipDistance", 0, 6, 0, 0, 4)];
        for (i, (xyz, w)) in SHARED_ROWS.into_iter().enumerate() {
            let row = i as u32 + 1;
            let mut a = element("TEXCOORD", xyz, 0, row, 0, 3);
            let mut b = element("TEXCOORD", w, 0, row, 3, 1);
            a.usage = 5;
            b.usage = i as u8 % 2;
            elements.push(b);
            elements.push(a);
        }
        let result = shared_rows(&elements).unwrap();
        assert_eq!(result.elements.len(), 6);
        assert_eq!(elements[0], result.elements[0]);
        let mut slots = std::collections::HashSet::new();
        for (old_id, old) in elements.iter().enumerate() {
            for col in 0..old.cols {
                let dst = result.map[old_id][col as usize];
                assert!(slots.insert((dst.id, dst.col)));
                let packed = &result.elements[dst.id];
                assert_eq!((old.row, old.col + col), (packed.row, packed.col + dst.col));
                assert_eq!((old.comp, old.interp), (packed.comp, packed.interp));
                assert_eq!((old.usage >> col) & 1, (packed.usage >> dst.col) & 1);
            }
        }
        assert_eq!(slots.len(), 24);
        for (i, (xyz, _)) in SHARED_ROWS.into_iter().enumerate() {
            let packed = &result.elements[i + 1];
            assert_eq!((packed.index, packed.cols), (xyz, 4));
        }
        elements[1].interp = 4;
        assert!(shared_rows(&elements).is_err());
        elements[1].interp = elements[2].interp;
        elements[1].row = Some(7);
        assert!(shared_rows(&elements).is_err());
    }

    #[test]
    fn relink_distinguishes_removed_clip_from_width_padding() {
        use crate::emit::bitcode::Attr;

        for (width, keep_clips, reason) in [
            (
                1,
                false,
                "relative column is outside the declared element width",
            ),
            (
                1,
                true,
                "relative column is outside the declared element width",
            ),
            (4, false, "clip value is a non-neutral f32 literal"),
        ] {
            let mut shader = Shader::new(false);
            shader.outputs = fixture(true);
            let clip = id(&shader.outputs, "SV_ClipDistance", 0);
            shader.outputs[clip].cols = width;
            let element = shader.m.i32(clip as u32);
            let zero = shader.m.i32(0);
            let column = shader.m.i8(1);
            let value = shader.m.f32(1.0_f32.to_bits());
            shader.op(
                shader.entry,
                "storeVertexOutput.f32",
                Attr::NoUnwind,
                shader.t.void,
                171,
                &[element, zero, column, value, zero],
            );
            let plan = signature(&shader.outputs, true).unwrap();
            assert_eq!(
                relink(&mut shader, plan, MeshVaryingStage::MeshOutput, keep_clips).unwrap_err(),
                format!("MS: storeVertexOutput: {reason}"),
            );
            assert_eq!(shader.outputs[clip].cols, width);
        }
    }

    #[test]
    fn keep_clips_preserves_live_stores_and_reads_within_mesh_budget() {
        use crate::emit::{
            bitcode::{Attr, Term},
            shader::Mesh,
        };

        let mut mesh = Shader::new(false);
        mesh.mesh = Some(Mesh {
            max_vertices: 256,
            max_primitives: 128,
            topology: 2,
        });
        mesh.outputs = fixture(true);
        let clip_mesh = mesh.outputs.iter_mut().find(|e| e.kind == 6).unwrap();
        clip_mesh.cols = 4;
        clip_mesh.usage = 15;

        let mut pixel = Shader::new(true);
        pixel.inputs = fixture(false);
        for e in pixel.inputs.iter_mut().skip(1) {
            e.row = e.row.map(|row| row + 1);
        }
        let mut clip_pixel = element("SV_ClipDistance", 0, 6, 1, 0, 4);
        clip_pixel.usage = 8;
        pixel.inputs.insert(1, clip_pixel);

        let vertex = mesh.m.i32(127);
        let value = mesh.m.f32((-1.0_f32).to_bits());
        let (clip, row, col) = (mesh.m.i32(1), mesh.m.i32(0), mesh.m.i8(3));
        mesh.op(
            mesh.entry,
            "storeVertexOutput.f32",
            Attr::NoUnwind,
            mesh.t.void,
            171,
            &[clip, row, col, value, vertex],
        );
        mesh.m.terminate(mesh.entry, Term::Ret);
        let (clip, row, col) = (pixel.m.i32(1), pixel.m.i32(0), pixel.m.i8(3));
        let vertex = pixel.m.undef(pixel.t.i32);
        pixel.op(
            pixel.entry,
            "loadInput.f32",
            Attr::ReadNone,
            pixel.t.f32,
            4,
            &[clip, row, col, vertex],
        );
        pixel.m.terminate(pixel.entry, Term::Ret);

        let mesh_signature = mesh.outputs.clone();
        let pixel_signature = pixel.inputs.clone();
        let mesh_body = mesh.m.write();
        let pixel_body = pixel.m.write();
        assert_eq!(
            apply(&mut mesh, &mut pixel, true).unwrap_err(),
            "MS: packed outputs exceed the DXIL 32 KiB limit"
        );
        assert_eq!(mesh.outputs, mesh_signature);
        assert_eq!(pixel.inputs, pixel_signature);

        mesh.mesh.as_mut().unwrap().max_vertices = 128;
        assert_eq!(
            apply(&mut mesh, &mut pixel, false).unwrap_err(),
            "MS: storeVertexOutput: clip value is a non-neutral f32 literal"
        );
        apply(&mut mesh, &mut pixel, true).unwrap();
        assert_eq!(mesh.outputs[1], mesh_signature[1]);
        assert_eq!(pixel.inputs[1], pixel_signature[1]);
        assert_eq!(mesh.m.write(), mesh_body);
        assert_eq!(pixel.m.write(), pixel_body);
        for (target, _, _) in MERGES {
            assert_eq!(mesh.outputs[id(&mesh.outputs, "TEXCOORD", target)].cols, 4);
            assert_eq!(pixel.inputs[id(&pixel.inputs, "TEXCOORD", target)].cols, 4);
        }
        let limits = mesh.mesh.unwrap();
        assert_eq!((limits.max_vertices, limits.max_primitives), (128, 128));
    }

    #[test]
    fn fixture_matches_packed_layout_and_preserves_all_components() {
        let (mesh, pixel) = (fixture(true), fixture(false));
        let result = plan(&mesh, &pixel).unwrap();
        for (old, new, vectors) in [
            (&mesh, &result.mesh_outputs, 15),
            (&pixel, &result.pixel_inputs, 14),
        ] {
            let mut seen = std::collections::HashSet::new();
            for (old_id, element) in old.iter().enumerate() {
                assert_eq!(new.map[old_id].len(), element.cols as usize);
                for (old_col, mapped) in new.map[old_id].iter().enumerate() {
                    assert!(seen.insert((mapped.id, mapped.col)));
                    let placed = &new.elements[mapped.id];
                    assert!(mapped.col < placed.cols);
                    assert_eq!(
                        (element.kind, element.comp, element.interp),
                        (placed.kind, placed.comp, placed.interp)
                    );
                    assert_eq!(
                        (element.usage >> old_col) & 1,
                        (placed.usage >> mapped.col) & 1
                    );
                    if element.kind != 0 {
                        assert_eq!(placed, element);
                        assert_eq!(mapped.col, old_col as u8);
                    }
                }
            }
            assert_eq!(
                seen.len(),
                new.elements.iter().map(|e| e.cols as usize).sum()
            );
            assert_eq!(
                new.elements.iter().filter_map(|e| e.row).max(),
                Some(vectors - 1)
            );
            let user: Vec<_> = new.elements.iter().filter(|e| e.kind == 0).collect();
            assert_eq!(user.len(), 14);
            assert_eq!(user.iter().map(|e| u32::from(e.cols)).sum::<u32>(), 48);
            assert_eq!(user.iter().filter(|e| e.cols == 3).count(), 3);
            for (anchor, donor_name, donor_index) in MERGES {
                let dst = id(&new.elements, "TEXCOORD", anchor);
                let source = id(old, "TEXCOORD", anchor);
                for col in 0..3 {
                    assert_eq!(new.map[source][col as usize], Component { id: dst, col });
                }
                assert_eq!(new.elements[dst].cols, 4);
                assert_eq!(
                    new.map[id(old, donor_name, donor_index)][0],
                    Component { id: dst, col: 3 }
                );
            }
            let tex1 = &new.elements[id(&new.elements, "TEXCOORD", 1)];
            let tex10 = id(&new.elements, "TEXCOORD", 10);
            let tail = &new.elements[tex10];
            assert_eq!((tex1.row, tex1.col), (Some(vectors - 10), 0));
            assert_eq!((tail.row, tail.col, tail.cols), (tex1.row, 2, 1));
            assert_eq!(
                new.map[id(old, "TEXCOORD", 10)][1],
                Component { id: tex10, col: 0 }
            );
        }
    }

    #[test]
    fn pixel_usage_is_remapped_even_for_an_unread_donor() {
        let mesh = fixture(true);
        let mut pixel = fixture(false);
        let old_id = id(&pixel, "TEXCOORD", 16);
        pixel[old_id].usage = 0;
        let new = plan(&mesh, &pixel).unwrap().pixel_inputs;
        let packed = id(&new.elements, "TEXCOORD", 2);
        assert_eq!(new.elements[packed].usage, 0b0111);
        assert_eq!(new.map[old_id][0], Component { id: packed, col: 3 });
    }

    #[test]
    fn rejects_incompatible_pixel_interpolation() {
        let mut pixel = fixture(false);
        let i = id(&pixel, "TEXCOORD", 3);
        pixel[i].interp = 4;
        let error = plan(&fixture(true), &pixel).unwrap_err();
        assert_eq!(
            error,
            "PS: TEXCOORD.3 and COLOR.3 have incompatible interpolation"
        );
    }

    #[test]
    fn rejects_non_float_or_system_components() {
        let mut mesh = fixture(true);
        let i = id(&mesh, "TEXCOORD", 4);
        mesh[i].comp = 5;
        assert_eq!(
            plan(&mesh, &fixture(false)).unwrap_err(),
            "MS: TEXCOORD4 is not a placed user float3"
        );
        mesh[i].comp = 9;
        mesh[i].kind = 6;
        assert_eq!(
            plan(&mesh, &fixture(false)).unwrap_err(),
            "MS: TEXCOORD4 is not a placed user float3"
        );
    }

    #[test]
    fn rejects_unwritten_or_invalid_usage() {
        let mut mesh = fixture(true);
        let i = id(&mesh, "TEXCOORD", 4);
        mesh[i].usage = 0b011;
        assert_eq!(
            plan(&mesh, &fixture(false)).unwrap_err(),
            "MS: TEXCOORD4 or TEXCOORD15 has an unwritten component"
        );
        mesh[i].usage = 0b1111;
        assert_eq!(
            plan(&mesh, &fixture(false)).unwrap_err(),
            "MS: TEXCOORD4 has usage outside its declared components"
        );
    }

    #[test]
    fn rejects_overlapping_proposed_rows() {
        let mut mesh = fixture(true);
        mesh.push(element("TEXCOORD", 17, 0, 8, 3, 1));
        assert_eq!(
            plan(&mesh, &fixture(false)).unwrap_err(),
            "MS: TEXCOORD17 overlaps row 8"
        );
    }

    #[test]
    fn rejects_missing_or_ambiguous_semantics() {
        let mut pixel = fixture(false);
        let i = id(&pixel, "TEXCOORD", 16);
        let mut donor = pixel.remove(i);
        assert_eq!(
            plan(&fixture(true), &pixel).unwrap_err(),
            "PS: missing TEXCOORD16"
        );
        pixel.push(donor.clone());
        donor.name = "texcoord".to_owned();
        donor.row = Some(16);
        pixel.push(donor);
        assert_eq!(
            plan(&fixture(true), &pixel).unwrap_err(),
            "PS: ambiguous TEXCOORD16"
        );
    }
}
