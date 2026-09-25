//! Hand-written bindings for the parts of the Limine boot protocol this
//! kernel actually uses (base revision negotiation, bootloader info, the
//! higher-half direct map offset, the memory map, and one framebuffer).
//!
//! These are translated field-for-field from `limine.h` (the protocol
//! header shipped alongside the `limine` binary in `../limine/`), matching
//! Limine 9.x / base revision 3. There's a `limine` crate on crates.io that
//! does this for you, but recent versions require nightly-only features
//! (`ptr_metadata`); writing the ~10 structs we need by hand keeps this
//! buildable on stable and is a good way to actually understand the
//! protocol. See <https://github.com/limine-bootloader/limine/blob/v9.x/PROTOCOL.md>
//! for the full spec if you want to add more requests later.

#![allow(dead_code)]

use core::sync::atomic::{AtomicU64, Ordering};

const COMMON_MAGIC: [u64; 2] = [0xc7b1dd30df4c8b88, 0x0a82e883a194f07b];

/// Tells Limine which revision of the base protocol we speak. Limine
/// rewrites the third element to `0` if it accepted the requested revision;
/// [`base_revision_supported`] checks that.
#[used]
#[unsafe(link_section = ".requests")]
static BASE_REVISION: [u64; 3] = [0xf9562b2d5c95a6c8, 0x6a7b384944536bdc, 3];

pub fn base_revision_supported() -> bool {
    core::hint::black_box(&BASE_REVISION)[2] == 0
}

/// Markers that bound the `.requests` section so Limine can find every
/// request struct without us having to register them individually.
#[used]
#[unsafe(link_section = ".requests_start_marker")]
static REQUESTS_START: [u64; 4] = [
    0xf6b8f4b39de7d1ae,
    0xfab91a6940fcb9cf,
    0x785c6ed015d3e316,
    0x181e920a7852b9d9,
];

#[used]
#[unsafe(link_section = ".requests_end_marker")]
static REQUESTS_END: [u64; 2] = [0xadc0e0531bb10d03, 0x9572709f31764c62];

// ---------------------------------------------------------------------
// Bootloader info
// ---------------------------------------------------------------------

#[repr(C)]
struct BootloaderInfoResponse {
    revision: u64,
    name: *const u8,
    version: *const u8,
}

#[repr(C)]
struct BootloaderInfoRequest {
    id: [u64; 4],
    revision: u64,
    response: AtomicU64, // *const BootloaderInfoResponse, written by Limine
}

unsafe impl Sync for BootloaderInfoRequest {}

#[used]
#[unsafe(link_section = ".requests")]
static BOOTLOADER_INFO_REQUEST: BootloaderInfoRequest = BootloaderInfoRequest {
    id: [
        COMMON_MAGIC[0],
        COMMON_MAGIC[1],
        0xf55038d8e2a1202f,
        0x279426fcf5f59740,
    ],
    revision: 0,
    response: AtomicU64::new(0),
};

pub struct BootloaderInfo {
    pub name: &'static str,
    pub version: &'static str,
}

pub fn bootloader_info() -> Option<BootloaderInfo> {
    let ptr = BOOTLOADER_INFO_REQUEST.response.load(Ordering::Acquire) as *const BootloaderInfoResponse;
    if ptr.is_null() {
        return None;
    }
    let resp = unsafe { &*ptr };
    Some(BootloaderInfo {
        name: unsafe { cstr_to_str(resp.name) },
        version: unsafe { cstr_to_str(resp.version) },
    })
}

// ---------------------------------------------------------------------
// Higher-half direct map (HHDM)
// ---------------------------------------------------------------------

#[repr(C)]
struct HhdmResponse {
    revision: u64,
    offset: u64,
}

#[repr(C)]
struct HhdmRequest {
    id: [u64; 4],
    revision: u64,
    response: AtomicU64,
}

unsafe impl Sync for HhdmRequest {}

#[used]
#[unsafe(link_section = ".requests")]
static HHDM_REQUEST: HhdmRequest = HhdmRequest {
    id: [
        COMMON_MAGIC[0],
        COMMON_MAGIC[1],
        0x48dcf1cb8ad2b852,
        0x63984e959a98244b,
    ],
    revision: 0,
    response: AtomicU64::new(0),
};

/// Returns the virtual-address offset of the higher-half direct map, i.e.
/// physical address `p` is also mapped at virtual address `p + offset`.
pub fn hhdm_offset() -> Option<u64> {
    let ptr = HHDM_REQUEST.response.load(Ordering::Acquire) as *const HhdmResponse;
    if ptr.is_null() {
        return None;
    }
    Some(unsafe { (*ptr).offset })
}

// ---------------------------------------------------------------------
// Framebuffer
// ---------------------------------------------------------------------

#[repr(C)]
struct RawFramebuffer {
    address: *mut u8,
    width: u64,
    height: u64,
    pitch: u64,
    bpp: u16,
    memory_model: u8,
    red_mask_size: u8,
    red_mask_shift: u8,
    green_mask_size: u8,
    green_mask_shift: u8,
    blue_mask_size: u8,
    blue_mask_shift: u8,
    unused: [u8; 7],
    edid_size: u64,
    edid: *const u8,
    mode_count: u64,
    modes: *const *const u8,
}

#[repr(C)]
struct FramebufferResponse {
    revision: u64,
    framebuffer_count: u64,
    framebuffers: *const *const RawFramebuffer,
}

#[repr(C)]
struct FramebufferRequest {
    id: [u64; 4],
    revision: u64,
    response: AtomicU64,
}

unsafe impl Sync for FramebufferRequest {}

#[used]
#[unsafe(link_section = ".requests")]
static FRAMEBUFFER_REQUEST: FramebufferRequest = FramebufferRequest {
    id: [
        COMMON_MAGIC[0],
        COMMON_MAGIC[1],
        0x9d5827dcd881dd75,
        0xa3148604f6fab11b,
    ],
    revision: 0,
    response: AtomicU64::new(0),
};

#[derive(Clone, Copy)]
pub struct Framebuffer {
    pub address: *mut u8,
    pub width: u64,
    pub height: u64,
    pub pitch: u64,
    pub bpp: u16,
    pub red_mask_shift: u8,
    pub green_mask_shift: u8,
    pub blue_mask_shift: u8,
}

/// Returns the first framebuffer Limine reports, if any.
pub fn framebuffer() -> Option<Framebuffer> {
    let ptr = FRAMEBUFFER_REQUEST.response.load(Ordering::Acquire) as *const FramebufferResponse;
    if ptr.is_null() {
        return None;
    }
    let resp = unsafe { &*ptr };
    if resp.framebuffer_count == 0 {
        return None;
    }
    let fb_ptr = unsafe { *resp.framebuffers } as *const RawFramebuffer;
    let fb = unsafe { &*fb_ptr };
    Some(Framebuffer {
        address: fb.address,
        width: fb.width,
        height: fb.height,
        pitch: fb.pitch,
        bpp: fb.bpp,
        red_mask_shift: fb.red_mask_shift,
        green_mask_shift: fb.green_mask_shift,
        blue_mask_shift: fb.blue_mask_shift,
    })
}

// ---------------------------------------------------------------------
// Memory map
// ---------------------------------------------------------------------

#[repr(C)]
struct MemmapEntryRaw {
    base: u64,
    length: u64,
    kind: u64,
}

#[repr(C)]
struct MemmapResponse {
    revision: u64,
    entry_count: u64,
    entries: *const *const MemmapEntryRaw,
}

#[repr(C)]
struct MemmapRequest {
    id: [u64; 4],
    revision: u64,
    response: AtomicU64,
}

unsafe impl Sync for MemmapRequest {}

#[used]
#[unsafe(link_section = ".requests")]
static MEMMAP_REQUEST: MemmapRequest = MemmapRequest {
    id: [
        COMMON_MAGIC[0],
        COMMON_MAGIC[1],
        0x67cf3d9d378a806f,
        0xe304acdfc50c3c62,
    ],
    revision: 0,
    response: AtomicU64::new(0),
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MemmapKind {
    Usable,
    Reserved,
    AcpiReclaimable,
    AcpiNvs,
    BadMemory,
    BootloaderReclaimable,
    ExecutableAndModules,
    Framebuffer,
    Unknown(u64),
}

impl MemmapKind {
    fn from_raw(v: u64) -> Self {
        match v {
            0 => MemmapKind::Usable,
            1 => MemmapKind::Reserved,
            2 => MemmapKind::AcpiReclaimable,
            3 => MemmapKind::AcpiNvs,
            4 => MemmapKind::BadMemory,
            5 => MemmapKind::BootloaderReclaimable,
            6 => MemmapKind::ExecutableAndModules,
            7 => MemmapKind::Framebuffer,
            other => MemmapKind::Unknown(other),
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            MemmapKind::Usable => "usable",
            MemmapKind::Reserved => "reserved",
            MemmapKind::AcpiReclaimable => "ACPI reclaimable",
            MemmapKind::AcpiNvs => "ACPI NVS",
            MemmapKind::BadMemory => "bad memory",
            MemmapKind::BootloaderReclaimable => "bootloader reclaimable",
            MemmapKind::ExecutableAndModules => "kernel/modules",
            MemmapKind::Framebuffer => "framebuffer",
            MemmapKind::Unknown(_) => "unknown",
        }
    }
}

#[derive(Clone, Copy)]
pub struct MemmapEntry {
    pub base: u64,
    pub length: u64,
    pub kind: MemmapKind,
}

pub struct MemmapIter {
    entries: *const *const MemmapEntryRaw,
    count: u64,
    index: u64,
}

impl Iterator for MemmapIter {
    type Item = MemmapEntry;

    fn next(&mut self) -> Option<MemmapEntry> {
        if self.index >= self.count {
            return None;
        }
        let entry_ptr = unsafe { *self.entries.add(self.index as usize) };
        let entry = unsafe { &*entry_ptr };
        self.index += 1;
        Some(MemmapEntry {
            base: entry.base,
            length: entry.length,
            kind: MemmapKind::from_raw(entry.kind),
        })
    }
}

/// Returns an iterator over the memory map, or `None` if Limine hasn't
/// answered the request (shouldn't happen on a conforming bootloader).
pub fn memmap() -> Option<MemmapIter> {
    let ptr = MEMMAP_REQUEST.response.load(Ordering::Acquire) as *const MemmapResponse;
    if ptr.is_null() {
        return None;
    }
    let resp = unsafe { &*ptr };
    Some(MemmapIter {
        entries: resp.entries,
        count: resp.entry_count,
        index: 0,
    })
}

// ---------------------------------------------------------------------

/// # Safety
/// `ptr` must be null or point to a valid, NUL-terminated, UTF-8 string
/// that lives at least as long as `'static` (true for everything Limine
/// hands us -- it lives in memory Limine reserved for the whole boot).
unsafe fn cstr_to_str(ptr: *const u8) -> &'static str {
    if ptr.is_null() {
        return "";
    }
    let mut len = 0usize;
    unsafe {
        while *ptr.add(len) != 0 {
            len += 1;
        }
        let slice = core::slice::from_raw_parts(ptr, len);
        core::str::from_utf8_unchecked(slice)
    }
}
