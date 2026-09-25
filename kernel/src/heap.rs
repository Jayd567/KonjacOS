//! A small kernel heap: a fixed virtual region, eagerly mapped a page at a
//! time at init, managed by a singly-linked free-list allocator
//! (first-fit, no coalescing). Good enough for a hobby kernel's early
//! `alloc` needs -- not something you'd want in production, and the lack
//! of coalescing means long-running alloc/free churn of mixed sizes will
//! fragment over time. That's a known, accepted limitation for now; a
//! bump-then-free-list-with-coalescing design would be the natural
//! upgrade if it ever matters.

use core::alloc::{GlobalAlloc, Layout};
use core::ptr::NonNull;

use crate::paging::{self, PAGE_WRITABLE};
use crate::pmm;
use crate::sync::SpinLock;

/// Where the heap lives in virtual memory. Chosen to sit well away from
/// both the higher-half kernel image (0xffffffff80000000+) and Limine's
/// HHDM window, in the unused canonical space in between.
const HEAP_BASE: u64 = 0xFFFF_9000_0000_0000;
/// How much of the heap is mapped up front, in bytes. Used to be 1 MiB,
/// plenty for the shell/demo commands this kernel ran at the time -- bumped
/// to 16 MiB once `wm.rs` started taking whole-framebuffer snapshots for
/// its background (`snapshot()`/`blit()` in `framebuffer.rs`): a single
/// 1280x800x32bpp snapshot alone is ~4 MiB, more than the entire old heap,
/// and 1 MiB's worth of `alloc::vec![0u8; ...]` for it failed outright the
/// first time `gui` ran. Bumped again to 64 MiB as part of the DOOM-porting
/// groundwork: `libc_shim.rs`'s `malloc`/`free` route straight through this
/// same allocator, and a real game's asset/level data (textures, sounds,
/// the WAD file's contents once one's loaded) is a different order of
/// magnitude from anything this kernel has allocated so far. Bumped again
/// to 96 MiB for real JVM bring-up (see README item 43): `sys_open` reads
/// a whole file into a heap `Vec<u8>` up front (`linux_syscall.rs`), so
/// every real JDK library `ld.so` opens before a task exits or crashes --
/// `libjli.so`, `libc.so.6`, `libjvm.so`, `libjava.so`, ... -- leaves a
/// same-sized heap allocation alive until that task is reaped; one real
/// `java` run's worth of these was measured at ~30 MiB, and 64 MiB wasn't
/// enough headroom for two such runs back to back even with item 43's
/// address-space leak fixed (this heap's first-fit/no-coalescing
/// allocator, see this module's own doc comment, can't always stretch
/// scattered smaller free blocks into one contiguous same-sized block
/// either). Deliberately not bumped further than this: this is eagerly
/// mapped physical memory (see `init` below), taken directly out of the
/// same ~185 MiB free at boot that real JVM segment mappings themselves
/// still need room in -- growing the heap without limit would just trade
/// one OOM for another. Revisit alongside actually growing the QEMU
/// config's own `-m` (`Makefile`'s `QEMU_FLAGS`) if this stops being
/// enough headroom.
const INITIAL_HEAP_SIZE: u64 = 96 * 1024 * 1024;
const PAGE_SIZE: u64 = 4096;

/// Every free block starts with this header, forming an intrusive linked
/// list threaded directly through the freed memory itself -- no separate
/// bookkeeping allocation needed.
#[repr(C)]
struct FreeBlock {
    size: usize,
    next: Option<NonNull<FreeBlock>>,
}

const MIN_BLOCK_SIZE: usize = core::mem::size_of::<FreeBlock>();
/// Every allocation is rounded up to at least this alignment, so that
/// writing a `FreeBlock` header into freed memory (which happens on every
/// `dealloc`) can never itself be misaligned and corrupt neighbouring data.
/// 16 (not just 8) matters now that `task.rs` allocates `fxsave`/`fxrstor`
/// buffers on the heap -- those instructions fault on anything less than
/// 16-byte aligned, and this is the only lever that guarantees it, since
/// `alloc()` below refuses any request for more alignment than this.
/// `pub(crate)` so `libc_shim.rs`'s `malloc` can size its own header to
/// match -- it needs to know this allocator's actual guarantee, not just
/// assume one.
pub(crate) const MIN_ALIGN: usize = 16;

struct FreeList {
    head: Option<NonNull<FreeBlock>>,
}

// Safety: single-core kernel; all access goes through `HEAP`'s SpinLock.
unsafe impl Send for FreeList {}

impl FreeList {
    const fn new() -> Self {
        FreeList { head: None }
    }

    /// Adds a block of `size` bytes at `ptr` back onto the free list.
    unsafe fn push(&mut self, ptr: *mut u8, size: usize) {
        let block = ptr as *mut FreeBlock;
        unsafe {
            block.write(FreeBlock { size, next: self.head });
        }
        self.head = NonNull::new(block);
    }

    /// First-fit: walks the list, unlinks the first block big enough,
    /// splits off and re-frees any leftover tail that's still big enough
    /// to be useful on its own.
    unsafe fn pop_fit(&mut self, size: usize) -> Option<*mut u8> {
        let mut prev: Option<NonNull<FreeBlock>> = None;
        let mut current = self.head;

        while let Some(mut node) = current {
            let block = unsafe { node.as_mut() };
            if block.size >= size {
                let next = block.next;
                match prev {
                    Some(mut p) => unsafe { p.as_mut().next = next },
                    None => self.head = next,
                }

                let leftover = block.size - size;
                let node_ptr = node.as_ptr() as *mut u8;
                if leftover >= MIN_BLOCK_SIZE {
                    let tail = unsafe { node_ptr.add(size) };
                    unsafe { self.push(tail, leftover) };
                }

                return Some(node_ptr);
            }
            prev = current;
            current = block.next;
        }
        None
    }
}

struct Heap {
    free: FreeList,
    /// Next never-yet-used byte within the mapped region -- the bump edge
    /// backing the free list before anything's ever been freed.
    bump: u64,
    limit: u64,
}

static HEAP: SpinLock<Heap> = SpinLock::new(Heap { free: FreeList::new(), bump: 0, limit: 0 });

pub struct KernelAllocator;

unsafe impl GlobalAlloc for KernelAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let size = layout.size().max(MIN_BLOCK_SIZE);
        let align = layout.align().max(MIN_ALIGN);
        // Every block we hand out is already MIN_ALIGN-aligned (bump edge
        // starts page-aligned and every allocation size is rounded up to a
        // multiple of MIN_ALIGN below), so as long as the caller doesn't
        // ask for more than that we can satisfy it directly.
        if align > MIN_ALIGN {
            return core::ptr::null_mut();
        }
        let size = (size + MIN_ALIGN - 1) & !(MIN_ALIGN - 1);

        let mut heap = HEAP.lock();
        if let Some(ptr) = unsafe { heap.free.pop_fit(size) } {
            return ptr;
        }

        // Nothing free-listed was big enough; carve a fresh block off the
        // bump edge if there's room left in the mapped region.
        if heap.bump + size as u64 <= heap.limit {
            let ptr = heap.bump as *mut u8;
            heap.bump += size as u64;
            return ptr;
        }

        core::ptr::null_mut()
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        let size = layout.size().max(MIN_BLOCK_SIZE);
        let size = (size + MIN_ALIGN - 1) & !(MIN_ALIGN - 1);
        let mut heap = HEAP.lock();
        unsafe {
            heap.free.push(ptr, size);
        }
    }
}

#[global_allocator]
static ALLOCATOR: KernelAllocator = KernelAllocator;

/// Maps `INITIAL_HEAP_SIZE` worth of fresh physical frames at `HEAP_BASE`
/// and hands the whole region to the allocator as one initial bump region.
///
/// # Safety
/// Must be called exactly once, after `pmm::init` and after paging is
/// otherwise ready to accept new mappings (i.e. any time after boot --
/// this just extends Limine's existing page tables).
pub unsafe fn init() {
    let pages = INITIAL_HEAP_SIZE / PAGE_SIZE;
    for i in 0..pages {
        let virt = HEAP_BASE + i * PAGE_SIZE;
        let phys = pmm::alloc_frame().expect("heap::init: out of physical memory mapping the initial heap");
        unsafe {
            paging::map_page(virt, phys, PAGE_WRITABLE);
        }
    }

    let mut heap = HEAP.lock();
    heap.bump = HEAP_BASE;
    heap.limit = HEAP_BASE + INITIAL_HEAP_SIZE;
}

/// Rough usage stats for `meminfo`: `(bytes still reachable via bump,
/// total mapped bytes)`. Doesn't account for free-listed blocks -- good
/// enough for a diagnostic, not a precise accounting.
pub fn stats() -> (u64, u64) {
    let heap = HEAP.lock();
    (heap.limit - heap.bump, heap.limit - HEAP_BASE)
}
