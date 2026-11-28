//! Preserve an acceleration header produced under builder quarantine across
//! submissions. Only the verified producer and readers opt into this path.
//! The consumer's temporary bytes are restored before ordinary GPU writeback.
use super::*;
use std::collections::BTreeMap;

const CAP: u64 = 4 << 20;
const LAST: usize = 16;
const HEADER: usize = 48;
const WORD: usize = 4;
const PRODUCER: &str = "5552dac6a2b7a1fde2b70ca2477d749fbde1c4bc129e6a77aa81eda3c0a005a9";

pub(crate) fn is_producer(hash: &str) -> bool {
    hash == PRODUCER || hash == &PRODUCER[..16]
}
pub(crate) fn is_consumer(hash: &str) -> bool {
    hash.starts_with("6d7fe7745ab9c6d8") || hash.starts_with("f22a76fcb967ee59")
}

#[derive(Clone)]
struct Root {
    word: u32,
    node: u64,
    content: Vec<u8>,
}

static DONE: Mutex<BTreeMap<u64, Option<Root>>> = Mutex::new(BTreeMap::new());
static FLIGHT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
use std::sync::atomic::Ordering::Relaxed;

pub(crate) struct Capture {
    addr: u64,
    bytes: usize,
    gpu: ID3D12Resource,
}

impl Drop for Capture {
    fn drop(&mut self) {
        FLIGHT.fetch_sub(self.bytes as u64, Relaxed);
    }
}

impl Capture {
    pub(crate) fn record(list: &HostList, address: u64, source: &Arc<Shadow>) {
        if !MEMORY
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .is_some_and(|m| m.traced_views.contains(&address))
        {
            return;
        }
        let bytes = source.left(address).min(CAP);
        let Some(offset) = source.offset_in(address) else {
            return;
        };
        if bytes < 128
            || FLIGHT
                .fetch_update(Relaxed, Relaxed, |n| {
                    (n + bytes <= 64 << 20).then_some(n + bytes)
                })
                .is_err()
        {
            return;
        }
        let Some(gpu) = host().and_then(|h| h.read_back_buffer(bytes)) else {
            FLIGHT.fetch_sub(bytes, Relaxed);
            return;
        };
        let src = &source.buffer;
        unsafe {
            list.list.ResourceBarrier(&[crate::host::transition(
                src,
                D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                D3D12_RESOURCE_STATE_COPY_SOURCE,
            )]);
            list.list.CopyBufferRegion(&gpu, 0, src, offset, bytes);
            list.list.ResourceBarrier(&[crate::host::transition(
                src,
                D3D12_RESOURCE_STATE_COPY_SOURCE,
                D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
            )]);
        }
        list.keep(src);
        list.keep_produced_root(Self {
            addr: address,
            bytes: bytes as usize,
            gpu,
        });
    }
    pub(crate) fn landed(&self) {
        let mut ptr = std::ptr::null_mut();
        unsafe {
            if self
                .gpu
                .Map(
                    0,
                    Some(&D3D12_RANGE {
                        Begin: 0,
                        End: self.bytes,
                    }),
                    Some(&mut ptr),
                )
                .is_err()
                || ptr.is_null()
            {
                return;
            }
            let bytes = std::slice::from_raw_parts(ptr.cast::<u8>(), self.bytes);
            let word = u32::from_le_bytes(bytes[HEADER..HEADER + WORD].try_into().unwrap());
            let offset = u64::from(word & !7) * 8;
            let size = if word & 7 == 5 { 128 } else { 64 };
            let node = self.addr + offset;
            let valid = (1..0x8000_0000).contains(&word)
                && offset >= 64
                && offset + size <= self.bytes as u64;
            let result = valid.then(|| Root {
                word,
                node,
                content: bytes[offset as usize..(offset + size) as usize].to_vec(),
            });
            let mut done = DONE.lock().unwrap_or_else(|e| e.into_inner());
            if done.len() < LAST || done.contains_key(&self.addr) {
                let old = done.insert(self.addr, result);
                if old.as_ref().and_then(|v| v.as_ref()).map(|r| r.word) != Some(word) {
                    static LOGS: std::sync::atomic::AtomicU64 =
                        std::sync::atomic::AtomicU64::new(0);
                    if LOGS.fetch_add(1, Relaxed) < 64 {
                        tracing::info!(target:"victory_gpu::memory_watch", address=format_args!("{:#x}",self.addr), word=format_args!("{word:#x}"), node=format_args!("{node:#x}"), valid, source_bytes=self.bytes, "GPU constructor root at completion");
                    }
                }
            }
            self.gpu.Unmap(0, Some(&D3D12_RANGE::default()));
        }
    }
}

pub(super) fn node(address: u64) -> Option<u64> {
    DONE.lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&address)?
        .as_ref()
        .map(|r| r.node)
}

struct Range {
    target: Arc<Shadow>,
    dst: u64,
    scratch: u64,
    bytes: u64,
}
pub(crate) struct Overlay {
    undo: ID3D12Resource,
    ranges: Vec<Range>,
}

impl Overlay {
    pub(crate) fn bind(list: &HostList, address: u64) -> Option<Self> {
        let root = DONE
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&address)?
            .as_ref()?
            .clone();
        let all = MEMORY.lock().unwrap_or_else(|e| e.into_inner());
        let m = all.as_ref()?;
        let mut ranges = Vec::with_capacity(3);
        let mut data = root.word.to_le_bytes().to_vec();
        data.extend_from_slice(&root.content);
        for (start, n, base) in [
            (address + HEADER as u64, WORD, 0),
            (root.node, root.content.len(), WORD),
        ] {
            let mut copied = 0;
            while copied < n {
                let addr = start + copied as u64;
                let page = addr / PAGE;
                if page >= u64::from(MEMORY_PAGES - 1) {
                    return None;
                }
                let ptr = unsafe { m.directory.1.add(page as usize * 2) };
                let (number, offset) = unsafe { (ptr.read_volatile(), ptr.add(1).read_volatile()) };
                let target = m.copies.get(number.checked_sub(1)? as usize)?.clone();
                if !target.separate_from_title() {
                    return None;
                }
                let bytes = (n - copied).min((PAGE - addr % PAGE) as usize) as u64;
                let dst = u64::from(offset) + addr % PAGE;
                if dst + bytes > unsafe { target.buffer.GetDesc().Width } {
                    return None;
                }
                ranges.push(Range {
                    target,
                    dst,
                    scratch: (base + copied) as u64,
                    bytes,
                });
                copied += bytes as usize;
            }
        }
        drop(all);
        let h = host()?;
        let (undo, _) = h.shared_buffer(data.len() as u64)?;
        let (upload, mapped) = h.upload_buffer(data.len() as u64)?;
        unsafe { std::ptr::copy_nonoverlapping(data.as_ptr(), mapped, data.len()) };
        unsafe {
            list.list.ResourceBarrier(&[crate::host::transition(
                &undo,
                D3D12_RESOURCE_STATE_COMMON,
                D3D12_RESOURCE_STATE_COPY_DEST,
            )]);
            for r in &ranges {
                let buf = &r.target.buffer;
                list.list.ResourceBarrier(&[crate::host::transition(
                    buf,
                    D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                    D3D12_RESOURCE_STATE_COPY_SOURCE,
                )]);
                list.list
                    .CopyBufferRegion(&undo, r.scratch, buf, r.dst, r.bytes);
                list.list.ResourceBarrier(&[crate::host::transition(
                    buf,
                    D3D12_RESOURCE_STATE_COPY_SOURCE,
                    D3D12_RESOURCE_STATE_COPY_DEST,
                )]);
                list.list
                    .CopyBufferRegion(buf, r.dst, &upload, r.scratch, r.bytes);
                list.list.ResourceBarrier(&[crate::host::transition(
                    buf,
                    D3D12_RESOURCE_STATE_COPY_DEST,
                    D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                )]);
                list.keep(buf);
            }
            list.list.ResourceBarrier(&[crate::host::transition(
                &undo,
                D3D12_RESOURCE_STATE_COPY_DEST,
                D3D12_RESOURCE_STATE_COPY_SOURCE,
            )]);
        }
        list.keep(&upload);
        list.keep(&undo);
        Some(Self { undo, ranges })
    }
    pub(crate) fn finish(self, list: &HostList) {
        for r in self.ranges {
            let buf = &r.target.buffer;
            unsafe {
                list.list.ResourceBarrier(&[crate::host::transition(
                    buf,
                    D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                    D3D12_RESOURCE_STATE_COPY_DEST,
                )]);
                list.list
                    .CopyBufferRegion(buf, r.dst, &self.undo, r.scratch, r.bytes);
                list.list.ResourceBarrier(&[crate::host::transition(
                    buf,
                    D3D12_RESOURCE_STATE_COPY_DEST,
                    D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                )]);
            }
        }
    }
}
