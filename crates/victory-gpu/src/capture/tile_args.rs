//! Diagnostic snapshots of the title's 120 indirect draw records for tiled materials.
//! Each copy is recorded at the command it describes and receives its own fenced buffer.
use super::{HostList, HostPipeline, Root, State};
use crate::host::{host, transition};
use crate::shadow::Shadow;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use windows::Win32::Graphics::Direct3D12::*;
use windows::core::Interface;

const RECORD_BYTES: usize = 16;
const TABLE_BYTES: usize = 120 * RECORD_BYTES;

#[derive(Clone, Copy, Debug)]
enum Phase {
    Reset,
    Material,
    Source,
    Bounded,
}

impl Phase {
    fn label(self) -> &'static str {
        match self {
            Self::Reset => "reset",
            Self::Material => "material",
            Self::Source => "source",
            Self::Bounded => "bounded",
        }
    }
}

pub(crate) struct Sample {
    frame: u64,
    ordinal: usize,
    phase: Phase,
    guest: u64,
    host_id: usize,
    offset: u64,
    len: usize,
    readback: ID3D12Resource,
    published: AtomicBool,
}

fn selected() -> Option<u64> {
    let frame = crate::queue::presented() + 1;
    crate::capture::target_frame(frame).then_some(frame)
}

fn ordinal(frame: u64, phase: Phase) -> Option<usize> {
    static VISITS: Mutex<(u64, [usize; 4])> = Mutex::new((0, [0; 4]));
    let mut visits = VISITS.lock().unwrap_or_else(|e| e.into_inner());
    if visits.0 != frame {
        *visits = (frame, [0; 4]);
    }
    let slot = &mut visits.1[phase as usize];
    if *slot >= 4 {
        return None;
    }
    *slot += 1;
    Some(*slot)
}

impl Sample {
    #[allow(clippy::too_many_arguments)]
    fn record(
        list: &HostList,
        frame: u64,
        ordinal: usize,
        phase: Phase,
        guest: u64,
        buffer: &ID3D12Resource,
        offset: u64,
        len: usize,
        state: D3D12_RESOURCE_STATES,
    ) -> Option<Self> {
        if offset.checked_add(len as u64)? > unsafe { buffer.GetDesc() }.Width {
            return None;
        }
        let readback = host()?.read_back_buffer(len as u64)?;
        let copy = D3D12_RESOURCE_STATE_COPY_SOURCE;
        // The producing dispatch used its UAV. The clamp read its source as an SRV and
        // explicitly left its private output in INDIRECT_ARGUMENT. Restore each state.
        unsafe {
            list.list
                .ResourceBarrier(&[transition(buffer, state, copy)]);
            list.list
                .CopyBufferRegion(&readback, 0, buffer, offset, len as u64);
            list.list
                .ResourceBarrier(&[transition(buffer, copy, state)]);
        }
        list.keep(buffer);
        Some(Self {
            frame,
            ordinal,
            phase,
            guest,
            host_id: buffer.as_raw() as usize,
            offset,
            len,
            readback,
            published: AtomicBool::new(false),
        })
    }

    fn bytes(&self) -> Option<Vec<u8>> {
        let range = D3D12_RANGE {
            Begin: 0,
            End: self.len,
        };
        let mut data = std::ptr::null_mut();
        unsafe {
            self.readback.Map(0, Some(&range), Some(&mut data)).ok()?;
            let bytes = (!data.is_null())
                .then(|| std::slice::from_raw_parts(data.cast::<u8>(), self.len).to_vec());
            self.readback.Unmap(0, Some(&D3D12_RANGE::default()));
            bytes
        }
    }

    pub(crate) fn landed(&self) {
        // Replaying a title command list must preserve the first fenced observation on disk.
        if self.published.swap(true, Ordering::AcqRel) {
            return;
        }
        let Some(bytes) = self.bytes() else { return };
        let record = |r: &[u8]| -> [u32; 4] {
            std::array::from_fn(|i| u32::from_le_bytes(r[i * 4..i * 4 + 4].try_into().unwrap()))
        };
        let first = record(&bytes[..RECORD_BYTES]);
        let active: Vec<usize> = bytes
            .chunks_exact(RECORD_BYTES)
            .enumerate()
            .filter_map(|(i, r)| {
                let d = record(r);
                (d[0] != 0 && d[1] != 0).then_some(i)
            })
            .collect();
        let label = self.phase.label();
        let name = format!(
            "frame-{}-tile-args-{label}-{}.bin",
            self.frame, self.ordinal
        );
        let saved =
            crate::capture::folder().is_some_and(|f| std::fs::write(f.join(&name), &bytes).is_ok());
        tracing::info!(target: "victory_gpu::memory_watch", frame=self.frame, phase=label, ordinal=self.ordinal,
            guest=format_args!("{:#x}", self.guest), resource=format_args!("{:#x}", self.host_id), offset=self.offset,
            bytes=bytes.len(), ?first, ?active, file=%name, saved, "captured tiled lighting indirect records");
    }
}

pub(super) fn producer(state: &State, list: &HostList, p: &HostPipeline) {
    let Some(frame) = selected() else { return };
    let Some(stage) = p.stages.first() else {
        return;
    };
    let (phase, offset) = match stage.shader.hash.as_str() {
        "28153514665eb0ad" => (Phase::Reset, 0),
        "fea2b7d3f17b504f" => (Phase::Material, 16 * 4),
        _ => return,
    };
    let Some(Root::Table(table)) = state.compute.get(1) else {
        return;
    };
    let Some(handle) = table.checked_add(offset) else {
        return;
    };
    let Some(heap) = state.heap_of(handle) else {
        return;
    };
    let Some((slot, live)) = heap.index(handle).zip(heap.host_when_ready()) else {
        return;
    };
    let Some(view) = crate::descriptor::view(handle) else {
        return;
    };
    let Some((shadow, guest, len)) = crate::descriptor::host::buffer_copy(&view) else {
        return;
    };
    // Both title shaders bind table 1 without CBVs. Only sample a live slot matching the
    // resource the descriptor describes. An unrelated shadow would obscure an alias bug.
    if len != TABLE_BYTES as u64 || live.shows(slot, &shadow.buffer) != Some(true) {
        tracing::warn!(target: "victory_gpu::memory_watch", frame, slot, len, "tiled arguments UAV identity unavailable");
        return;
    }
    if let Some(n) = ordinal(frame, phase)
        && let Some(sample) = Sample::record(
            list,
            frame,
            n,
            phase,
            guest,
            &shadow.buffer,
            shadow.offset(guest),
            TABLE_BYTES,
            D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
        )
    {
        list.keep_tile_args(sample);
    }
}

pub(super) fn consumer(
    list: &HostList,
    p: &HostPipeline,
    args: &Shadow,
    guest: u64,
    bounded: &ID3D12Resource,
    at: u64,
) {
    let Some(frame) = selected() else { return };
    // Dry STANDARD is record zero. Matching both stages excludes camera, water and SSS passes.
    if !p.stages.iter().any(|s| s.shader.hash == "25ebc92075b62822")
        || !p.stages.iter().any(|s| s.shader.hash == "1cd9bc634417a981")
    {
        return;
    }
    let Some(n) = ordinal(frame, Phase::Source) else {
        return;
    };
    let len = args.left(guest).min(TABLE_BYTES as u64) as usize / RECORD_BYTES * RECORD_BYTES;
    if len >= RECORD_BYTES
        && let Some(sample) = Sample::record(
            list,
            frame,
            n,
            Phase::Source,
            guest,
            &args.buffer,
            args.offset(guest),
            len,
            D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE,
        )
    {
        list.keep_tile_args(sample);
    }
    if let Some(sample) = Sample::record(
        list,
        frame,
        n,
        Phase::Bounded,
        guest,
        bounded,
        at,
        RECORD_BYTES,
        D3D12_RESOURCE_STATE_INDIRECT_ARGUMENT,
    ) {
        list.keep_tile_args(sample);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "real Win64 GPU clamp and ordered buffer readbacks"]
    fn producer_and_clamp_samples_survive_reuse_of_the_same_gpu_buffers() {
        let h = host().expect("host GPU");
        let list = h.new_list().unwrap();
        let args = h.uav_buffer(TABLE_BYTES as u64).unwrap();
        let (upload, mapped) = h.upload_buffer((2 * TABLE_BYTES) as u64).unwrap();
        let mut zero = vec![0u32; TABLE_BYTES / 4];
        for r in zero.chunks_exact_mut(4) {
            r[0] = 180;
        }
        let mut one = zero.clone();
        one[1] = 1;
        one[9] = 1;
        unsafe {
            std::ptr::copy_nonoverlapping(zero.as_ptr(), mapped.cast::<u32>(), zero.len());
            std::ptr::copy_nonoverlapping(
                one.as_ptr(),
                mapped.add(TABLE_BYTES).cast::<u32>(),
                one.len(),
            );
        }
        let gpu_write = D3D12_RESOURCE_STATE_UNORDERED_ACCESS;
        let read = D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE;
        let dest = D3D12_RESOURCE_STATE_COPY_DEST;
        let as_args = D3D12_RESOURCE_STATE_INDIRECT_ARGUMENT;
        let copy_to_args = |from, at, to| unsafe {
            list.list.ResourceBarrier(&[transition(&args, from, dest)]);
            list.list
                .CopyBufferRegion(&args, 0, &upload, at, TABLE_BYTES as u64);
            list.list.ResourceBarrier(&[transition(&args, dest, to)]);
        };
        copy_to_args(D3D12_RESOURCE_STATE_COMMON, 0, gpu_write);
        let reset = Sample::record(
            &list,
            7,
            1,
            Phase::Reset,
            0,
            &args,
            0,
            TABLE_BYTES,
            gpu_write,
        )
        .unwrap();
        copy_to_args(gpu_write, TABLE_BYTES as u64, gpu_write);
        let material = Sample::record(
            &list,
            7,
            1,
            Phase::Material,
            0,
            &args,
            0,
            TABLE_BYTES,
            gpu_write,
        )
        .unwrap();
        unsafe {
            list.list
                .ResourceBarrier(&[transition(&args, gpu_write, read)])
        };
        let bounded = crate::indirect::bound(&list, (&args, 0), None, (0, 16, 1)).unwrap();
        let source =
            Sample::record(&list, 7, 1, Phase::Source, 0, &args, 0, TABLE_BYTES, read).unwrap();
        let first = Sample::record(
            &list,
            7,
            1,
            Phase::Bounded,
            0,
            &bounded,
            0,
            RECORD_BYTES,
            as_args,
        )
        .unwrap();
        let publish = Sample::record(
            &list,
            7,
            3,
            Phase::Bounded,
            0,
            &bounded,
            0,
            RECORD_BYTES,
            as_args,
        )
        .unwrap();
        list.keep_tile_args(publish);
        copy_to_args(read, 0, read);
        let overwritten = crate::indirect::bound(&list, (&args, 0), None, (0, 16, 1)).unwrap();
        assert_eq!(overwritten.as_raw(), bounded.as_raw());
        let last = Sample::record(
            &list,
            7,
            2,
            Phase::Bounded,
            0,
            &overwritten,
            0,
            RECORD_BYTES,
            as_args,
        )
        .unwrap();
        list.close();
        h.execute(&[&list]);
        let encoded = |words: &[u32]| {
            words
                .iter()
                .flat_map(|w| w.to_le_bytes())
                .collect::<Vec<_>>()
        };
        assert_eq!(reset.bytes().unwrap(), encoded(&zero));
        assert_eq!(material.bytes().unwrap(), encoded(&one));
        assert_eq!(source.bytes().unwrap(), encoded(&one));
        assert_eq!(first.bytes().unwrap(), encoded(&one[..4]));
        assert_eq!(last.bytes().unwrap(), encoded(&zero[..4]));
        if let Some(folder) = crate::capture::folder() {
            let saved = std::fs::read(folder.join("frame-7-tile-args-bounded-3.bin")).unwrap();
            assert_eq!(saved, encoded(&one[..4]));
        }
        println!(
            "Win64 D3D12: 120 reset records, 120 material records, full source, first bounded [180,1,0,0], reused same scratch [180,0,0,0]."
        );
    }
}
