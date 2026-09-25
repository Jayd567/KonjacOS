//! Ties `pmm` (physical frames) and `heap` (the `GlobalAlloc` backing
//! `alloc::*`) together behind one `init()`, plus the `meminfo` shell
//! command's implementation.

extern crate alloc;

use alloc::vec::Vec;

use crate::{pmm, heap};
use crate::println;

/// # Safety
/// Must be called exactly once, after the HHDM offset is known, and before
/// anything tries to `alloc::*`.
pub unsafe fn init(hhdm_offset: u64) {
    unsafe {
        pmm::init(hhdm_offset);
        heap::init();
    }
}

/// Allocates and drops a `Vec`, pushing a handful of values and checking
/// they read back correctly, as a quick end-to-end proof the heap actually
/// works (not just that it compiles). Returns `true` on success.
fn self_test() -> bool {
    let mut v: Vec<u32> = Vec::new();
    for i in 0..64u32 {
        v.push(i * i);
    }
    if v.len() != 64 {
        return false;
    }
    for (i, value) in v.iter().enumerate() {
        if *value != (i as u32) * (i as u32) {
            return false;
        }
    }
    drop(v);
    true
}

/// Implements the `meminfo` shell command: physical frame usage, heap
/// usage, and a live allocator self-test.
pub fn print_info() {
    let (total_frames, free_frames) = pmm::stats();
    let used_frames = total_frames - free_frames;
    let frame_kib = pmm::FRAME_SIZE / 1024;
    println!(
        "physical: {} MiB used, {} MiB free, {} MiB total ({} frames @ {} KiB)",
        used_frames * pmm::FRAME_SIZE / (1024 * 1024),
        free_frames * pmm::FRAME_SIZE / (1024 * 1024),
        total_frames * pmm::FRAME_SIZE / (1024 * 1024),
        total_frames,
        frame_kib
    );
    // Exact frame counts, not MiB-rounded -- a handful of leaked pages (a
    // ring-3 task's private address space, say) is invisible in the MiB
    // figures above (256 frames = 1 MiB) but shows up here immediately.
    println!("physical (exact): {used_frames} used frames, {free_frames} free frames");

    let (bump_free, mapped) = heap::stats();
    println!(
        "heap: {} KiB free (bump), {} KiB mapped",
        bump_free / 1024,
        mapped / 1024
    );

    if self_test() {
        println!("heap self-test: OK (allocated + verified a 64-element Vec)");
    } else {
        println!("heap self-test: FAILED");
    }
}
