//! Physical frame allocator: a bitmap over every 4 KiB frame of physical
//! memory Limine described, one bit per frame (0 = free, 1 = used).
//!
//! The bitmap itself needs somewhere to live *before* there's a heap to put
//! it in -- chicken and egg -- so `init()` picks the largest usable region
//! from the memory map and carves the bitmap out of the front of it,
//! marking those frames used immediately afterward. Access is through the
//! HHDM (higher-half direct map) Limine already set up: physical address
//! `p` is readable/writable at `hhdm_offset + p`, so this needs no paging
//! work of its own -- `paging.rs` (built on top of this allocator) is what
//! adds *new* mappings for things like the heap.

use crate::limine::{self, MemmapKind};
use crate::sync::IrqSpinLock;

pub const FRAME_SIZE: u64 = 4096;

struct Bitmap {
    /// HHDM-mapped pointer to the bitmap's backing bytes.
    bits: *mut u8,
    frame_count: u64,
    free_count: u64,
    /// Search hint: the bitmap word index to start the next allocation
    /// search from, so repeated allocations don't rescan from frame 0
    /// every time once the low end fills up.
    next_hint: u64,
}

// Safety: single-core kernel, and all access goes through PMM's IrqSpinLock.
unsafe impl Send for Bitmap {}

impl Bitmap {
    #[inline]
    fn get(&self, frame: u64) -> bool {
        let byte = unsafe { *self.bits.add((frame / 8) as usize) };
        (byte >> (frame % 8)) & 1 != 0
    }

    #[inline]
    fn set(&mut self, frame: u64, used: bool) {
        unsafe {
            let ptr = self.bits.add((frame / 8) as usize);
            let byte = *ptr;
            *ptr = if used { byte | (1 << (frame % 8)) } else { byte & !(1 << (frame % 8)) };
        }
    }
}

struct Pmm {
    bitmap: Option<Bitmap>,
    hhdm_offset: u64,
}

// Page faults and the scheduler reaper also allocate/free with IRQs off.
// Ordinary task-context users must not be preempted while holding PMM.
static PMM: IrqSpinLock<Pmm> = IrqSpinLock::new(Pmm { bitmap: None, hhdm_offset: 0 });

fn phys_to_virt(hhdm_offset: u64, phys: u64) -> *mut u8 {
    (hhdm_offset + phys) as *mut u8
}

/// # Safety
/// Must be called exactly once, with a valid HHDM offset and the real
/// Limine memory map, before any other function in this module.
pub unsafe fn init(hhdm_offset: u64) {
    let Some(entries) = limine::memmap() else {
        panic!("pmm::init: no memory map from Limine");
    };

    // Pass 1: find the highest usable address (-> how many frames to track
    // at all) and the largest single usable region (-> where the bitmap
    // itself lives).
    let mut highest_addr: u64 = 0;
    let mut best_region: (u64, u64) = (0, 0); // (base, length)
    for entry in entries {
        if entry.kind == MemmapKind::Usable {
            let end = entry.base + entry.length;
            if end > highest_addr {
                highest_addr = end;
            }
            if entry.length > best_region.1 {
                best_region = (entry.base, entry.length);
            }
        }
    }
    assert!(best_region.1 > 0, "pmm::init: no usable memory regions at all");

    let frame_count = highest_addr.div_ceil(FRAME_SIZE);
    let bitmap_bytes = frame_count.div_ceil(8);

    assert!(
        bitmap_bytes <= best_region.1,
        "pmm::init: largest usable region is smaller than the bitmap it would need to hold"
    );

    let bitmap_ptr = phys_to_virt(hhdm_offset, best_region.0);

    // Start with every frame marked used; usable regions get cleared below.
    // This is what makes reserved/ACPI/bootloader-reclaimable/framebuffer/
    // kernel-and-modules regions come out "used" without having to name
    // them individually -- anything not explicitly usable stays used.
    unsafe {
        core::ptr::write_bytes(bitmap_ptr, 0xFF, bitmap_bytes as usize);
    }

    let mut bitmap = Bitmap {
        bits: bitmap_ptr,
        frame_count,
        free_count: 0,
        next_hint: 0,
    };

    let Some(entries) = limine::memmap() else {
        unreachable!("memmap() succeeded once already");
    };
    for entry in entries {
        if entry.kind != MemmapKind::Usable {
            continue;
        }
        let first_frame = entry.base / FRAME_SIZE;
        let last_frame = (entry.base + entry.length) / FRAME_SIZE;
        for frame in first_frame..last_frame {
            bitmap.set(frame, false);
            bitmap.free_count += 1;
        }
    }

    // Now reserve the frames the bitmap itself occupies.
    let bitmap_first_frame = best_region.0 / FRAME_SIZE;
    let bitmap_frame_count = bitmap_bytes.div_ceil(FRAME_SIZE);
    for frame in bitmap_first_frame..bitmap_first_frame + bitmap_frame_count {
        if !bitmap.get(frame) {
            bitmap.set(frame, true);
            bitmap.free_count -= 1;
        }
    }

    let mut pmm = PMM.lock();
    pmm.hhdm_offset = hhdm_offset;
    pmm.bitmap = Some(bitmap);
}

/// Allocates one 4 KiB physical frame, returning its physical address.
/// `None` if physical memory is exhausted.
pub fn alloc_frame() -> Option<u64> {
    let mut pmm = PMM.lock();
    let bitmap = pmm.bitmap.as_mut()?;
    let start = bitmap.next_hint;
    for frame in (start..bitmap.frame_count).chain(0..start) {
        if !bitmap.get(frame) {
            bitmap.set(frame, true);
            bitmap.free_count -= 1;
            bitmap.next_hint = frame + 1;
            return Some(frame * FRAME_SIZE);
        }
    }
    None
}

/// Frees a frame previously returned by [`alloc_frame`].
///
/// # Safety
/// `phys` must be a frame this allocator actually handed out, and nothing
/// may still be using it (mapped or otherwise referenced).
#[allow(dead_code)] // Part of the allocator's API surface; not exercised yet.
pub unsafe fn free_frame(phys: u64) {
    let mut pmm = PMM.lock();
    if let Some(bitmap) = pmm.bitmap.as_mut() {
        let frame = phys / FRAME_SIZE;
        if bitmap.get(frame) {
            bitmap.set(frame, false);
            bitmap.free_count += 1;
        }
    }
}

/// `(total_frames, free_frames)`.
pub fn stats() -> (u64, u64) {
    let pmm = PMM.lock();
    match &pmm.bitmap {
        Some(b) => (b.frame_count, b.free_count),
        None => (0, 0),
    }
}

pub fn hhdm_offset() -> u64 {
    PMM.lock().hhdm_offset
}

