//! Mirror the 64 bytes actually emitted by InitHeaders into the loose UAV
//! selected by the builder. The GPU compares the original request and every
//! output dword first, so an absent record or a stale page cannot match.
use super::*;

pub(crate) const HASH: &str = "3cea4534bbc15632";

fn kernel() -> Option<&'static (ID3D12RootSignature, ID3D12PipelineState)> {
    static PSO: std::sync::OnceLock<Option<(ID3D12RootSignature, ID3D12PipelineState)>> =
        std::sync::OnceLock::new();
    PSO.get_or_init(|| {
        let h = host()?;
        let arg = |kind, reg| D3D12_ROOT_PARAMETER {
            ParameterType: kind,
            Anonymous: D3D12_ROOT_PARAMETER_0 {
                Descriptor: D3D12_ROOT_DESCRIPTOR {
                    ShaderRegister: reg,
                    RegisterSpace: 0,
                },
            },
            ShaderVisibility: D3D12_SHADER_VISIBILITY_ALL,
        };
        let constants = D3D12_ROOT_PARAMETER {
            ParameterType: D3D12_ROOT_PARAMETER_TYPE_32BIT_CONSTANTS,
            Anonymous: D3D12_ROOT_PARAMETER_0 {
                Constants: D3D12_ROOT_CONSTANTS {
                    ShaderRegister: 0,
                    RegisterSpace: 0,
                    Num32BitValues: 3,
                },
            },
            ShaderVisibility: D3D12_SHADER_VISIBILITY_ALL,
        };
        let root = crate::over::root_signature(
            &h.device,
            &[
                arg(D3D12_ROOT_PARAMETER_TYPE_SRV, 0),
                arg(D3D12_ROOT_PARAMETER_TYPE_UAV, 0),
                arg(D3D12_ROOT_PARAMETER_TYPE_UAV, 1),
                constants,
            ],
            &[],
            D3D12_ROOT_SIGNATURE_FLAG_NONE,
        )
        .ok()?;
        let cs = include_bytes!("init_header.dxil");
        let pso = unsafe {
            h.device
                .CreateComputePipelineState(&D3D12_COMPUTE_PIPELINE_STATE_DESC {
                    pRootSignature: std::mem::transmute_copy(&root),
                    CS: D3D12_SHADER_BYTECODE {
                        pShaderBytecode: cs.as_ptr().cast(),
                        BytecodeLength: cs.len(),
                    },
                    ..Default::default()
                })
                .ok()?
        };
        Some((root, pso))
    })
    .as_ref()
}

/// Exactly the published raw directory page, after the guest initializer
/// has written it. The pointer is in that page, not in the CPU title.
pub(crate) fn targets() -> Vec<(u64, Arc<Shadow>, u64)> {
    let all = MEMORY.lock().unwrap_or_else(|e| e.into_inner());
    let Some(m) = all.as_ref() else { return vec![] };
    m.traced_views
        .iter()
        .filter_map(|&address| {
            let page = address / PAGE;
            if page >= u64::from(MEMORY_PAGES - 1) || address % PAGE > PAGE - 64 {
                return None;
            }
            let p = unsafe { m.directory.1.add(page as usize * 2) };
            let (n, offset) = unsafe { (p.read_volatile(), p.add(1).read_volatile()) };
            let backing = m.copies.get(n.checked_sub(1)? as usize)?.clone();
            let pos = u64::from(offset) + address % PAGE;
            if pos + 64 > unsafe { backing.buffer.GetDesc().Width } {
                return None;
            }
            Some((address, backing, pos))
        })
        .collect()
}

pub(crate) fn bridge(
    list: &HostList,
    jobs: &Arc<Shadow>,
    table: u64,
    count: u32,
    source: &Arc<Shadow>,
    offset: u64,
    dest: &Arc<Shadow>,
    address: u64,
) -> bool {
    // The helper's root descriptors are raw. Both title subranges and the
    // directory offset have to fit the underlying D3D12 buffers.
    let Some(entries) = jobs.offset_in(table) else {
        return false;
    };
    let Some(target) = dest.offset_in(address) else {
        return false;
    };
    if count == 0
        || count > 16384
        || jobs.left(table) < u64::from(count) * 40
        || dest.left(address) < 64
        || Arc::ptr_eq(source, dest)
        || !dest.separate_from_title()
        || (entries | offset | target) & 15 != 0
        || offset + 64 > unsafe { source.buffer.GetDesc().Width }
    {
        return false;
    }
    let Some((root, pso)) = kernel() else {
        return false;
    };
    let args = [address as u32, (address >> 32) as u32, count];
    unsafe {
        let l = &list.list;
        l.ResourceBarrier(&[crate::indirect::uav_barrier()]);
        l.SetComputeRootSignature(root);
        l.SetPipelineState(pso);
        l.SetComputeRootShaderResourceView(0, jobs.buffer.GetGPUVirtualAddress() + entries);
        l.SetComputeRootUnorderedAccessView(1, source.buffer.GetGPUVirtualAddress() + offset);
        l.SetComputeRootUnorderedAccessView(2, dest.buffer.GetGPUVirtualAddress() + target);
        l.SetComputeRoot32BitConstants(3, 3, args.as_ptr().cast(), 0);
        l.Dispatch(1, 1, 1);
        l.ResourceBarrier(&[crate::indirect::uav_barrier()]);
    }
    Report::record(
        list, jobs, entries, count, source, offset, dest, target, address,
    );
    for r in [&jobs.buffer, &source.buffer, &dest.buffer] {
        list.keep(r);
    }
    list.keep(root);
    list.keep(pso);
    static LOGS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    if LOGS.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 64 {
        tracing::info!(target:"victory_gpu::memory_watch",address=format_args!("{address:#x}"),table=format_args!("{table:#x}"),count,raw=source.id(),uav=dest.id(),"GPU verifies initializer request and header before builder");
    }
    true
}

/// After the helper, capture the actual GPU inputs and both 64-byte outputs.
/// Only the diagnostics flag performs readback; the helper never needs it.
pub(crate) struct Report {
    frame: u64,
    address: u64,
    raw: u64,
    dest: u64,
    count: usize,
    data: ID3D12Resource,
}

impl Report {
    fn record(
        list: &HostList,
        jobs: &Arc<Shadow>,
        entries: u64,
        count: u32,
        source: &Arc<Shadow>,
        offset: u64,
        dest: &Arc<Shadow>,
        target: u64,
        address: u64,
    ) {
        static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        static SAMPLES: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        if !*ON
            .get_or_init(|| std::env::var_os("VICTORY_TRACE_AS_HEADER").is_some_and(|v| v == "1"))
            || count > 256
            || SAMPLES.fetch_add(1, std::sync::atomic::Ordering::Relaxed) >= 96
        {
            return;
        }
        let job_bytes = u64::from(count) * 40;
        let len = 128 + job_bytes;
        let Some(data) = host().and_then(|h| h.read_back_buffer(len)) else {
            return;
        };
        unsafe {
            let l = &list.list;
            for (buf, at, to, bytes, state) in [
                (
                    &source.buffer,
                    offset,
                    0,
                    64,
                    D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                ),
                (
                    &dest.buffer,
                    target,
                    64,
                    64,
                    D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                ),
                (
                    &jobs.buffer,
                    entries,
                    128,
                    job_bytes,
                    D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE,
                ),
            ] {
                l.ResourceBarrier(&[crate::host::transition(
                    buf,
                    state,
                    D3D12_RESOURCE_STATE_COPY_SOURCE,
                )]);
                l.CopyBufferRegion(&data, to, buf, at, bytes);
                l.ResourceBarrier(&[crate::host::transition(
                    buf,
                    D3D12_RESOURCE_STATE_COPY_SOURCE,
                    state,
                )]);
            }
        }
        list.keep_init_report(Self {
            frame: crate::queue::presented() + 1,
            address,
            raw: source.id(),
            dest: dest.id(),
            count: count as usize,
            data,
        });
    }

    pub(crate) fn landed(&self) {
        let mut ptr = std::ptr::null_mut();
        let n = 128 + self.count * 40;
        unsafe {
            if self
                .data
                .Map(0, Some(&D3D12_RANGE { Begin: 0, End: n }), Some(&mut ptr))
                .is_err()
                || ptr.is_null()
            {
                return;
            }
            let bytes = std::slice::from_raw_parts(ptr.cast::<u8>(), n);
            let mut matching = None;
            for record in bytes[128..].chunks_exact(40) {
                let words: [u32; 10] = std::array::from_fn(|i| {
                    u32::from_le_bytes(record[4 * i..4 * i + 4].try_into().unwrap())
                });
                if u64::from(words[0]) | u64::from(words[1]) << 32 == self.address {
                    matching = Some(words);
                    break;
                }
            }
            if let Some(record) = matching {
                let src: [u32; 16] = std::array::from_fn(|i| {
                    u32::from_le_bytes(bytes[4 * i..4 * i + 4].try_into().unwrap())
                });
                let dst: [u32; 16] = std::array::from_fn(|i| {
                    u32::from_le_bytes(bytes[64 + 4 * i..68 + 4 * i].try_into().unwrap())
                });
                let expected = [
                    record[4],
                    0,
                    record[5],
                    record[6],
                    0x7f7fffff,
                    0x7f7fffff,
                    0x7f7fffff,
                    0xff7fffff,
                    0xff7fffff,
                    0xff7fffff,
                    0,
                    record[7],
                    u32::MAX,
                    record[8],
                    record[9],
                    0x007f7f7f,
                ];
                tracing::info!(target:"victory_gpu::memory_watch",frame=self.frame,address=format_args!("{:#x}",self.address),
                    raw=self.raw, destination=self.dest, count=self.count, record=format_args!("{record:08x?}"),
                    source_matches=src==expected,target_matches=dst==expected,raw_header=format_args!("{src:08x?}"),
                    builder_header=format_args!("{dst:08x?}"),"initializer request and both GPU headers after guarded bridge");
                #[cfg(test)]
                eprintln!(
                    "initializer fenced GPU evidence: source_matches={} target_matches={} leaves={} actual_count={}",
                    src == expected,
                    dst == expected,
                    expected[3],
                    dst[3]
                );
            } else {
                static FIRST: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
                if FIRST.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 8 {
                    tracing::info!(target:"victory_gpu::memory_watch",frame=self.frame,address=format_args!("{:#x}",self.address), count=self.count,
                        "initializer GPU inputs contain other destinations");
                }
            }
            self.data.Unmap(0, Some(&D3D12_RANGE::default()));
        }
    }
}
