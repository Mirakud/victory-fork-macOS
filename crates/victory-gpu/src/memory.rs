//! Title memory by address, for translated shaders that read and write
//! through raw 40-bit addresses (`victory_shader::emit::SLOT_MEMORY`: the
//! console's ray-tracing structure builder and the programs that walk its
//! structures; the structures themselves lie in plain title memory, in no
//! buffer object).
//!
//! The copies are the ones every other access uses ([`crate::shadow`]: a
//! title buffer's copy, or the 4 MiB window of memory outside buffers), so
//! a root view and a raw address reach the same host bytes. This module
//! keeps what a shader needs to find them:
//!
//! - the **directory**: per 64 KiB page of title address space the number
//!   of the copy that holds the page (0: none) and the page's byte offset
//!   in it; one buffer for all lists, entries only ever go from empty to
//!   filled;
//! - per view heap a run of slots with a raw unordered access view of each
//!   numbered copy (slot 0 of the run: a null view, for pages without a
//!   copy);
//! - the **faults**: the page numbers shaders found empty, read back here
//!   once a frame, so the copies exist from the next frame on. A pointer a
//!   shader takes out of memory cannot be known before it runs; the
//!   addresses a dispatch is given as root views are made ready before it.
//!
//! **A page that is not there when a shader touches it is a lost access**:
//! the read gives 0, the store is dropped, and the fault note only helps
//! the next frame. For the structure builder that is the same as a dropped
//! stage, so before a dispatch the host makes ready everything it can see
//! the dispatch name: the whole copy around each root view, around each
//! pair of root constants that is a title address and around each root
//! constant that is one in 256-byte units (the builder passes structures
//! all three ways), and the 4 MiB after each (a structure may cross a
//! window's end).
//! shortcut: what a stage reaches only through pointers it reads out of
//! memory (InitHeaders' table of structure addresses, LoadInstances'
//! instance records) is not made ready unless something else named it;
//! the fault count in the log says whether that happens. The whole answer
//! is the extent of the title's allocation around an address, which the
//! memory module of the layer knows and this one does not ask yet.
//!
//! A numbered copy is kept for the life of the directory. After the
//! speculative budget, only pages demanded by GPU faults or by shader-
//! declared view addresses can be admitted. Each such page adds 64 KiB of refresh and possible writeback.
//! Both the number of those pages and the total size of the additional
//! retained resources are capped.

#[cfg(feature = "diagnostics")]
pub(crate) mod produced;
#[cfg(feature = "diagnostics")]
pub(crate) mod initialized;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use victory_shader::emit::{MEMORY_FAULTS, MEMORY_PAGES};
use windows::Win32::Graphics::Direct3D12::*;

use crate::descriptor::host::HostHeap;
use crate::host::{HostList, host};
use crate::shadow::Shadow;

/// Copies that can be numbered (the run of heap slots is one longer). A
/// listed frame of the opening cutscene names about 50 windows.
const COPIES: u32 = 1023;
const FAULT_PAGES: usize = 2048;
const DEFAULT_FAULT_MIB: u64 = 256;
// A newly bound traversal root must still fit after incidental faults fill the cache.
const ROOT_PAGES: usize = 64;
const ROOT_BYTES: u64 = 64 << 20;
/// A window of memory outside buffers (`shadow.rs`).
const CELL: u64 = 4 << 20;
/// A directory page.
const PAGE: u64 = 1 << 16;

/// A directory entry has a 32-bit byte offset. Every byte in its page
/// must be addressable by the shader's 32-bit addition.
fn page_offset(offset: u64) -> Option<u32> {
    (offset <= (1u64 << 32) - PAGE).then_some(offset as u32)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PageRequest {
    Hint,
    GpuFault,
    BoundView,
}

impl PageRequest {
    fn limits(self, pages: usize, bytes: u64) -> (usize, u64) {
        if self == Self::GpuFault {
            (pages.saturating_sub(ROOT_PAGES), bytes.saturating_sub(ROOT_BYTES))
        } else {
            (pages, bytes)
        }
    }
}

struct Memory {
    /// Buffer, its mapping, its GPU address.
    directory: (ID3D12Resource, *mut u32, u64),
    faults: (ID3D12Resource, *mut u32, u64),
    /// Copy `n` is `copies[n - 1]`.
    copies: Vec<Arc<Shadow>>,
    /// Host range to synchronise, indexed by an append-only list. A fault
    /// reserve entry spans only the page a shader actually touched.
    ranges: Vec<(usize, u64, u64)>,
    numbers: HashMap<usize, usize>,
    fault_capacity: usize,
    fault_budget: u64,
    fault_pages: usize,
    fault_bytes: u64,
    #[cfg(feature = "diagnostics")]
    traced_views: std::collections::HashSet<u64>,
    /// Per host view heap (by address): the first slot of its run, and how
    /// many copies have their view there.
    heaps: HashMap<usize, (u32, u32)>,
    /// The frame the faults were last read in.
    frame: u64,
    /// Pages asked for this frame that are not the title's memory.
    refused: std::collections::HashSet<u64>,
}

// SAFETY: the mappings are of buffers the struct owns; all access is under
// the mutex.
unsafe impl Send for Memory {}

static MEMORY: Mutex<Option<Memory>> = Mutex::new(None);

/// What a draw binds.
pub(crate) struct Ready {
    pub(crate) directory: u64,
    pub(crate) faults: u64,
    /// The view heap slot of the null view; copy `n`'s view is `n` further.
    pub(crate) base: u32,
}

impl Memory {
    fn new() -> Option<Memory> {
        let host = host()?;
        let make = |dwords: u64, what: &str| {
            let (buffer, data) = host.shared_buffer(dwords * 4)?;
            crate::host::name(&buffer, || format!("title memory by address: {what}"));
            // SAFETY: a fresh mapping of at least that many bytes.
            unsafe { std::ptr::write_bytes(data, 0, (dwords * 4) as usize) };
            // SAFETY: a live buffer.
            let gpu = unsafe { buffer.GetGPUVirtualAddress() };
            Some((buffer, data.cast::<u32>(), gpu))
        };
        tracing::info!("title memory by address: a shader reads or writes through raw addresses");
        Some(Memory {
            directory: make(u64::from(MEMORY_PAGES) * 2, "directory")?,
            faults: make(u64::from(MEMORY_FAULTS), "faults")?,
            copies: Vec::new(),
            ranges: Vec::new(),
            numbers: HashMap::new(),
            fault_capacity: if std::env::var_os("VICTORY_BVH_BUILDER").is_some_and(|v| v == "1") {
                FAULT_PAGES
            } else { 0 },
            fault_budget: std::env::var("VICTORY_BVH_PAGE_CACHE_MB").ok()
                .and_then(|mb| mb.parse::<u64>().ok())
                .unwrap_or(DEFAULT_FAULT_MIB).clamp(128, 2048) << 20,
            fault_pages: 0,
            fault_bytes: 0,
            #[cfg(feature = "diagnostics")]
            traced_views: Default::default(),
            heaps: HashMap::new(),
            frame: u64::MAX,
            refused: Default::default(),
        })
    }

    /// Gives the page of `address` its copy, if it has none and the memory
    /// is the title's.
    fn map(&mut self, address: u64, request: PageRequest) {
        let page = address / PAGE;
        // The last entry stays empty: addresses past the directory read it.
        if page >= u64::from(MEMORY_PAGES) - 1 {
            return;
        }
        // SAFETY: inside the directory's mapping.
        let entry = unsafe { self.directory.1.add(page as usize * 2) };
        // SAFETY: as above.
        if unsafe { entry.read() } != 0 {
            return;
        }
        let reserve = self.copies.len() >= COPIES as usize;
        if reserve && request == PageRequest::Hint {
            return;
        }
        // A shader-declared view address is exact even before the first
        // fault. Only that view's 64 KiB page is required before dispatch.
        let exact = reserve || (request == PageRequest::BoundView && self.fault_capacity != 0);
        let (max_pages, max_bytes) = request.limits(self.fault_capacity, self.fault_budget);
        if exact && self.fault_pages >= max_pages {
            static SAID: std::sync::Once = std::sync::Once::new();
            SAID.call_once(|| tracing::warn!(views = self.copies.len(), fault_pages = self.fault_pages,
                "raw-address GPU fault page limit reached; new missing pages read zero"));
            return;
        }
        let start = page * PAGE;
        // A shader's pointer, or a constant taken for an address, may be
        // anything: only memory the title has.
        if self.refused.contains(&page) {
            return;
        }
        if !crate::texture::committed(start as *const u8, PAGE as usize) {
            self.refused.insert(page);
            return;
        }
        // The copy a root view at that address would get; a buffer that
        // ends inside the page is passed over for what holds all of it.
        let whole = |s: &Arc<Shadow>| s.offset_in(start).is_some_and(|_| s.left(start) >= PAGE);
        let shadow = if exact {
            // Reuse a buffer, or a window already holding the page. A new
            // fine-grid window need only hold 64 KiB, not a whole 4 MiB tail.
            crate::shadow::find_any(start, PAGE).filter(whole)
        } else {
            crate::shadow::find(start, 1).filter(whole)
                .or_else(|| crate::shadow::find(start, PAGE).filter(whole))
        };
        let Some(shadow) = shadow else { return };
        let Some(offset) = shadow.offset_in(start).and_then(page_offset) else { return };
        // SAFETY: the shadow owns its live host buffer.
        let desc = unsafe { shadow.buffer.GetDesc() };
        if desc.Flags.0 & D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS.0 == 0 {
            return;
        }
        let key = Arc::as_ptr(&shadow) as usize;
        let n = match self.numbers.get(&key) {
            Some(&n) => n,
            None => {
                if exact {
                    // Include the staging resource of a device-local copy.
                    let bytes = desc.Width.saturating_mul(if shadow.carded() { 2 } else { 1 });
                    if bytes > max_bytes.saturating_sub(self.fault_bytes) {
                        static SAID: std::sync::Once = std::sync::Once::new();
                        SAID.call_once(|| tracing::warn!(bytes, max_bytes, ?request, retained_bytes = self.fault_bytes,
                            "raw-address fault resource exceeds the remaining reserve"));
                        return;
                    }
                    self.fault_bytes += bytes;
                }
                let n = self.copies.len();
                self.copies.push(shadow.clone());
                self.numbers.insert(key, n);
                n
            }
        };
        if exact {
            self.ranges.push((n, start, PAGE));
            self.fault_pages += 1;
            if self.fault_pages.is_power_of_two() {
                tracing::info!(target: "victory_gpu::memory_watch", pages = self.fault_pages,
                    views = self.copies.len(), retained_bytes = self.fault_bytes,
                    ?request, "raw-address pages admitted on demand");
            }
        } else {
            let (first, len) = shadow.span();
            self.ranges.push((n, first, len));
            let mut at = first.next_multiple_of(PAGE);
            while at + PAGE <= first + len && at / PAGE < u64::from(MEMORY_PAGES) - 1 {
                let Some(other_offset) = page_offset(at - first) else { break };
                // SAFETY: inside the mapped directory. Publish offset before number.
                let other = unsafe { self.directory.1.add((at / PAGE) as usize * 2) };
                unsafe {
                    if other.read() == 0 {
                        other.add(1).write_volatile(other_offset);
                        other.write_volatile(n as u32 + 1);
                    }
                }
                at += PAGE;
            }
        }
        // The offset first: a shader that sees the number sees it too.
        // SAFETY: inside the directory's mapping.
        unsafe {
            entry.add(1).write_volatile(offset);
            entry.write_volatile(n as u32 + 1);
        }
    }

    /// A GPU write to a sub-page title buffer and a later BVH read can
    /// share a guest address but use distinct host resources. Carry only
    /// the AS bytes produced earlier on this list into the directory UAV.
    fn sync_bound_alias(&self, list: &HostList, address: u64) {
        let page = address / PAGE;
        if address == 0 || page >= u64::from(MEMORY_PAGES - 1) { return; }
        let Some(producer) = crate::shadow::existing_buffer(address) else {
            #[cfg(feature = "diagnostics")]
            trace_as_alias(address, None, None, "no-existing-buffer", 0);
            return;
        };
        // SAFETY: checked directory bounds; the Memory lock is held.
        let number = unsafe { self.directory.1.add(page as usize * 2).read_volatile() };
        let Some(window) = number.checked_sub(1).and_then(|n| self.copies.get(n as usize)) else {
            #[cfg(feature = "diagnostics")]
            trace_as_alias(address, Some(&producer), None, "unmapped", 0);
            return;
        };
        if producer.buffer == window.buffer {
            #[cfg(feature = "diagnostics")]
            trace_as_alias(address, Some(&producer), Some(window), "same-buffer", 0);
            return;
        }
        let page_end = (page + 1) * PAGE;
        let maximum = producer.left(address).min(window.left(address)).min(page_end - address);
        let bytes = list.as_source_bytes(&producer, window, address as usize, maximum as usize) as u64;
        if bytes == 0 {
            #[cfg(feature = "diagnostics")]
            trace_as_alias(address, Some(&producer), Some(window), "no-earlier-source-write", 0);
            return;
        }
        let (Some(src), Some(dst)) = (producer.offset_in(address), window.offset_in(address)) else { return };
        let uav = D3D12_RESOURCE_STATE_UNORDERED_ACCESS;
        let read = D3D12_RESOURCE_STATE_COPY_SOURCE;
        let write = D3D12_RESOURCE_STATE_COPY_DEST;
        use crate::host::transition;
        // Both resources are UAVs in the raw builder/reader. The source
        // was written earlier on this list. A new buffer can promote from
        // COMMON to UAV. Leave both UAVs in their state for the next use.
        unsafe {
            list.list.ResourceBarrier(&[
                transition(&producer.buffer, uav, read), transition(&window.buffer, uav, write),
            ]);
            list.list.CopyBufferRegion(&window.buffer, dst, &producer.buffer, src, bytes);
            list.list.ResourceBarrier(&[
                transition(&producer.buffer, read, uav), transition(&window.buffer, write, uav),
            ]);
        }
        list.keep(&producer.buffer);
        list.keep(&window.buffer);
        #[cfg(feature = "diagnostics")]
        trace_as_alias(address, Some(&producer), Some(window), "copied", bytes);
    }

    /// The pages shaders found empty since the last frame.
    fn take_faults(&mut self) {
        let mut pages = Vec::new();
        for i in 1..MEMORY_FAULTS as usize {
            // SAFETY: inside the faults' mapping; the GPU may be writing,
            // a number missed now is written again next frame.
            let at = unsafe { self.faults.1.add(i) };
            // SAFETY: as above.
            let page = unsafe { at.read_volatile() };
            if page != 0 {
                pages.push(page);
                // SAFETY: as above.
                unsafe { at.write_volatile(0) };
            }
        }
        if !pages.is_empty() {
            // Each is an access that read 0 or was dropped.
            static SAID: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            if SAID.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 50 {
                tracing::warn!(
                    pages = pages.len(),
                    first = format_args!("{:#x}", u64::from(pages[0]) * PAGE),
                    "title memory by address: shaders touched pages that had no copy (those accesses read 0 or were dropped); the copies are made now"
                );
            }
        }
        for page in pages {
            self.map(u64::from(page) * PAGE, PageRequest::GpuFault);
        }
    }

    /// What a dispatch names: the copy around `address` and the 4 MiB
    /// after it.
    fn name(&mut self, address: u64) {
        self.map(address, PageRequest::Hint);
        self.map((address & !(CELL - 1)) + CELL, PageRequest::Hint);
    }
}

#[cfg(feature = "diagnostics")]
fn trace_as_alias(address: u64, src: Option<&Arc<Shadow>>, dst: Option<&Arc<Shadow>>, status: &'static str, bytes: u64) {
    static SEEN: std::sync::Mutex<Vec<(u64, &'static str)>> = std::sync::Mutex::new(Vec::new());
    let mut seen = SEEN.lock().unwrap_or_else(|e| e.into_inner());
    if seen.len() < 64 && !seen.contains(&(address, status)) {
        seen.push((address, status));
        tracing::info!(target: "victory_gpu::memory_watch", address = format_args!("{address:#x}"),
            source = ?src.map(|s| (s.id(), s.span())), directory = ?dst.map(|s| (s.id(), s.span())),
            status, bytes, "acceleration structure producer and directory alias");
    }
}

/// A GPU copy of the exact bytes selected by the shader's page entry.
#[cfg(feature = "diagnostics")]
pub(crate) struct HeaderProbe {
    frame: u64,
    address: u64,
    window: u64,
    readback: ID3D12Resource,
}

#[cfg(feature = "diagnostics")]
impl HeaderProbe {
    fn record(memory: &Memory, list: &HostList, address: u64) {
        static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        if !*ON.get_or_init(|| std::env::var_os("VICTORY_TRACE_AS_HEADER").is_some_and(|v| v == "1")) { return; }
        let page = address / PAGE;
        if page >= u64::from(MEMORY_PAGES-1) || address % PAGE + 256 > PAGE { return; }
        let ptr = unsafe { memory.directory.1.add(page as usize * 2) };
        let (number, offset) = unsafe { (ptr.read_volatile(), ptr.add(1).read_volatile()) };
        let Some(window) = number.checked_sub(1).and_then(|n| memory.copies.get(n as usize)) else { return };
        // At most four samples per address and eight distinct AS roots in
        // a diagnostic run. Space observations to include the first update.
        let frame = crate::queue::presented() + 1;
        static VISITS: Mutex<Vec<(u64, u64, u32)>> = Mutex::new(Vec::new());
        {
            let mut visits = VISITS.lock().unwrap_or_else(|e| e.into_inner());
            let has_room = visits.len() < 8;
            match visits.iter_mut().find(|v| v.0 == address) {
                Some(v) if v.2 < 4 && frame >= v.1 + [0, 2, 16, 64][v.2 as usize] => { v.1 = frame; v.2 += 1; }
                Some(_) => return,
                None if has_room => visits.push((address,frame,1)),
                None => return,
            }
        }
        let Some(back) = host().and_then(|h| h.read_back_buffer(256)) else { return };
        let byte = u64::from(offset) + address % PAGE;
        // Use the numerical directory offset, the same one the shader
        // adds to its intra-page byte. Never infer it from the title VA.
        if byte + 256 > unsafe { window.buffer.GetDesc().Width } { return; }
        let read = D3D12_RESOURCE_STATE_COPY_SOURCE;
        let uav = D3D12_RESOURCE_STATE_UNORDERED_ACCESS;
        unsafe {
            list.list.ResourceBarrier(&[crate::host::transition(&window.buffer, uav, read)]);
            list.list.CopyBufferRegion(&back, 0, &window.buffer, byte, 256);
            list.list.ResourceBarrier(&[crate::host::transition(&window.buffer, read, uav)]);
        }
        list.keep(&window.buffer);
        let id = window.id();
        let overlaps = crate::shadow::as_header_windows(address,256);
        list.defer_host(move || {
            let mut guest = [0u8;256];
            let valid = crate::host::read_into(address as *const u8, &mut guest);
            let words: [u32;64] = std::array::from_fn(|i| u32::from_le_bytes(guest[i*4..i*4+4].try_into().unwrap()));
            tracing::info!(target:"victory_gpu::memory_watch", frame, address=format_args!("{address:#x}"), directory=id,
                number, byte, valid, words=format_args!("{words:08x?}"), "AS header guest before GPU submission");
            for other in &overlaps {
                let words = crate::shadow::as_host_header(other,address);
                tracing::info!(target:"victory_gpu::memory_watch", frame, address=format_args!("{address:#x}"), id=other.id(),
                    span=?other.span(), card=other.carded(), is_directory=(other.id()==id), words=format_args!("{words:08x?}"),
                    "AS header overlapping window CPU mapping at submission");
            }
        });
        list.keep_as_header(Self { frame, address, window:id, readback:back });
    }

    pub(crate) fn landed(&self) {
        let mut data = std::ptr::null_mut();
        unsafe {
            if self.readback.Map(0,Some(&D3D12_RANGE { Begin:0,End:256 }),Some(&mut data)).is_err() || data.is_null() { return; }
            let words = data.cast::<[u32;64]>().read_unaligned();
            self.readback.Unmap(0,Some(&D3D12_RANGE::default()));
            #[cfg(test)]
            eprintln!("AS header GPU readback {:#x} dword5={:08x}", self.address, words[5]);
            tracing::info!(target:"victory_gpu::memory_watch", frame=self.frame, address=format_args!("{:#x}",self.address), id=self.window,
                words=format_args!("{words:08x?}"), "AS header from GPU directory before trace dispatch");
        }
    }
}

/// Before a draw whose shader reads title memory by address: what its
/// root arguments name gets its copies (`addresses`: its root views;
/// `constants`: its root constants, see the module), every numbered copy
/// has its view in `heap`'s run and is brought up to date when `list` is
/// submitted (and, with `writes`, copied back after). None if the host
/// cannot hold it; the draw is then dropped.
pub(crate) fn ready(
    list: &HostList,
    heap: &HostHeap,
    addresses: &[u64],
    constants: &[u32],
    confirmed_views: &[u64],
    writes: bool,
) -> Option<Ready> {
    let mut guard = MEMORY.lock().unwrap_or_else(|e| e.into_inner());
    if guard.is_none() {
        *guard = Memory::new();
    }
    let m = guard.as_mut()?;
    let frame = crate::frame::current();
    let new_frame = m.frame != frame;
    if new_frame {
        m.frame = frame;
        m.refused.clear();
    }
    for &address in confirmed_views.iter().filter(|&&a| a != 0) {
        m.map(address, PageRequest::BoundView);
        #[cfg(feature = "diagnostics")]
        if let Some(node)=produced::node(address) { m.map(node,PageRequest::BoundView);m.map(node+127,PageRequest::BoundView); }
        #[cfg(feature = "diagnostics")]
        if address / PAGE < u64::from(MEMORY_PAGES - 1)
            && m.traced_views.len() < 16 && m.traced_views.insert(address)
        {
            // SAFETY: a valid directory index, under the directory lock.
            let entry = unsafe { m.directory.1.add((address / PAGE * 2) as usize) };
            let (number, offset) = unsafe { (entry.read_volatile(), entry.add(1).read_volatile()) };
            tracing::info!(target: "victory_gpu::memory_watch",
                address = format_args!("{address:#x}"), number, offset,
                "raw-address shader view prepared before dispatch");
        }
    }
    if new_frame {
        m.take_faults();
    }
    for &address in addresses {
        m.name(address);
    }
    // A title address in two constants (low dword, then bits 32 to 39), or
    // in one in 256-byte units. A constant that only looks like one names
    // memory nothing reads; one that is not the title's memory is refused.
    for (i, &c) in constants.iter().enumerate() {
        if let Some(&high) = constants.get(i + 1)
            && (1..=0xFF).contains(&high)
            && c >= 0x1_0000
        {
            m.name(u64::from(c) | u64::from(high) << 32);
        }
        if c >= 0x0100_0000 {
            m.name(u64::from(c) << 8);
        }
    }
    let key = std::ptr::from_ref(heap) as usize;
    let (base, made) = match m.heaps.get(&key) {
        Some(&run) => run,
        None => {
            // Held for the heap's life: not given back with a list's slots.
            let base = heap.dynamic(COPIES + m.fault_capacity as u32 + 1)?;
            heap.null_raw_uav_at(base);
            (base, 0)
        }
    };
    for (n, shadow) in m.copies.iter().enumerate().skip(made as usize) {
        let (_, len) = shadow.span();
        heap.raw_uav_at(base + 1 + n as u32, &shadow.buffer, 0, len);
    }
    m.heaps.insert(key, (base, m.copies.len() as u32));
    // Run before declaring any writes by the current shader: only its
    // predecessors can have produced the AS that this shader reads.
    for (i, &address) in confirmed_views.iter().enumerate() {
        if !confirmed_views[..i].contains(&address) {
            m.sync_bound_alias(list, address);
            #[cfg(feature = "diagnostics")]
            HeaderProbe::record(m, list, address);
        }
    }
    // Include ranges published by other command lists since this one
    // last bound the directory. Read and write progress are independent.
    let (read, write, first) = list.raw_memory_ranges(m.ranges.len(), writes);
    for &(n, start, len) in &m.ranges[read..] {
        m.copies[n].refresh(list, start, len);
    }
    if writes {
        for &(n, start, len) in &m.ranges[write..] {
            m.copies[n].gpu_writes(list, start, len);
        }
    }
    if first {
        list.keep(&m.directory.0);
        list.keep(&m.faults.0);
    }
    Some(Ready {
        directory: m.directory.2,
        faults: m.faults.2,
        base,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc, VirtualFree,
    };

    #[test]
    fn later_tracing_roots_have_capacity_after_ordinary_faults() {
        let budget = 768 << 20;
        assert_eq!(PageRequest::GpuFault.limits(2048, budget), (1984, 704 << 20));
        assert_eq!(PageRequest::BoundView.limits(2048, budget), (2048, budget));
        assert_eq!(PageRequest::GpuFault.limits(0, 0), (0, 0));
    }

    #[test]
    fn offsets_cover_the_whole_directory_page() {
        assert_eq!(page_offset(0), Some(0));
        assert_eq!(page_offset((1 << 32) - PAGE), Some(0xffff0000));
        assert_eq!(page_offset((1 << 32) - PAGE + 1), None);
        assert_eq!(page_offset(1 << 32), None);
    }

    #[test]
    #[ignore = "requires D3D12 with a shader-visible portable descriptor heap"]
    fn gpu_reads_faulted_pages_after_1023_speculative_copies() {
        let host = host().expect("D3D12 device");
        let heap = HostHeap::new(false, 4).expect("shader-visible descriptor heap");
        let len = (u64::from(COPIES) + 5) * PAGE;
        let start = [0x9_0000_0000u64, 0xb_0000_0000, 0xd_0000_0000]
            .into_iter()
            .find_map(|at| {
                // SAFETY: ask Windows for a fresh test allocation in the guest address space.
                let ptr = unsafe {
                    VirtualAlloc(
                        at as *const _,
                        len as usize,
                        MEM_COMMIT | MEM_RESERVE,
                        PAGE_READWRITE,
                    )
                };
                (!ptr.is_null()).then_some(ptr as u64)
            })
            .expect("guest virtual address range");
        let extra = start + u64::from(COPIES) * PAGE;
        let guest = [extra + 20, extra + PAGE + 40];
        for (at, val) in guest.into_iter().zip([0x5ea2_10abu32, 0xd157_c012]) {
            // SAFETY: the test owns and committed these aligned words.
            unsafe { (at as *mut u32).write(val) };
        }
        unsafe {
            ((guest[0]+8) as *mut u32).write(21);
            ((guest[0]+12) as *mut u32).write(0xa15e_a5ed);
        }
        let mut memory = Memory::new().unwrap();
        for i in 0..COPIES {
            let at = start + u64::from(i) * PAGE;
            crate::shadow::register(at as usize, at, PAGE);
            memory.map(at, PageRequest::Hint);
        }
        assert_eq!(memory.copies.len(), COPIES as usize);
        crate::shadow::register(extra as usize, extra, 2 * PAGE);
        memory.map(extra, PageRequest::Hint);
        assert_eq!(memory.copies.len(), COPIES as usize);
        let entry = |m: &Memory, at: u64| {
            // SAFETY: both GPU directory words for this guest page are within its mapping.
            let ptr = unsafe { m.directory.1.add((at / PAGE) as usize * 2) };
            unsafe { (ptr.read_volatile(), ptr.add(1).read_volatile()) }
        };
        assert_eq!(entry(&memory, extra), (0, 0));
        assert_eq!(entry(&memory, guest[1]), (0, 0));
        let final_page = u64::from(MEMORY_PAGES - 1);
        for page in [final_page, final_page + 1] {
            memory.map(page * PAGE, PageRequest::GpuFault);
        }
        assert_eq!(entry(&memory, final_page * PAGE), (0, 0));
        let reserve = memory.fault_capacity > 0;
        *MEMORY.lock().unwrap() = Some(memory);
        let root_arg = |kind, reg| D3D12_ROOT_PARAMETER {
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
                    Num32BitValues: 4,
                },
            },
            ShaderVisibility: D3D12_SHADER_VISIBILITY_ALL,
        };
        let root = crate::over::root_signature(
            &host.device,
            &[
                root_arg(D3D12_ROOT_PARAMETER_TYPE_SRV, 0),
                root_arg(D3D12_ROOT_PARAMETER_TYPE_UAV, 0),
                root_arg(D3D12_ROOT_PARAMETER_TYPE_UAV, 1),
                constants,
            ],
            &[],
            D3D12_ROOT_SIGNATURE_FLAG_CBV_SRV_UAV_HEAP_DIRECTLY_INDEXED,
        )
        .unwrap();
        let cs = include_bytes!("../tests/shaders/raw_fault_pages.dxil");
        let (out, output) = host.shared_buffer(8).unwrap();
        // SAFETY: both shader, root, buffers and heap remain live until the synchronous readback.
        let pso: ID3D12PipelineState = unsafe {
            host.device
                .CreateComputePipelineState(&D3D12_COMPUTE_PIPELINE_STATE_DESC {
                    pRootSignature: std::mem::transmute_copy(&root),
                    CS: D3D12_SHADER_BYTECODE {
                        pShaderBytecode: cs.as_ptr().cast(),
                        BytecodeLength: cs.len(),
                    },
                    ..Default::default()
                })
                .unwrap()
        };
        let issue = |list: &HostList, bound: &Ready, reads: [u64; 2]| {
            unsafe {
                list.list.SetDescriptorHeaps(&[Some(heap.heap.clone())]);
                list.list.SetComputeRootSignature(&root);
                list.list.SetPipelineState(&pso);
                list.list
                    .SetComputeRootShaderResourceView(0, bound.directory);
                list.list
                    .SetComputeRootUnorderedAccessView(1, out.GetGPUVirtualAddress());
                list.list.SetComputeRootUnorderedAccessView(2, bound.faults);
                for (i, at) in reads.into_iter().enumerate() {
                    let args = [
                        (at / PAGE) as u32,
                        (at % PAGE) as u32,
                        bound.base,
                        i as u32 * 4,
                    ];
                    list.list
                        .SetComputeRoot32BitConstants(3, 4, args.as_ptr().cast(), 0);
                    list.list.Dispatch(1, 1, 1);
                }
            }
        };
        let dispatch = |list: &HostList, bound: &Ready, reads: [u64; 2]| {
            issue(list,bound,reads);
            list.close();
            host.execute(&[list]);
            unsafe { output.cast::<[u32; 2]>().read_volatile() }
        };
        let first = host.new_list().unwrap();
        let bound = ready(&first, &heap, &[], &[], &[], false).unwrap();
        let last_reserved = bound.base + COPIES + if reserve { FAULT_PAGES as u32 } else { 0 };
        assert!(last_reserved < unsafe { heap.heap.GetDesc().NumDescriptors });
        let initial = dispatch(&first, &bound, guest);
        assert_eq!(initial, [0, 0]);
        let later = host.new_list().unwrap();
        let held = ready(&later, &heap, &[], &[], &[], false).unwrap();
        {
            let mut guard = MEMORY.lock().unwrap();
            let m = guard.as_mut().unwrap();
            for at in guest {
                let page = (at / PAGE) as u32;
                let slot = 1 + page % (MEMORY_FAULTS - 1);
                assert_eq!(
                    unsafe { m.faults.1.add(slot as usize).read_volatile() },
                    page
                );
            }
            m.take_faults();
            let (number, offset, pages) = if reserve {
                (COPIES + 1, PAGE as u32, 2)
            } else {
                (0, 0, 0)
            };
            assert_eq!(entry(m, guest[0]), (number, 0));
            assert_eq!(entry(m, guest[1]), (number, offset));
            m.map(guest[1], PageRequest::GpuFault);
            assert_eq!(m.fault_pages, pages);
            assert_eq!(m.copies.len(), COPIES as usize + usize::from(reserve));
            assert_eq!(m.ranges.len(), COPIES as usize + pages);
            assert_eq!(
                m.ranges
                    .iter()
                    .skip(COPIES as usize)
                    .map(|&(_, _, n)| n)
                    .sum::<u64>(),
                pages as u64 * PAGE
            );
        }
        let published = ready(&later, &heap, &[], &[], &[], false).unwrap();
        assert_eq!(held.base, published.base);
        let final_values = dispatch(&later, &published, guest);
        if reserve {
            let guard = MEMORY.lock().unwrap();
            let m = guard.as_ref().unwrap();
            let src = &m.copies[COPIES as usize];
            let mut ptr = std::ptr::null_mut();
            unsafe {
                src.buffer.Map(0, None, Some(&mut ptr)).unwrap();
                let observed = [20, PAGE as usize + 40]
                    .map(|offset| ptr.cast::<u8>().add(offset).cast::<u32>().read_volatile());
                src.buffer.Unmap(0, None);
                assert_eq!(observed, [0x5ea2_10ab, 0xd157_c012]);
                eprintln!(
                    "D3D12 mapped shadow={observed:x?}, guest={:x?}, base={}, GPU read={final_values:x?}",
                    guest.map(|at| (at as *const u32).read_volatile()),
                    published.base
                );
            }
        }
        assert_eq!(
            final_values,
            if reserve {
                [0x5ea2_10ab, 0xd157_c012]
            } else {
                [0, 0]
            }
        );
        eprintln!(
            "D3D12 real heap reads after 1023 views, fault reserve {reserve}: first {initial:x?}, next {final_values:x?}"
        );
        // A real AS descriptor can point near the end of a page. This new
        // page has never generated a fault. Its neighbour stays unmapped.
        let view_resource = extra + 2 * PAGE;
        let as_address = view_resource + 0xe100;
        let next_page = view_resource + PAGE;
        crate::shadow::register(view_resource as usize, view_resource, 2 * PAGE);
        unsafe { ((as_address + 20) as *mut u32).write(0x1234_abcd); }
        let descriptor = crate::descriptor::View::Shader(0, Some([
            0, 11, 0x1688, 0, as_address as u32, (as_address >> 32) as u32, 0, 0, 0, 0,
        ]));
        let confirmed = crate::descriptor::host::acceleration_structure_address(&descriptor).unwrap();
        let view_list = host.new_list().unwrap();
        let with_view = ready(&view_list, &heap, &[], &[], &[0, confirmed, confirmed], false).unwrap();
        let first_view = dispatch(&view_list, &with_view, [confirmed + 20, next_page + 20]);
        assert_eq!(first_view, if reserve { [0x1234_abcd, 0] } else { [0, 0] });
        {
            let guard = MEMORY.lock().unwrap();
            let m = guard.as_ref().unwrap();
            assert_eq!(entry(m, next_page), (0, 0));
            let number = if reserve { COPIES + 2 } else { 0 };
            assert_eq!(entry(m, confirmed), (number, 0));
            if reserve { assert_eq!(m.fault_pages, 3); }
        }
        eprintln!("D3D12 shader-declared AS {confirmed:#x}, first dispatch {first_view:x?}, next page unprepared");
        if reserve {
            // The producer's resource is smaller than the directory page.
            // Give the GPU producer a distinct marker in the same list as
            // the reader, before the end-of-submission copy to title memory.
            let partial_page = extra + 4 * PAGE;
            let partial_as = partial_page + 0xe100;
            let partial_word = partial_as + 20;
            let old_word = 0x5842_4f58u32;
            let new_word = 0x5ea2_10abu32;
            let canary = 0x0bad_f00du32;
            unsafe {
                (partial_word as *mut u32).write(old_word);
                ((partial_as - 4) as *mut u32).write(canary);
                ((partial_as + 0x1000) as *mut u32).write(!canary);
            }
            crate::shadow::register(partial_as as usize, partial_as, 0x1000);
            let producer = crate::shadow::find(partial_as, 1).unwrap();
            let alias_list = host.new_list().unwrap();
            producer.refresh(&alias_list, partial_as, 0x1000);
            producer.gpu_writes(&alias_list, partial_as, 0x1000);
            // Real shader UAV write into the sub-buffer before its reader.
            unsafe {
                alias_list.list.SetDescriptorHeaps(&[Some(heap.heap.clone())]);
                alias_list.list.SetComputeRootSignature(&root);
                alias_list.list.SetPipelineState(&pso);
                alias_list.list.SetComputeRootShaderResourceView(0, published.directory);
                alias_list.list.SetComputeRootUnorderedAccessView(1, producer.buffer.GetGPUVirtualAddress());
                alias_list.list.SetComputeRootUnorderedAccessView(2, published.faults);
                let args = [(guest[0]/PAGE) as u32, (guest[0]%PAGE) as u32, published.base, 20];
                alias_list.list.SetComputeRoot32BitConstants(3, 4, args.as_ptr().cast(), 0);
                alias_list.list.Dispatch(1,1,1);
            }
            let bound = ready(&alias_list, &heap, &[], &[], &[partial_as], false).unwrap();
            let directory_owner = {
                let guard = MEMORY.lock().unwrap();
                let m = guard.as_ref().unwrap();
                let (index, _) = entry(m,partial_as);
                m.copies[index as usize-1].clone()
            };
            let same = Arc::ptr_eq(&producer, &directory_owner);
            let seen = dispatch(&alias_list, &bound, [partial_word, partial_as - 4]);
            let after = host.new_list().unwrap();
            let outside = dispatch(&after, &bound, [partial_as - 4, partial_as + 0x1000]);
            assert_eq!(outside,[canary,!canary]);
            let guest_after = unsafe { (partial_word as *const u32).read_volatile() };
            let mut sampled = std::ptr::null_mut();
            let written = unsafe {
                producer.buffer.Map(0,None,Some(&mut sampled)).unwrap();
                let value = sampled.cast::<u8>().add(20).cast::<u32>().read_volatile();
                producer.buffer.Unmap(0, None);
                value
            };
            eprintln!("AS partial page same-command-list: same resource={same}, producer={:x?}, directory={:x?}, new in producer={written:#x}, directory GPU read={seen:x?}, guest after fence={guest_after:#x}", producer.span(),directory_owner.span());
            assert!(!same);
            assert_eq!(written, new_word);
            assert_eq!(seen, [new_word, canary]);
            assert_eq!(guest_after, new_word);
            eprintln!("AS alias GPU adjacent words preserved: {outside:x?}");
            // Another list has no earlier GPU write to the source. A newer
            // guest value must then survive an existing but stale sub-buffer.
            let newer_guest = 0x7856_3412u32;
            unsafe { (partial_word as *mut u32).write(newer_guest); }
            let new_list = host.new_list().unwrap();
            let fresh = ready(&new_list, &heap, &[], &[], &[partial_as], false).unwrap();
            let from_guest = dispatch(&new_list, &fresh, [partial_word, partial_as - 4]);
            assert_eq!(from_guest, [newer_guest, canary]);
            eprintln!("AS unrelated list: newer guest value survives stale sub-buffer {from_guest:x?}");
            // The constructor's UAV and the consumer's page belong to distinct
            // GPU resources. The title still owns its empty original memory.
            unsafe {
                ((partial_as+48) as *mut u32).write(0);
                ((partial_as+128) as *mut u32).write(0);
            }
            let build=host.new_list().unwrap();
            producer.refresh(&build,partial_as,0x1000);
            unsafe {
                build.list.SetDescriptorHeaps(&[Some(heap.heap.clone())]);
                build.list.SetComputeRootSignature(&root);
                build.list.SetPipelineState(&pso);
                build.list.SetComputeRootShaderResourceView(0,published.directory);
                build.list.SetComputeRootUnorderedAccessView(1,producer.buffer.GetGPUVirtualAddress());
                build.list.SetComputeRootUnorderedAccessView(2,published.faults);
                for (address,dest) in [(guest[0]+8,48),(guest[0]+12,128)] {
                    let args=[(address/PAGE) as u32,(address%PAGE) as u32,published.base,dest];
                    build.list.SetComputeRoot32BitConstants(3,4,args.as_ptr().cast(),0);
                    build.list.Dispatch(1,1,1);
                }
            }
            produced::Capture::record(&build,partial_as,&producer);
            build.close(); host.execute(&[&build]);
            assert_eq!(produced::node(partial_as), Some(partial_as+128));
            let consumer=host.new_list().unwrap();
            let visible=ready(&consumer,&heap,&[],&[],&[partial_as],true).unwrap();
            let patch=produced::Overlay::bind(&consumer,partial_as).expect("fenced root and first node");
            issue(&consumer,&visible,[partial_as+48,partial_as+128]);
            patch.finish(&consumer);
            consumer.close(); host.execute(&[&consumer]);
            let seen=unsafe {output.cast::<[u32;2]>().read_volatile()};
            let guest_after=unsafe {[((partial_as+48) as *const u32).read_volatile(),((partial_as+128) as *const u32).read_volatile()]};
            let clean=host.new_list().unwrap();
            let ordinary=dispatch(&clean,&visible,[partial_as+48,partial_as+128]);
            eprintln!("fenced GPU constructor -> second list: AS={partial_as:#x} node={:#x} renderer={seen:x?} unmodified guest={guest_after:x?} ordinary GPU reads={ordinary:x?}",partial_as+128);
            assert_eq!(seen,[21,0xa15ea5ed]);
            assert_eq!(guest_after,[0,0]);
            assert_eq!(ordinary,[0,0]);
            // Replay the guest initializer's emitted dwords and the exact
            // 40-byte structured input. The helper authenticates every word.
            let jobs_at=start+0x400;
            let jobs=crate::shadow::find(jobs_at,40).unwrap();
            let raw_at=directory_owner.offset_in(partial_as).unwrap();
            let expected=[6,0,11,74,0x7f7fffff,0x7f7fffff,0x7f7fffff,0xff7fffff,
                          0xff7fffff,0xff7fffff,0,17,0xffffffff,23,28,0x007f7f7fu32];
            for (at,bad_header,should_copy) in [(partial_as+256,false,false),(partial_as,true,false),(partial_as,false,true)] {
                let list=host.new_list().unwrap();
                let records=[at as u32,(at>>32) as u32,0,0,6,11,74,17,23,28];
                let mut header=expected;if bad_header {header[3]=75}
                let seed=[0x89abcdefu32;17];
                for (dest,offset,words) in [(&jobs,jobs.offset_in(jobs_at).unwrap(),records.as_slice()),(&directory_owner,raw_at,header.as_slice()),(&producer,producer.offset_in(partial_as).unwrap(),seed.as_slice())] {
                    let bytes=std::mem::size_of_val(words);
                    let (upload,ptr)=host.upload_buffer(bytes as u64).unwrap();
                    unsafe {
                        std::ptr::copy_nonoverlapping(words.as_ptr().cast::<u8>(),ptr,bytes);
                        list.list.ResourceBarrier(&[crate::host::transition(&dest.buffer,D3D12_RESOURCE_STATE_UNORDERED_ACCESS,D3D12_RESOURCE_STATE_COPY_DEST)]);
                        list.list.CopyBufferRegion(&dest.buffer,offset,&upload,0,bytes as u64);
                        list.list.ResourceBarrier(&[crate::host::transition(&dest.buffer,D3D12_RESOURCE_STATE_COPY_DEST,D3D12_RESOURCE_STATE_UNORDERED_ACCESS)]);
                    }
                    list.keep(&upload);
                }
                assert!(initialized::bridge(&list,&jobs,jobs_at,1,&directory_owner,raw_at,&producer,partial_as));
                list.close();host.execute(&[&list]);
                let mut at=std::ptr::null_mut();
                let observed=unsafe {
                    producer.buffer.Map(0,None,Some(&mut at)).unwrap();
                    let got=at.cast::<u8>().add(producer.offset_in(partial_as).unwrap() as usize).cast::<[u32;17]>().read_unaligned();
                    producer.buffer.Unmap(0,None);got
                };
                let guest_counter=unsafe{((partial_as+12) as *const u32).read_volatile()};
                if should_copy {assert_eq!(&observed[..16],expected.as_slice());}else{assert_eq!(observed,seed);}
                assert_eq!(observed[16],seed[16]);assert_eq!(guest_counter,0);
                eprintln!("initializer real GPU match={should_copy} leaves_at_plus12={} seed_at_plus48={:08x} next_word={:08x} guest_counter={guest_counter}",observed[3],observed[12],observed[16]);
            }
            crate::shadow::unregister(partial_as as usize, partial_as);
        }
        *MEMORY.lock().unwrap() = None;
        drop(view_list);
        crate::shadow::unregister(view_resource as usize, view_resource);
        drop(first);
        drop(later);
        for i in 0..=COPIES {
            let at = start + u64::from(i) * PAGE;
            crate::shadow::unregister(at as usize, at);
        }
        unsafe {
            assert_ne!(VirtualFree(start as *mut _, 0, MEM_RELEASE), 0);
        }
    }
}
