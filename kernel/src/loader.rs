//! The "big trio" loader: KonjacOS can load and run **flat binaries**,
//! **ELF64 executables**, and **PE32+ (.exe) executables** through the same
//! code path, dispatching on the file's own magic bytes rather than a file
//! extension or anything the caller has to specify.
//!
//! The key thing that makes this tractable instead of a Wine-scale project:
//! *container format* and *ABI* are two separate questions. A container
//! format (ELF, PE) is just a header plus a list of "map these file bytes
//! at this virtual address, with these permissions, zero-extended to this
//! larger size" instructions -- that's [`Segment`] below, and it's the same
//! shape regardless of which format described it. Parsing that container is
//! genuinely format-specific (see [`parse_elf`]/[`parse_pe`]/[`parse_flat`]
//! below) but *mapping* it is not, and neither is *running* it -- both of
//! those go through the exact same [`map_segment`]/[`task::spawn_user`]
//! path `usermode.rs`'s original hand-written demo already proved works.
//!
//! What this deliberately does **not** attempt is real binary
//! compatibility with actual Linux or Windows executables: those assume a
//! specific syscall table (Linux's `syscall` ABI, or the Win32 API surface
//! reached indirectly through `kernel32.dll`/`ntdll.dll` imports resolved
//! by a real Windows loader), neither of which exists here. A `.exe` that
//! imports from a real DLL gets rejected outright at parse time (see
//! `parse_pe`'s import-table check) rather than silently failing at
//! runtime the first time it calls something that doesn't exist. What
//! *does* work: a program written for KonjacOS's own tiny `int 0x80`
//! syscall table (`syscall.rs`), compiled and linked as an ELF64
//! executable (`ld`), a PE32+ executable (`x86_64-w64-mingw32-ld`), or
//! dumped as a raw flat binary -- three completely different containers
//! carrying what can be the exact same machine code, all runnable by the
//! same kernel. That's the actual point: format flexibility on top of one
//! OS's own ABI, not compatibility with someone else's.
//!
//! `parse_elf` also accepts `ET_DYN` (PIE, and now shared-library) ELF
//! files, not just `ET_EXEC` -- and, unlike the first version of this
//! loader, it's no longer limited to a PIE's purely self-referential
//! `R_X86_64_RELATIVE` fixups. `load_and_run` now implements real (if
//! still narrow) dynamic linking: a main executable's `DT_NEEDED` entries
//! are loaded as their own ELF files, each into its own fixed address
//! range (see `LIB_BASE`), and `R_X86_64_GLOB_DAT`/`R_X86_64_JUMP_SLOT`/
//! `R_X86_64_64` relocations are resolved by name against every loaded
//! library's exported dynamic symbols -- eagerly, at load time, rather
//! than through a real PLT's lazy-binding resolver stub (see
//! `ExternReloc`'s doc comment). What's still *not* supported: a library
//! depending on another library (single-level only), symbol versioning,
//! and anything but the SysV-style `DT_HASH` for finding a library's
//! exported symbol count (see `parse_elf_at`'s own doc comment) -- all of
//! which get rejected with a clear reason rather than silently
//! mis-linking, exactly like `parse_pe` rejects a `.exe` needing real
//! DLLs.

extern crate alloc;

use alloc::collections::BTreeMap;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use crate::fat16;
use crate::paging::{self, PAGE_USER, PAGE_WRITABLE};
use crate::pmm;
use crate::task;

const PAGE_SIZE: u64 = 4096;

/// Every loaded program's stack lands at this fixed address in its own
/// private address space, regardless of format -- ELF/PE headers describe
/// code and data layout, not where the OS should put an initial stack, and
/// since every task built here gets its own fresh `paging::new_address_space`
/// (see `usermode.rs`'s module docs for why that makes reusing the same
/// address across completely unrelated tasks safe), there's no collision
/// risk in always picking the same one.
const USER_STACK_BASE: u64 = 0x0000_0050_0000_0000;
/// 128 KiB -- generous headroom over what a native KonjacOS demo needs
/// (bumped up from an original 16 KiB specifically because a real
/// `musl-gcc`-built static binary's startup path -- TLS setup, stdio
/// buffering -- turned out to want more than that; see `load_and_run`'s
/// real Linux initial-stack layout and `linux_syscall.rs`'s module docs).
const USER_STACK_SIZE: u64 = 128 * 1024;

/// Where a flat `.bin` gets loaded -- the one format with no header to say
/// where it wants to live, so KonjacOS just picks something (matching the
/// address `usermode.rs`'s original hand-written demo always used).
const FLAT_BASE: u64 = 0x0000_0040_0000_0000;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Format {
    Elf,
    Pe,
    Flat,
}

impl Format {
    pub fn label(self) -> &'static str {
        match self {
            Format::Elf => "ELF64",
            Format::Pe => "PE32+",
            Format::Flat => "flat binary",
        }
    }
}

/// One contiguous chunk of the program's address space, in the same terms
/// every real OS's loader ultimately reduces a segment/section to: copy
/// `file_bytes` to `vaddr`, then zero-fill whatever's left up to
/// `mem_size` (this is how BSS -- zero-initialized globals that take up no
/// space in the file at all -- gets created out of nothing at load time).
struct Segment {
    vaddr: u64,
    mem_size: u64,
    writable: bool,
    file_bytes: Vec<u8>,
}

struct Image {
    format: Format,
    entry: u64,
    segments: Vec<Segment>,
    /// `(target_vaddr, value)` pairs to write *after* every segment is
    /// mapped -- a PIE's self-relocations (see `parse_elf`'s `ET_DYN`
    /// handling). Empty for every format except a self-relocating ELF
    /// PIE, which is the only one that needs any fixing up after the raw
    /// bytes are in place.
    relocations: Vec<(u64, u64)>,
    /// Relocations that need an actual symbol looked up somewhere else
    /// (this file's own exports, or -- for the main executable -- a
    /// `DT_NEEDED` library's) before they can be written, unlike
    /// `relocations` above which are self-contained. See
    /// [`ExternReloc`]/[`load_and_run`]'s real dynamic-linking pass.
    extern_relocations: Vec<ExternReloc>,
    /// `(target_vaddr, resolver_vaddr)` for every `R_X86_64_IRELATIVE`
    /// (GNU IFUNC) entry in `.rela.dyn` -- unlike every other relocation
    /// type here, the "value" to write isn't computable at all without
    /// actually *calling* `resolver_vaddr` as a function (that's what an
    /// IFUNC is: code that picks its own real implementation, e.g. by
    /// CPUID, at bind time) and using its return value, which this loader
    /// can't do yet -- see `load_and_run`'s own check of this field.
    /// Parsed rather than rejected at parse time so a file that has these
    /// (real `ld.so`/`libc.so` builds routinely do) can still be loaded on
    /// the [`load_and_run_with_interp`] path, which never applies any
    /// relocations from this list anyway -- the interpreter resolves its
    /// own IFUNCs itself, in userspace, exactly like every other
    /// relocation on that path.
    irelative_relocations: Vec<(u64, u64)>,
    /// `DT_NEEDED` entries this file's `PT_DYNAMIC` names, in order --
    /// only ever non-empty for the main executable; a loaded library's own
    /// transitive dependencies aren't followed (see `load_and_run`'s doc
    /// comment).
    needed: Vec<String>,
    /// `(name, absolute_address)` for every globally-bound, defined symbol
    /// in this file's own dynamic symbol table -- what `load_and_run`
    /// collects from each loaded `DT_NEEDED` library to resolve the main
    /// executable's `extern_relocations` against.
    exports: Vec<(String, u64)>,
    /// This ELF's program header table, already relocated to a real
    /// runtime address (`0` for a format other than ELF, or if it
    /// couldn't be located -- see `parse_elf_at`'s computation) -- what
    /// `load_and_run` puts in the real Linux initial stack's `AT_PHDR`/
    /// `AT_PHENT`/`AT_PHNUM` auxv entries. A statically-linked musl
    /// binary's own startup code (`__init_tls`) walks this table itself,
    /// hunting for a `PT_TLS` segment, before it ever calls `arch_prctl`
    /// -- see `linux_syscall.rs`'s module docs for the `strace`-derived
    /// reasoning that led here. Real Linux computes this exact same way:
    /// the kernel doesn't parse its own phdr table specially either, it
    /// just knows the first `PT_LOAD` segment's file offset is 0 (true of
    /// every sane linker's output, this project's own included) and
    /// reports `phdr_vaddr = that segment's runtime base + e_phoff`.
    phdr_vaddr: u64,
    phentsize: u64,
    phnum: u64,
    /// This ELF's `PT_INTERP` path (e.g. `/libc.so`), if it has one --
    /// `None` for everything except a *real*, dynamically-linked Linux
    /// binary. See [`load_and_run_with_interp`] for what this actually
    /// changes: a file with an interpreter gets loaded through a
    /// completely different, much simpler path than `load_and_run`'s own
    /// `DT_NEEDED` scheme above -- no relocations of any kind applied by
    /// this kernel at all, for either this file or the interpreter, since
    /// that's now 100% the interpreter's own job, done in userspace after
    /// it's handed control. See this module's own doc comment for the
    /// full reasoning.
    interp: Option<String>,
}

/// One relocation whose value is "wherever symbol `sym_name` ends up
/// loaded", not something computable from this file alone -- the actual,
/// minimal definition of *external* dynamic linking, as opposed to a
/// PIE's purely self-referential `R_X86_64_RELATIVE` fixups. `target_vaddr`
/// is already bias-adjusted, same as `Image::relocations`'s pairs.
struct ExternReloc {
    target_vaddr: u64,
    sym_name: String,
    kind: u32,
    addend: i64,
}

/// Looks at a file's first few bytes to figure out which of the three
/// formats it is. This is the actual "dynamic" part of the loader: nothing
/// downstream (mapping, spawning) needs to know or care which one it
/// picked -- see the module docs.
pub fn detect(bytes: &[u8]) -> Format {
    if bytes.len() >= 4 && &bytes[0..4] == b"\x7fELF" {
        Format::Elf
    } else if bytes.len() >= 2 && &bytes[0..2] == b"MZ" {
        Format::Pe
    } else {
        Format::Flat
    }
}

fn read_u16(bytes: &[u8], off: usize) -> Result<u16, &'static str> {
    bytes
        .get(off..off + 2)
        .map(|s| u16::from_le_bytes([s[0], s[1]]))
        .ok_or("loader: file too short (truncated header)")
}

fn read_u32(bytes: &[u8], off: usize) -> Result<u32, &'static str> {
    bytes
        .get(off..off + 4)
        .map(|s| u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
        .ok_or("loader: file too short (truncated header)")
}

fn read_u64(bytes: &[u8], off: usize) -> Result<u64, &'static str> {
    bytes
        .get(off..off + 8)
        .map(|s| u64::from_le_bytes([s[0], s[1], s[2], s[3], s[4], s[5], s[6], s[7]]))
        .ok_or("loader: file too short (truncated header)")
}

// --- Flat binary -----------------------------------------------------------

/// No header at all: the entire file *is* the program, loaded verbatim at
/// a fixed address with its own start as the entry point. Mapped
/// writable+executable across the board since there's nothing in a flat
/// binary that could say otherwise (no equivalent of ELF's `PF_W` or PE's
/// section characteristics) -- the simplest possible container, and
/// correspondingly the least safe/expressive one.
fn parse_flat(bytes: &[u8]) -> Image {
    let mut segments = Vec::new();
    segments.push(Segment {
        vaddr: FLAT_BASE,
        mem_size: bytes.len() as u64,
        writable: true,
        file_bytes: bytes.to_vec(),
    });
    Image {
        format: Format::Flat,
        entry: FLAT_BASE,
        segments,
        relocations: Vec::new(),
        extern_relocations: Vec::new(),
        irelative_relocations: Vec::new(),
        needed: Vec::new(),
        exports: Vec::new(),
        phdr_vaddr: 0,
        phentsize: 0,
        phnum: 0,
        interp: None,
    }
}

// --- ELF64 -------------------------------------------------------------

const PT_LOAD: u32 = 1;
const PT_DYNAMIC: u32 = 2;
const PT_INTERP: u32 = 3;
const PF_W: u32 = 1 << 1;

const DT_NULL: u64 = 0;
const DT_NEEDED: u64 = 1;
const DT_PLTRELSZ: u64 = 2;
const DT_HASH: u64 = 4;
const DT_STRTAB: u64 = 5;
const DT_SYMTAB: u64 = 6;
const DT_RELA: u64 = 7;
const DT_RELASZ: u64 = 8;
const DT_STRSZ: u64 = 10;
const DT_JMPREL: u64 = 23;
// DT_RELRSZ/DT_RELR/DT_RELRENT: the compact "RELR" relative-relocation
// format -- binutils/glibc's default for a self-relocating binary's own
// R_X86_64_RELATIVE-only fixups since ~2021 (ld's `-z pack-relative-relocs`,
// on by default). A real `ld-linux-x86-64.so.2` build carries its own
// self-relocations here instead of in `.rela.dyn` -- see the DT_RELR
// decoding loop in `parse_elf_at` for the actual bitmap format.
const DT_RELRSZ: u64 = 35;
const DT_RELR: u64 = 36;
const DT_RELRENT: u64 = 37;
const RELA_ENTRY_SIZE: u64 = 24; // Elf64_Rela: r_offset(8) + r_info(8) + r_addend(8)
const SYM_ENTRY_SIZE: u64 = 24; // Elf64_Sym: st_name(4) + st_info(1) + st_other(1) + st_shndx(2) + st_value(8) + st_size(8)

const R_X86_64_64: u32 = 1;
const R_X86_64_GLOB_DAT: u32 = 6;
const R_X86_64_JUMP_SLOT: u32 = 7;
const R_X86_64_RELATIVE: u32 = 8;
// GNU IFUNC: the relocation's "value" is what calling the function at
// `bias + r_addend` returns, not `bias + r_addend` itself -- see
// `Image::irelative_relocations`'s doc comment for why this loader parses
// but doesn't yet resolve these.
const R_X86_64_IRELATIVE: u32 = 37;

/// Where a PIE (`ET_DYN`) executable's segments get placed. KonjacOS
/// doesn't implement real load-address randomization (ASLR) -- just one
/// more fixed region, distinct from `FLAT_BASE`/the stack/`brk`/`mmap`,
/// for the same "no collision, every task has its own private address
/// space" reasoning those already rely on.
const PIE_BASE: u64 = 0x0000_0048_0000_0000;

/// Where the first `DT_NEEDED` shared library a program loads gets placed
/// -- a region of its own, distinct from every other fixed address this
/// loader hands out. Each additional library gets the next `LIB_SLOT_SIZE`
/// slice up from there (see `load_and_run`); like `PIE_BASE`, this is a
/// fixed placement, not real ASLR, which is fine for the same reason it's
/// fine everywhere else here: every task gets its own private address
/// space, so there's nothing else that could ever collide with it.
const LIB_BASE: u64 = 0x0000_0044_0000_0000;
/// How much address space each loaded library gets to itself -- 256 MiB,
/// far more than any of KonjacOS's own hand-assembled demo libraries need,
/// but cheap to reserve since it's just address space, not memory, until
/// something actually gets mapped into it.
const LIB_SLOT_SIZE: u64 = 0x1000_0000;
/// How many `DT_NEEDED` libraries a single program can depend on. A small,
/// fixed cap -- like `task::MAX_TASKS` -- rather than anything the loader
/// needs to grow dynamically; real dynamic linking's *transitive* closure
/// (a library depending on another library) isn't implemented at all yet,
/// so in practice this is a ceiling on one executable's *direct*
/// dependency count.
const MAX_LIBS: usize = 8;

/// Where a real `PT_INTERP` dynamic linker (e.g. musl's `libc.so`, which
/// doubles as its own `ld.so` -- see [`load_and_run_with_interp`]) gets
/// placed. A region of its own, distinct from every other fixed address
/// this loader hands out (`FLAT_BASE`=`0x40...`, `LIB_BASE`=`0x44...`,
/// `PIE_BASE`=`0x48...`, with plenty of room between `LIB_BASE`'s own
/// `MAX_LIBS`-sized span and `PIE_BASE` to fit this) -- same "fixed
/// placement is fine because every task gets its own private address
/// space" reasoning as the others.
const INTERP_BASE: u64 = 0x0000_0046_0000_0000;

/// Finds which mapped `PT_LOAD` segment (`raw_loads`, each an original
/// pre-bias `(vaddr, file_offset, file_size)`) contains `vaddr`, and
/// returns the corresponding file offset -- the same "which segment's
/// file-backed range contains this address" translation every `PT_DYNAMIC`
/// tag whose value is itself a vaddr (`DT_STRTAB`, `DT_SYMTAB`, `DT_RELA`,
/// `DT_JMPREL`, `DT_HASH`, ...) needs before this loader (which only ever
/// looks at the file's own bytes, never a live mapping while parsing) can
/// actually read through it.
fn vaddr_to_file_offset(raw_loads: &[(u64, u64, u64)], vaddr: u64) -> Option<u64> {
    raw_loads.iter().find_map(|&(pv, po, pf)| if vaddr >= pv && vaddr < pv + pf { Some(po + (vaddr - pv)) } else { None })
}

/// Reads a NUL-terminated string out of `bytes` starting at `off` -- what
/// both `DT_NEEDED` (a library's soname) and every dynamic symbol's
/// `st_name` point into (the file's `.dynstr` section, located via
/// `DT_STRTAB`).
fn read_cstr(bytes: &[u8], off: usize) -> Result<&str, &'static str> {
    let rest = bytes.get(off..).ok_or("loader: dynamic string table entry points outside the file")?;
    let len = rest.iter().position(|&b| b == 0).ok_or("loader: unterminated string in dynamic string/symbol table")?;
    core::str::from_utf8(&rest[..len]).map_err(|_| "loader: dynamic string/symbol table entry isn't valid UTF-8")
}

/// Parses a 64-bit little-endian x86_64 ELF executable: walks the program
/// header table and turns every `PT_LOAD` entry into a [`Segment`]. Two
/// shapes are accepted:
///
/// `ET_EXEC` (static, non-PIE): every address in the file is already
/// absolute, so `bias` is 0 and nothing further is needed -- this is the
/// original, simpler case.
///
/// `ET_DYN` (PIE): every address in the file is *relative to wherever it
/// ends up loaded*, so this picks a fixed [`PIE_BASE`] and adds it to
/// every segment's `vaddr` and to the entry point. That alone would be
/// enough for code with no absolute pointers anywhere, but real compiled
/// code almost always has at least one (a global variable holding another
/// global's address, a vtable, ...) -- those show up as entries in the
/// PIE's `PT_DYNAMIC` segment's relocation table (`.rela.dyn`, found via
/// the `DT_RELA`/`DT_RELASZ` tags in the `Elf64_Dyn` array), and *fixing
/// those up* is the actual, minimal definition of "self-relocation": for
/// each `R_X86_64_RELATIVE` entry, write `bias + r_addend` at
/// `bias + r_offset`, once the segment holding that address is mapped
/// (see `load_and_run`, which applies [`Image::relocations`] after
/// mapping). This is real, working dynamic-linking groundwork, not a
/// simulation of it -- verified against a real `ld -pie`-linked binary,
/// not just written to compile -- but it's still only the *self*-relocating
/// slice: a relocation of any other type means resolving a symbol against
/// something outside this one file, which needs an actual dynamic linker
/// (a `DT_SYMTAB`/`DT_STRTAB` walk, matching against other loaded
/// objects) this kernel doesn't have, so that -- and any `DT_NEEDED`
/// dependency on an external shared library, which needs a dynamic linker
/// for an entirely different reason (there's nothing here that can load a
/// second file at all) -- gets rejected outright with a clear reason,
/// exactly like `parse_pe`'s import-table check rejects a `.exe` that
/// needs real DLLs.
fn parse_elf(bytes: &[u8]) -> Result<Image, &'static str> {
    parse_elf_at(bytes, None)
}

/// The real implementation behind [`parse_elf`]: identical for the main
/// executable and for a `DT_NEEDED` library `load_and_run` loads on its
/// behalf, except for `forced_bias` -- `None` picks the bias the normal
/// way (0 for `ET_EXEC`, [`PIE_BASE`] for a bare `ET_DYN`), while `Some`
/// is how `load_and_run` places each library at its own [`LIB_BASE`]-
/// derived slot instead. Everything past that (segments, self-relocations,
/// and now also `DT_NEEDED` names, `extern_relocations`, and this file's
/// own exported dynamic symbols) applies equally to either caller -- a
/// library is just an `ET_DYN` file like a PIE executable is, parsed by
/// the exact same code, only ever *loaded* differently.
fn parse_elf_at(bytes: &[u8], forced_bias: Option<u64>) -> Result<Image, &'static str> {
    if bytes.len() < 64 {
        return Err("loader: ELF file too short to hold an ELF64 header");
    }
    if &bytes[0..4] != b"\x7fELF" {
        return Err("loader: not an ELF file (bad magic)");
    }
    if bytes[4] != 2 {
        return Err("loader: only 64-bit ELF (EI_CLASS=ELFCLASS64) is supported");
    }
    if bytes[5] != 1 {
        return Err("loader: only little-endian ELF (EI_DATA=ELFDATA2LSB) is supported");
    }

    let e_type = read_u16(bytes, 16)?;
    if e_type != 2 && e_type != 3 {
        return Err("loader: only ET_EXEC (static) or ET_DYN (PIE/shared library) ELF files are supported");
    }
    let is_pie = e_type == 3;
    let bias: u64 = forced_bias.unwrap_or(if is_pie { PIE_BASE } else { 0 });
    let e_machine = read_u16(bytes, 18)?;
    if e_machine != 0x3E {
        return Err("loader: not an x86_64 (EM_X86_64) ELF executable");
    }

    let e_entry = read_u64(bytes, 24)?;
    let e_phoff = read_u64(bytes, 32)? as usize;
    let e_phentsize = read_u16(bytes, 54)? as usize;
    let e_phnum = read_u16(bytes, 56)? as usize;

    let mut segments = Vec::new();
    // Every PT_LOAD's *original* (pre-bias) vaddr/offset/filesz, purely so
    // DT_RELA's d_val (itself an original, pre-bias vaddr -- it's part of
    // the same not-yet-relocated file this loop is already walking) can be
    // translated back to a file offset below, the same "which segment's
    // file-backed range contains this address" trick `rva_to_file_offset`
    // uses for PE.
    let mut raw_loads: Vec<(u64, u64, u64)> = Vec::new();
    let mut dynamic_range: Option<(usize, usize)> = None;
    let mut interp: Option<String> = None;
    for i in 0..e_phnum {
        let ph = e_phoff + i * e_phentsize;
        let p_type = read_u32(bytes, ph)?;
        match p_type {
            PT_INTERP => {
                // A real Linux binary's requested dynamic linker path
                // (e.g. `/libc.so` -- musl uniquely makes its own libc.so
                // double as its own ld.so, unlike glibc's separate
                // ld-linux.so.2). NUL-terminated within the segment's own
                // file bytes, same shape as every other ELF string this
                // loader reads. See load_and_run_with_interp for what
                // having this actually changes.
                let p_offset = read_u64(bytes, ph + 8)? as usize;
                let p_filesz = read_u64(bytes, ph + 32)? as usize;
                let raw = bytes.get(p_offset..p_offset + p_filesz).ok_or("loader: PT_INTERP program header points outside the file")?;
                let len = raw.iter().position(|&b| b == 0).unwrap_or(raw.len());
                interp = Some(core::str::from_utf8(&raw[..len]).map_err(|_| "loader: PT_INTERP path isn't valid UTF-8")?.to_string());
            }
            PT_LOAD => {
                let p_flags = read_u32(bytes, ph + 4)?;
                let p_offset = read_u64(bytes, ph + 8)?;
                let p_vaddr = read_u64(bytes, ph + 16)?;
                let p_filesz = read_u64(bytes, ph + 32)?;
                let p_memsz = read_u64(bytes, ph + 40)?;

                if p_filesz > p_memsz {
                    return Err("loader: ELF PT_LOAD segment's file size exceeds its memory size");
                }
                let file_bytes = bytes
                    .get(p_offset as usize..(p_offset + p_filesz) as usize)
                    .ok_or("loader: ELF program header points outside the file")?
                    .to_vec();

                segments.push(Segment {
                    vaddr: p_vaddr.wrapping_add(bias),
                    mem_size: p_memsz,
                    writable: p_flags & PF_W != 0,
                    file_bytes,
                });
                raw_loads.push((p_vaddr, p_offset, p_filesz));
            }
            PT_DYNAMIC => {
                let p_offset = read_u64(bytes, ph + 8)? as usize;
                let p_filesz = read_u64(bytes, ph + 32)? as usize;
                dynamic_range = Some((p_offset, p_filesz));
            }
            _ => {}
        }
    }

    if segments.is_empty() {
        return Err("loader: ELF file has no PT_LOAD segments -- nothing to run");
    }

    let mut relocations = Vec::new();
    let mut extern_relocations = Vec::new();
    let mut irelative_relocations = Vec::new();
    let mut needed = Vec::new();
    let mut exports = Vec::new();

    if let Some((dyn_off, dyn_size)) = dynamic_range {
        // First pass: just collect every tag's raw value. DT_STRTAB/
        // DT_SYMTAB and friends are vaddrs into *this* file, so nothing
        // that depends on them (DT_NEEDED's actual name, a symbol's actual
        // name) can be resolved until every tag has been seen -- the
        // dynamic section's tags aren't guaranteed to appear in any
        // particular order.
        let mut needed_vaddrs = Vec::new();
        let mut strtab_vaddr: Option<u64> = None;
        let mut symtab_vaddr: Option<u64> = None;
        let mut hash_vaddr: Option<u64> = None;
        let mut rela_vaddr: Option<u64> = None;
        let mut rela_size: Option<u64> = None;
        let mut jmprel_vaddr: Option<u64> = None;
        let mut pltrel_size: Option<u64> = None;
        let mut strsz: Option<u64> = None;
        let mut relr_vaddr: Option<u64> = None;
        let mut relr_size: Option<u64> = None;
        let mut relr_ent: Option<u64> = None;

        let mut off = dyn_off;
        let end = dyn_off + dyn_size;
        while off + 16 <= end {
            let tag = read_u64(bytes, off)?;
            let val = read_u64(bytes, off + 8)?;
            match tag {
                DT_NULL => break,
                DT_NEEDED => needed_vaddrs.push(val),
                DT_STRTAB => strtab_vaddr = Some(val),
                DT_SYMTAB => symtab_vaddr = Some(val),
                DT_HASH => hash_vaddr = Some(val),
                DT_RELA => rela_vaddr = Some(val),
                DT_RELASZ => rela_size = Some(val),
                DT_JMPREL => jmprel_vaddr = Some(val),
                DT_PLTRELSZ => pltrel_size = Some(val),
                DT_STRSZ => strsz = Some(val),
                DT_RELR => relr_vaddr = Some(val),
                DT_RELRSZ => relr_size = Some(val),
                DT_RELRENT => relr_ent = Some(val),
                _ => {}
            }
            off += 16;
        }

        // DT_STRTAB is what every name below (DT_NEEDED's soname, every
        // symbol's st_name) is an offset into, so nothing that names
        // anything can proceed without it -- but a file with *no* strings
        // to resolve (no DT_NEEDED, no exported symbols) legitimately has
        // no DT_STRTAB at all, so this only errors once something actually
        // needs it.
        let strtab_off = match strtab_vaddr {
            Some(v) => Some(vaddr_to_file_offset(&raw_loads, v).ok_or("loader: couldn't locate this file's dynamic string table (DT_STRTAB) in the file")?),
            None => None,
        };

        for &nv in &needed_vaddrs {
            let strtab_off = strtab_off.ok_or("loader: DT_NEEDED entry with no DT_STRTAB to read its name from")?;
            let name = read_cstr(bytes, (strtab_off + nv) as usize)?;
            needed.push(name.to_string());
        }
        let _ = strsz; // Only ever consulted indirectly, via read_cstr's own bounds check.

        // R_X86_64_RELATIVE self-relocations -- unchanged from the
        // PIE-only self-relocation support this grew out of, just no
        // longer gated on `is_pie`: a dynamically-linked ET_EXEC's
        // .rela.dyn can carry the exact same self-relocations a PIE's can
        // (e.g. RELRO's .data.rel.ro pointers), and can *also* mix in
        // GLOB_DAT/R_X86_64_64 entries that need real symbol resolution
        // (see extern_relocations below) -- both live in the same table.
        if let (Some(rela_vaddr), Some(rela_size)) = (rela_vaddr, rela_size) {
            let rela_offset = vaddr_to_file_offset(&raw_loads, rela_vaddr).ok_or("loader: couldn't locate this file's relocation table (DT_RELA) in the file")?;
            let count = rela_size / RELA_ENTRY_SIZE;
            for i in 0..count {
                let entry = (rela_offset + i * RELA_ENTRY_SIZE) as usize;
                let r_offset = read_u64(bytes, entry)?;
                let r_info = read_u64(bytes, entry + 8)?;
                let r_addend = read_u64(bytes, entry + 16)? as i64;
                let r_type = (r_info & 0xFFFF_FFFF) as u32;
                let r_sym = (r_info >> 32) as u64;

                if r_type == R_X86_64_RELATIVE {
                    relocations.push((bias.wrapping_add(r_offset), bias.wrapping_add(r_addend as u64)));
                } else if r_type == R_X86_64_64 || r_type == R_X86_64_GLOB_DAT {
                    let symtab_off = symtab_vaddr.and_then(|v| vaddr_to_file_offset(&raw_loads, v)).ok_or("loader: relocation needs a symbol but this file has no DT_SYMTAB")?;
                    let strtab_off = strtab_off.ok_or("loader: relocation needs a symbol name but this file has no DT_STRTAB")?;
                    let sym_off = (symtab_off + r_sym * SYM_ENTRY_SIZE) as usize;
                    let st_name = read_u32(bytes, sym_off)?;
                    let name = read_cstr(bytes, (strtab_off + st_name as u64) as usize)?;
                    extern_relocations.push(ExternReloc { target_vaddr: bias.wrapping_add(r_offset), sym_name: name.to_string(), kind: r_type, addend: r_addend });
                } else if r_type == R_X86_64_IRELATIVE {
                    irelative_relocations.push((bias.wrapping_add(r_offset), bias.wrapping_add(r_addend as u64)));
                } else {
                    return Err("loader: this file has a .rela.dyn relocation type KonjacOS's loader doesn't support yet");
                }
            }
        }

        // DT_RELR: a real ld.so build's own self-relocations (and,
        // increasingly, any binary linked with a modern `ld`) mostly live
        // here instead of in `.rela.dyn`, as a packed bitmap rather than
        // one 24-byte Elf64_Rela entry per fixup. Every entry this format
        // describes is a plain R_X86_64_RELATIVE-style fixup -- the same
        // "write bias + addend at bias + offset" `relocations` above
        // already carries -- just with the addend left implicit: it's
        // whatever pointer value the linker already wrote at that address
        // (computed as if the file had loaded at bias 0), read back out of
        // the file's own bytes here rather than out of a relocation-table
        // entry. See the generic-ABI RELR proposal (Fangrui Song,
        // "Relative relocations and RELR") for the format this decodes:
        // each 8-byte word in the table is either an even *address* (the
        // next fixup site) or an odd *bitmap*, whose bits 1..63 each mean
        // "address + that-bit-index * 8 also needs fixing", letting up to
        // 63 more fixups ride along without their own address word.
        if let (Some(relr_vaddr), Some(relr_size)) = (relr_vaddr, relr_size) {
            if let Some(entry_size) = relr_ent {
                if entry_size != 8 {
                    return Err("loader: this file's DT_RELRENT isn't 8 bytes, which KonjacOS's RELR decoder doesn't support");
                }
            }
            let relr_offset = vaddr_to_file_offset(&raw_loads, relr_vaddr).ok_or("loader: couldn't locate this file's RELR relative relocation table (DT_RELR) in the file")?;
            let count = relr_size / 8;
            let mut addr: u64 = 0;
            for i in 0..count {
                let word = read_u64(bytes, (relr_offset + i * 8) as usize)?;
                if word & 1 == 0 {
                    addr = word;
                    let file_off = vaddr_to_file_offset(&raw_loads, addr).ok_or("loader: a DT_RELR address entry doesn't fall inside any PT_LOAD segment")?;
                    let old_value = read_u64(bytes, file_off as usize)?;
                    relocations.push((bias.wrapping_add(addr), bias.wrapping_add(old_value)));
                    addr += 8;
                } else {
                    let mut bitmap = word >> 1;
                    let mut site = addr;
                    while bitmap != 0 {
                        site += 8;
                        if bitmap & 1 != 0 {
                            let file_off = vaddr_to_file_offset(&raw_loads, site).ok_or("loader: a DT_RELR bitmap entry doesn't fall inside any PT_LOAD segment")?;
                            let old_value = read_u64(bytes, file_off as usize)?;
                            relocations.push((bias.wrapping_add(site), bias.wrapping_add(old_value)));
                        }
                        bitmap >>= 1;
                    }
                    addr += 8 * 63;
                }
            }
        }

        // .rela.plt (DT_JMPREL/DT_PLTRELSZ): the GOT slots every PLT stub
        // `ld` auto-generated for a call to an external function jumps
        // through. KonjacOS never actually leaves PLT0's lazy-binding
        // stub in play -- there's no runtime resolver to jump to -- so
        // this resolves every one of these eagerly, at load time, exactly
        // like a real dynamic linker running with `LD_BIND_NOW`: by the
        // time this task's first instruction runs, every GOT slot a PLT
        // stub reads already holds the real, final address, so the stub's
        // own `jmp *GOT[n]` lands correctly on the very first call.
        if let (Some(jmprel_vaddr), Some(pltrel_size)) = (jmprel_vaddr, pltrel_size) {
            let symtab_off = symtab_vaddr.and_then(|v| vaddr_to_file_offset(&raw_loads, v)).ok_or("loader: this file's .rela.plt needs DT_SYMTAB, which is missing")?;
            let strtab_off = strtab_off.ok_or("loader: this file's .rela.plt needs DT_STRTAB, which is missing")?;
            let jmprel_offset = vaddr_to_file_offset(&raw_loads, jmprel_vaddr).ok_or("loader: couldn't locate this file's PLT relocation table (DT_JMPREL) in the file")?;
            let count = pltrel_size / RELA_ENTRY_SIZE;
            for i in 0..count {
                let entry = (jmprel_offset + i * RELA_ENTRY_SIZE) as usize;
                let r_offset = read_u64(bytes, entry)?;
                let r_info = read_u64(bytes, entry + 8)?;
                let r_addend = read_u64(bytes, entry + 16)? as i64;
                let r_type = (r_info & 0xFFFF_FFFF) as u32;
                let r_sym = (r_info >> 32) as u64;

                if r_type != R_X86_64_JUMP_SLOT {
                    return Err("loader: this file's .rela.plt has a relocation type other than R_X86_64_JUMP_SLOT, which KonjacOS's loader doesn't support");
                }
                let sym_off = (symtab_off + r_sym * SYM_ENTRY_SIZE) as usize;
                let st_name = read_u32(bytes, sym_off)?;
                let name = read_cstr(bytes, (strtab_off + st_name as u64) as usize)?;
                extern_relocations.push(ExternReloc { target_vaddr: bias.wrapping_add(r_offset), sym_name: name.to_string(), kind: r_type, addend: r_addend });
            }
        }

        // This file's own exported symbols -- irrelevant for a main
        // executable (nothing loads a program *as* someone else's
        // library), but what makes a loaded DT_NEEDED library useful at
        // all: load_and_run collects every library's exports into one
        // table to resolve the main executable's extern_relocations
        // against. DT_HASH's second word (nchain) is used as the dynamic
        // symbol table's entry count -- see read_cstr's doc comment and
        // build.sh's `--hash-style=sysv` for why that tag is guaranteed to
        // exist here rather than only the newer GNU-hash section, which
        // doesn't carry an explicit count the same simple way.
        if let (Some(symtab_vaddr), Some(hash_vaddr), Some(strtab_off)) = (symtab_vaddr, hash_vaddr, strtab_off) {
            let symtab_off = vaddr_to_file_offset(&raw_loads, symtab_vaddr).ok_or("loader: couldn't locate this file's dynamic symbol table (DT_SYMTAB) in the file")?;
            let hash_off = vaddr_to_file_offset(&raw_loads, hash_vaddr).ok_or("loader: couldn't locate this file's symbol hash table (DT_HASH) in the file")?;
            let nchain = read_u32(bytes, (hash_off + 4) as usize)? as u64;

            for i in 0..nchain {
                let sym_off = (symtab_off + i * SYM_ENTRY_SIZE) as usize;
                let st_name = read_u32(bytes, sym_off)?;
                let st_info = bytes.get(sym_off + 4).copied().ok_or("loader: file too short (truncated dynamic symbol table)")?;
                let st_shndx = read_u16(bytes, sym_off + 6)?;
                let st_value = read_u64(bytes, sym_off + 8)?;
                let bind = st_info >> 4;
                const STB_LOCAL: u8 = 0;
                const SHN_UNDEF: u16 = 0;
                // A defined (SHN_UNDEF means "not defined here"), non-local
                // (STB_LOCAL symbols aren't meant to be visible outside
                // this file at all, dynamic-symbol-table presence or not)
                // symbol with an actual name is a real export.
                if st_name != 0 && st_shndx != SHN_UNDEF && bind != STB_LOCAL {
                    let name = read_cstr(bytes, (strtab_off + st_name as u64) as usize)?;
                    exports.push((name.to_string(), bias.wrapping_add(st_value)));
                }
            }
        }
    }

    // AT_PHDR: real Linux computes it as "the first PT_LOAD segment's
    // runtime base + e_phoff", relying on every sane linker emitting that
    // segment's file offset as 0 (so it covers the ELF header and phdr
    // table itself) -- see Image::phdr_vaddr's doc comment. `0` (an
    // honestly-absent auxv entry, not a wrong one) if that assumption
    // doesn't hold for this particular file.
    //
    // This was previously missing the `+ e_phoff` the doc comment (and
    // real Linux) both describe -- silently wrong by e_phoff bytes (64,
    // for every binary this loader has ever built) for every prior item,
    // but harmless in practice for a *statically*-linked musl binary with
    // no PT_TLS segment (item 20/21/22's demos): a phdr walk that's off by
    // one header's worth of bytes just fails to find a segment type it was
    // never going to find anyway, silently. It stopped being harmless the
    // moment something actually load-bearing depended on AT_PHDR being
    // exactly right: a real PT_INTERP dynamic linker (this item) walks
    // this exact table to find the main program's own PT_DYNAMIC segment,
    // and a wrong AT_PHDR there means reading whatever garbage happens to
    // sit 64 bytes off from a real program header as if it were one --
    // caught the hard way, via a real page fault (CR2=0) inside musl's own
    // `decode_dyn`, not by inspection.
    let phdr_vaddr = raw_loads.iter().find(|&&(_, po, _)| po == 0).map(|&(pv, _, _)| pv.wrapping_add(bias) + e_phoff as u64).unwrap_or(0);

    Ok(Image {
        format: Format::Elf,
        entry: e_entry.wrapping_add(bias),
        segments,
        relocations,
        extern_relocations,
        irelative_relocations,
        needed,
        exports,
        phdr_vaddr,
        phentsize: e_phentsize as u64,
        phnum: e_phnum as u64,
        interp,
    })
}

// --- PE32+ (.exe) --------------------------------------------------------

const IMAGE_FILE_MACHINE_AMD64: u16 = 0x8664;
const PE32_PLUS_MAGIC: u16 = 0x20b;
const IMAGE_SCN_MEM_WRITE: u32 = 0x8000_0000;

struct SectionHeader {
    virtual_address: u32,
    virtual_size: u32,
    size_of_raw_data: u32,
    ptr_to_raw_data: u32,
    characteristics: u32,
}

/// Translates a PE "RVA" (an offset from `ImageBase`, the currency every
/// data directory -- imports, exports, relocations, ... -- is expressed
/// in) to a byte offset in the file itself, by finding which section
/// contains it. Returns `None` for an RVA that lands in a section with no
/// file-backed data at all (e.g. pure BSS), since there's nothing on disk
/// to point at.
fn rva_to_file_offset(sections: &[SectionHeader], rva: u32) -> Option<usize> {
    for s in sections {
        let span = s.virtual_size.max(s.size_of_raw_data);
        if rva >= s.virtual_address && rva < s.virtual_address + span {
            if s.ptr_to_raw_data == 0 {
                return None;
            }
            return Some((s.ptr_to_raw_data + (rva - s.virtual_address)) as usize);
        }
    }
    None
}

/// Parses a 64-bit ("PE32+") Windows executable: DOS stub -> `PE\0\0`
/// signature -> COFF file header -> optional header -> section table, each
/// section becoming a [`Segment`]. Refuses anything with a *real* import
/// table (data directory 1): a real `.exe` resolves those against actual
/// DLLs (`kernel32.dll` and friends) via a step this kernel simply doesn't
/// have, so rather than loading a binary that's guaranteed to crash the
/// instant it calls an unresolved import, this rejects it up front with an
/// explanation. Note this isn't just "is the import directory's size zero"
/// -- even a self-contained binary with no imports at all typically still
/// gets an import directory entry from the linker (mingw's default PE
/// linker script always lays one out), just one whose first descriptor is
/// entirely zero (the standard "no more entries" terminator) -- so this
/// walks the actual descriptor and only rejects a *non-empty* one. What's
/// left -- self-contained PE binaries that only use KonjacOS's own
/// `int 0x80` syscalls -- is exactly what this loader is for; see the
/// module docs.
fn parse_pe(bytes: &[u8]) -> Result<Image, &'static str> {
    if bytes.len() < 0x40 {
        return Err("loader: PE file too short to hold a DOS header");
    }
    if &bytes[0..2] != b"MZ" {
        return Err("loader: not a PE file (bad DOS signature)");
    }
    let e_lfanew = read_u32(bytes, 0x3C)? as usize;
    if bytes.get(e_lfanew..e_lfanew + 4) != Some(b"PE\0\0".as_slice()) {
        return Err("loader: not a PE file (bad PE signature)");
    }

    let coff = e_lfanew + 4;
    let machine = read_u16(bytes, coff)?;
    if machine != IMAGE_FILE_MACHINE_AMD64 {
        return Err("loader: not an x86_64 (IMAGE_FILE_MACHINE_AMD64) PE executable");
    }
    let num_sections = read_u16(bytes, coff + 2)? as usize;
    let size_of_opt_header = read_u16(bytes, coff + 16)? as usize;

    let opt = coff + 20;
    let magic = read_u16(bytes, opt)?;
    if magic != PE32_PLUS_MAGIC {
        return Err("loader: only PE32+ (64-bit) executables are supported, not classic 32-bit PE32");
    }
    let entry_rva = read_u32(bytes, opt + 16)?;
    let image_base = read_u64(bytes, opt + 24)?;
    let num_rva_and_sizes = read_u32(bytes, opt + 108)?;

    let section_table = opt + size_of_opt_header;
    let mut sections = Vec::new();
    for i in 0..num_sections {
        let sh = section_table + i * 40;
        sections.push(SectionHeader {
            virtual_address: read_u32(bytes, sh + 12)?,
            virtual_size: read_u32(bytes, sh + 8)?,
            size_of_raw_data: read_u32(bytes, sh + 16)?,
            ptr_to_raw_data: read_u32(bytes, sh + 20)?,
            characteristics: read_u32(bytes, sh + 36)?,
        });
    }

    // Data directory 1 is the import table (RVA, then size, 8 bytes per
    // entry starting at +112) -- see this function's doc comment for why a
    // non-empty *descriptor* (not just a non-empty directory entry) is
    // what actually means "reject".
    if num_rva_and_sizes > 1 {
        let import_rva = read_u32(bytes, opt + 112 + 8)?;
        let import_size = read_u32(bytes, opt + 112 + 8 + 4)?;
        if import_size != 0 {
            let has_real_import = match rva_to_file_offset(&sections, import_rva) {
                Some(off) => bytes.get(off..off + 20).is_none_or(|d| d.iter().any(|&b| b != 0)),
                None => true, // can't even locate it -- assume the worst rather than silently ignore it.
            };
            if has_real_import {
                return Err(
                    "loader: this PE imports functions from external DLLs, which KonjacOS can't resolve -- only self-contained .exe files built against KonjacOS's own syscalls can run",
                );
            }
        }
    }

    let mut segments = Vec::new();
    for s in &sections {
        // VirtualSize of 0 is a (rare, older-toolchain) way of saying "use
        // SizeOfRawData instead" -- see the PE/COFF spec's own note on this
        // field.
        let mem_size = if s.virtual_size != 0 { s.virtual_size as u64 } else { s.size_of_raw_data as u64 };
        if mem_size == 0 {
            continue;
        }

        let file_bytes = if s.ptr_to_raw_data == 0 || s.size_of_raw_data == 0 {
            Vec::new() // a pure-BSS section (e.g. `.bss` itself): nothing to copy, just zero-fill.
        } else {
            let copy_len = (s.size_of_raw_data as u64).min(mem_size) as usize;
            bytes
                .get(s.ptr_to_raw_data as usize..s.ptr_to_raw_data as usize + copy_len)
                .ok_or("loader: PE section points outside the file")?
                .to_vec()
        };

        segments.push(Segment {
            vaddr: image_base + s.virtual_address as u64,
            mem_size,
            writable: s.characteristics & IMAGE_SCN_MEM_WRITE != 0,
            file_bytes,
        });
    }

    if segments.is_empty() {
        return Err("loader: PE file has no sections to load");
    }

    Ok(Image {
        format: Format::Pe,
        entry: image_base + entry_rva as u64,
        segments,
        relocations: Vec::new(),
        extern_relocations: Vec::new(),
        irelative_relocations: Vec::new(),
        needed: Vec::new(),
        exports: Vec::new(),
        phdr_vaddr: 0,
        phentsize: 0,
        phnum: 0,
        interp: None,
    })
}

// --- Mapping + spawning ----------------------------------------------------

/// Maps one segment into `pml4`, copying `seg.file_bytes` to `seg.vaddr`
/// and zero-filling everything else up to `seg.mem_size`. Handles
/// `seg.vaddr` not being page-aligned (routine for ELF, where only the
/// *offset within a page* has to match between the file and the virtual
/// address, not the page boundary itself) by rounding down to the
/// containing page and copying each page's data at the right in-page
/// offset -- the same trick every real ELF loader uses.
///
/// # Safety
/// `pml4` must be a valid PML4 physical address (typically fresh out of
/// `paging::new_address_space`), not yet switched to.
unsafe fn map_segment(pml4: u64, seg: &Segment) {
    let hhdm = pmm::hhdm_offset();
    let page_base = seg.vaddr & !(PAGE_SIZE - 1);
    let lead_in = seg.vaddr - page_base;
    let span = lead_in + seg.mem_size;
    let pages = span.div_ceil(PAGE_SIZE);
    let flags = if seg.writable { PAGE_WRITABLE | PAGE_USER } else { PAGE_USER };

    let data_start = lead_in;
    let data_end = lead_in + seg.file_bytes.len() as u64;

    for i in 0..pages {
        let virt = page_base + i * PAGE_SIZE;
        let phys = pmm::alloc_frame().expect("loader: out of memory mapping a segment");
        unsafe {
            paging::map_page_in(pml4, virt, phys, flags);
            core::ptr::write_bytes((hhdm + phys) as *mut u8, 0, PAGE_SIZE as usize);
        }

        let page_start = i * PAGE_SIZE;
        let page_end = page_start + PAGE_SIZE;
        let overlap_start = page_start.max(data_start);
        let overlap_end = page_end.min(data_end);
        if overlap_start < overlap_end {
            let page_offset = (overlap_start - page_start) as usize;
            let file_offset = (overlap_start - data_start) as usize;
            let len = (overlap_end - overlap_start) as usize;
            unsafe {
                core::ptr::copy_nonoverlapping(
                    seg.file_bytes[file_offset..file_offset + len].as_ptr(),
                    (hhdm + phys + page_offset as u64) as *mut u8,
                    len,
                );
            }
        }
    }
}

/// Detects, parses, and spawns a program from raw file bytes (as read by,
/// say, `fat16::read_file`) as a new isolated ring-3 task -- the one entry
/// point `commands.rs`'s `run` shell command needs, regardless of which of
/// the three formats `bytes` turns out to be. Returns the new task's ID and
/// which format it was recognized as, or an error describing why the file
/// couldn't be loaded (never a panic -- a malformed or unsupported file is
/// an ordinary, expected outcome for something reading arbitrary user
/// files, not a kernel bug).
unsafe fn write_u64_at(pml4: u64, target_vaddr: u64, value: u64) -> Result<(), &'static str> {
    let page = target_vaddr & !(PAGE_SIZE - 1);
    let offset = target_vaddr - page;
    let phys = unsafe { paging::translate(pml4, page) }.ok_or("loader: relocation target isn't mapped by any segment")?;
    unsafe {
        core::ptr::write_unaligned((pmm::hhdm_offset() + phys + offset) as *mut u64, value);
    }
    Ok(())
}

/// Same idea as [`write_u64_at`], for an arbitrary byte slice that might
/// span a page boundary (a program name string, say) -- walks forward a
/// page at a time, re-translating at each boundary, rather than assuming
/// the whole write lands in one physical frame.
unsafe fn write_bytes_at(pml4: u64, target_vaddr: u64, data: &[u8]) -> Result<(), &'static str> {
    let hhdm = pmm::hhdm_offset();
    let mut written = 0usize;
    while written < data.len() {
        let vaddr = target_vaddr + written as u64;
        let page = vaddr & !(PAGE_SIZE - 1);
        let offset = (vaddr - page) as usize;
        let phys = unsafe { paging::translate(pml4, page) }.ok_or("loader: initial stack write target isn't mapped")?;
        let chunk = (PAGE_SIZE as usize - offset).min(data.len() - written);
        unsafe {
            core::ptr::copy_nonoverlapping(data[written..written + chunk].as_ptr(), (hhdm + phys + offset as u64) as *mut u8, chunk);
        }
        written += chunk;
    }
    Ok(())
}

// Real Linux auxv (`AT_*`) type constants -- see the kernel's own
// <linux/auxvec.h>. Only the handful a real static musl binary's startup
// path actually reads get used below.
const AT_NULL: u64 = 0;
const AT_PHDR: u64 = 3;
const AT_PHENT: u64 = 4;
const AT_PHNUM: u64 = 5;
const AT_PAGESZ: u64 = 6;
const AT_BASE: u64 = 7;
const AT_FLAGS: u64 = 8;
const AT_ENTRY: u64 = 9;
const AT_UID: u64 = 11;
const AT_EUID: u64 = 12;
const AT_GID: u64 = 13;
const AT_EGID: u64 = 14;
const AT_SECURE: u64 = 23;
const AT_RANDOM: u64 = 25;

/// Builds a *real* Linux process initial stack -- `argc`, `argv[]`,
/// `envp[]`, and an `auxv[]` array, exactly the layout the real
/// `execve(2)` syscall hands a real Linux program, ending with the value
/// at the final stack pointer being `argc` itself. `argv0` is genuinely
/// the caller's own invocation string (`cmd_run`'s `path`), not
/// `spawn_user`'s separate, `'static`, task-table-only `name` -- the two
/// used to be the same string, harmlessly, right up until a real glibc
/// `ld.so`/`libjli.so` (see this crate's own JVM-bring-up work) turned out
/// to actually *read* `argv[0]` back (falling back to it, in the absence
/// of a working `/proc/self/exe`, to locate its own install layout) and
/// got a meaningless constant like `"run:elf"` instead of anything
/// resembling a real path. Real Linux's own `argv[0]` is exactly whatever
/// the caller passed to `execve`, unrelated to whatever bookkeeping name
/// a shell/task table wants to display, so this loader now honestly keeps
/// the two separate instead of conflating them. KonjacOS's own native
/// demo programs (built against `syscall.rs`'s tiny `int 0x80` ABI) never
/// read any of this -- they just start executing -- so building it for
/// *every* program, not only ones known to need it, is harmless, and
/// avoids having to first figure out which ABI a given file was built
/// against before deciding whether to bother.
///
/// This exists because a real `musl-gcc`-built static binary's own crt
/// startup (`_start`/`__libc_start_main`/`__init_tls`) unconditionally
/// dereferences `[rsp]` expecting `argc` to already be there -- confirmed
/// the hard way, not guessed at: without this, `run`ning a real musl
/// binary page-faulted immediately, at exactly `USER_STACK_BASE +
/// USER_STACK_SIZE` (one byte past the mapped stack), because musl's
/// first stack read landed on the unmapped page right past a stack that
/// held nothing at all. `AT_RANDOM` matters for the same reason: musl's
/// stack-protector canary setup reads 16 "random" bytes through it
/// unconditionally, and a missing/zero auxv entry there is a null-pointer
/// read, not a graceful skip. `AT_PHDR`/`AT_PHENT`/`AT_PHNUM` matter for a
/// *different* real reason -- see [`Image::phdr_vaddr`]'s doc comment.
///
/// Returns the final stack pointer (`argc`'s address) `spawn_user` should
/// actually start the task with, 16-byte aligned per the SysV ABI's own
/// initial-process-stack requirement.
///
/// `at_base` is `AT_BASE` -- always `0` (no separate interpreter) for
/// everything this loader ran before this item, but a *real* `PT_INTERP`
/// dynamic linker (see [`load_and_run_with_interp`]) needs its own load
/// bias reported here: it's the one auxv entry that tells a real `ld.so`
/// where *it itself* ended up, as distinct from `AT_PHDR`/`AT_ENTRY`
/// (which always describe the main *program*, not the interpreter,
/// regardless of which one is actually running first -- see
/// `load_and_run_with_interp`'s doc comment).
unsafe fn build_initial_stack(pml4: u64, stack_top: u64, argv: &[String], envp: &[String], image: &Image, at_base: u64) -> Result<u64, &'static str> {
    // Real argv strings, written at the top of the stack in argv[0..]
    // order -- each one's real address (not the string content itself) is
    // what the argv pointer array further down actually needs. `argv[0]`
    // is conventionally the program's own path; `argv[1..]` are whatever
    // extra arguments `cmd_run` parsed off the command line (see its own
    // doc comment) -- e.g. the `-version` in `run java -version`, which a
    // real launcher like `java` inspects before doing anything else.
    let mut cursor = stack_top;
    let mut argv_addrs = Vec::with_capacity(argv.len());
    for arg in argv {
        let mut bytes = arg.as_bytes().to_vec();
        bytes.push(0);
        cursor -= bytes.len() as u64;
        unsafe { write_bytes_at(pml4, cursor, &bytes)? };
        argv_addrs.push(cursor);
    }

    // Real environment strings, written right below argv the same way --
    // each one's real address (not the string content itself) is what the
    // envp pointer array further down actually needs, matching real
    // Linux's own "argv/envp strings live at the top of the stack, the
    // pointer arrays just below them" layout. Empty (`&[]`) for every
    // caller before item 41 -- see `cmd_run`'s own doc comment for the
    // first real source of a non-empty one.
    let mut envp_addrs = Vec::with_capacity(envp.len());
    for var in envp {
        let mut bytes = var.as_bytes().to_vec();
        bytes.push(0);
        cursor -= bytes.len() as u64;
        unsafe { write_bytes_at(pml4, cursor, &bytes)? };
        envp_addrs.push(cursor);
    }

    // Doesn't need to be cryptographically random -- just present and
    // readable. See this function's doc comment for why AT_RANDOM being
    // *absent* would be worse than it being predictable.
    let random_bytes: [u8; 16] = [0xA5, 0x3C, 0x91, 0x7E, 0x0F, 0xD2, 0x44, 0x88, 0x19, 0x2B, 0x6C, 0x77, 0xEE, 0x55, 0x33, 0x01];
    let random_addr = (cursor - 16) & !0xF;
    unsafe { write_bytes_at(pml4, random_addr, &random_bytes)? };

    let auxv: [(u64, u64); 13] = [
        (AT_PHDR, image.phdr_vaddr),
        (AT_PHENT, image.phentsize),
        (AT_PHNUM, image.phnum),
        (AT_PAGESZ, PAGE_SIZE),
        (AT_BASE, at_base),
        (AT_FLAGS, 0),
        (AT_ENTRY, image.entry),
        (AT_UID, 0),
        (AT_EUID, 0),
        (AT_GID, 0),
        (AT_EGID, 0),
        (AT_SECURE, 0),
        (AT_RANDOM, random_addr),
    ];

    // argc(1) + argv(one pointer per real argument, + NULL) + envp(one pointer per real variable, + NULL) + (auxv entries + AT_NULL terminator), each auxv entry 2 words.
    let ptr_words = 1 + (argv_addrs.len() + 1) + (envp_addrs.len() + 1) + (auxv.len() + 1) * 2;
    let sp = (random_addr - ptr_words as u64 * 8) & !0xF;

    let mut cursor = sp;
    let mut push = |value: u64| -> Result<(), &'static str> {
        unsafe { write_u64_at(pml4, cursor, value)? };
        cursor += 8;
        Ok(())
    };
    push(argv_addrs.len() as u64)?; // argc
    for &addr in &argv_addrs {
        push(addr)?;
    }
    push(0)?; // argv terminator
    for &addr in &envp_addrs {
        push(addr)?;
    }
    push(0)?; // envp terminator
    for &(t, v) in &auxv {
        push(t)?;
        push(v)?;
    }
    push(AT_NULL)?;
    push(0)?;

    Ok(sp)
}

/// Real dynamic linking, via a real `PT_INTERP` dynamic linker -- what a
/// genuinely unmodified, dynamically-linked `musl-gcc` binary (built
/// *without* `-static`) needs, unlike everything `load_and_run`'s own
/// `DT_NEEDED`/`extern_relocations` scheme above has ever run. The two
/// approaches solve the same problem at opposite ends of "who does the
/// work": the `DT_NEEDED` scheme is this kernel acting as its own tiny,
/// from-scratch dynamic linker (parsing symbol tables, resolving
/// `R_X86_64_JUMP_SLOT`/`GLOB_DAT` by name, writing GOT entries) -- real
/// work, but narrow (SysV hash only, one dependency level, no `dlopen`).
/// This function does none of that: it hands a real interpreter (in
/// practice so far, musl's own `libc.so`, which -- uniquely among libc
/// implementations -- doubles as its own `ld.so`, so `DT_NEEDED: libc.so`
/// just means "the thing that's about to load you") two *raw, completely
/// unrelocated* mappings and exactly the auxv information real Linux
/// itself would provide, then jumps to the *interpreter's* entry point,
/// not the program's. Everything past that -- the interpreter's own
/// self-relocation (solved the same way every real `ld.so` solves its own
/// chicken-and-egg startup problem: a small position-independent bootstrap
/// stub that only touches `R_X86_64_RELATIVE`-style fixups, before it has
/// any working global state), finding and relocating the main program,
/// resolving symbols including real `GNU_HASH` (which this kernel's own
/// `DT_NEEDED` scheme deliberately doesn't support -- see `parse_elf_at`'s
/// doc comment), and eventually jumping to the program's *real* entry
/// point (found via `AT_ENTRY`) -- happens entirely in userspace, using
/// ordinary syscalls (`mmap`, `mprotect`, `arch_prctl`, ...) this kernel
/// already implements (see `linux_syscall.rs`). This is, deliberately, the
/// exact same division of labor real Linux's own kernel ELF loader
/// (`binfmt_elf.c`) uses: the kernel never resolves a single relocation
/// for either the executable or its interpreter, for either of them --
/// only `mmap`s raw bytes and hands off.
///
/// `AT_PHDR`/`AT_PHENT`/`AT_PHNUM`/`AT_ENTRY` describe the *main program*
/// (`image`), exactly as real Linux's own `execve` always reports them,
/// regardless of which file's code actually runs first -- that's how a
/// real `ld.so` finds the program it's about to link. `AT_BASE` is the
/// *interpreter's* own load bias, the one auxv entry that's about the
/// interpreter rather than the program -- what lets it find its own
/// `PT_DYNAMIC` segment to self-relocate before it can trust anything
/// else. The task's actual first instruction, unlike every other format
/// this loader runs, is the interpreter's entry point, not the program's.
fn load_and_run_with_interp(name: &'static str, argv: &[String], envp: &[String], image: Image, interp_path: &str) -> Result<(u64, Format), &'static str> {
    // fat16::read_file already accepts an absolute, `/`-separated path
    // (see its own doc comment) -- exactly the shape a real PT_INTERP
    // string always has (e.g. "/libc.so"), so no massaging needed here,
    // unlike the DT_NEEDED scheme's bare-soname lookups above.
    let interp_bytes = fat16::read_file(interp_path).map_err(|_| "loader: couldn't find this program's PT_INTERP dynamic linker on disk")?;
    if detect(&interp_bytes) != Format::Elf {
        return Err("loader: this program's PT_INTERP dynamic linker isn't an ELF file");
    }
    let interp_image = parse_elf_at(&interp_bytes, Some(INTERP_BASE))?;

    let pml4 = unsafe { paging::new_address_space() };
    for seg in &image.segments {
        unsafe { map_segment(pml4, seg) };
    }
    for seg in &interp_image.segments {
        unsafe { map_segment(pml4, seg) };
    }
    // No relocations applied here, deliberately -- see this function's own
    // doc comment. image.relocations/extern_relocations and
    // interp_image.relocations/extern_relocations are simply never
    // consulted on this path; the interpreter does that work itself, in
    // userspace, after it's handed control below.

    let stack_pages = USER_STACK_SIZE / PAGE_SIZE;
    for i in 0..stack_pages {
        let virt = USER_STACK_BASE + i * PAGE_SIZE;
        let phys = pmm::alloc_frame().ok_or("loader: out of memory mapping the stack")?;
        unsafe {
            paging::map_page_in(pml4, virt, phys, PAGE_WRITABLE | PAGE_USER);
        }
    }
    let stack_top = USER_STACK_BASE + USER_STACK_SIZE;
    let initial_sp = unsafe { build_initial_stack(pml4, stack_top, argv, envp, &image, INTERP_BASE)? };

    let id = task::spawn_user(name, argv[0].clone(), interp_image.entry, initial_sp, pml4).ok_or("loader: out of task slots")?;
    Ok((id, image.format))
}

pub fn load_and_run(name: &'static str, argv: &[String], envp: &[String], bytes: &[u8]) -> Result<(u64, Format), &'static str> {
    let image = match detect(bytes) {
        Format::Elf => parse_elf(bytes)?,
        Format::Pe => parse_pe(bytes)?,
        Format::Flat => parse_flat(bytes),
    };

    if let Some(interp_path) = image.interp.clone() {
        return load_and_run_with_interp(name, argv, envp, image, &interp_path);
    }

    if image.needed.len() > MAX_LIBS {
        return Err("loader: this program depends on more shared libraries than KonjacOS's loader supports at once");
    }

    // Load every DT_NEEDED dependency as its own ELF file, each parsed
    // exactly like the main executable (parse_elf_at is the same code
    // either way -- see its doc comment) but placed at its own fixed
    // LIB_BASE-derived slot instead of PIE_BASE, so the two never
    // collide even though both are ET_DYN. Only one level deep: a
    // library's own `needed` list (if it somehow has one) is never
    // followed, which is fine for now since nothing this loader can
    // build yet produces one.
    let mut libraries = Vec::new();
    for (i, lib_name) in image.needed.iter().enumerate() {
        // FAT16 is flat/8.3 (see fat16.rs) -- a soname like "libfoo.so" is
        // looked up directly at the filesystem root, case-insensitively,
        // same as every other file `run` loads.
        let lib_bytes = fat16::read_file(lib_name).map_err(|_| "loader: couldn't find a DT_NEEDED shared library on disk")?;
        if detect(&lib_bytes) != Format::Elf {
            return Err("loader: a DT_NEEDED dependency isn't an ELF shared library");
        }
        let lib_bias = LIB_BASE + i as u64 * LIB_SLOT_SIZE;
        let lib_image = parse_elf_at(&lib_bytes, Some(lib_bias))?;
        libraries.push(lib_image);
    }

    // Every loaded library's exported dynamic symbols, by name -- what
    // the main executable's (and, in principle, a sibling library's)
    // extern_relocations get resolved against below. A later library's
    // export silently wins over an earlier one's on a name collision,
    // same as a real dynamic linker's own symbol-search-order behavior.
    let mut exports: BTreeMap<String, u64> = BTreeMap::new();
    for lib in &libraries {
        for (name, addr) in &lib.exports {
            exports.insert(name.clone(), *addr);
        }
    }

    let pml4 = unsafe { paging::new_address_space() };
    for seg in &image.segments {
        unsafe { map_segment(pml4, seg) };
    }
    for lib in &libraries {
        for seg in &lib.segments {
            unsafe { map_segment(pml4, seg) };
        }
    }

    // A PIE's self-relocations (see parse_elf_at's ET_DYN handling) can
    // only be applied *after* every segment is mapped -- each one needs a
    // real physical frame to write into, found by translating the
    // (already-biased) target virtual address back through this task's
    // own address space. Writing through the HHDM here, not through the
    // user-facing mapping, is deliberate: it works regardless of whether
    // that segment is writable from ring 3 (a real loader fixes up
    // read-only relocations -- e.g. RELRO's .data.rel.ro -- before the
    // program ever gets a chance to see them as read-only). Applied for
    // every loaded file, main executable and libraries alike -- each one's
    // self-relocations only ever reference its own bias, so processing
    // order between files doesn't matter here, unlike extern_relocations
    // below.
    for img in core::iter::once(&image).chain(libraries.iter()) {
        for &(target_vaddr, value) in &img.relocations {
            unsafe { write_u64_at(pml4, target_vaddr, value)? };
        }
    }

    // Unlike load_and_run_with_interp (which never applies any relocations
    // at all -- the interpreter redoes all of them itself, IFUNCs
    // included), this eager path has no interpreter left to hand an
    // unresolved IFUNC off to: it's about to jump straight to the entry
    // point below with whatever's in memory. Rather than silently leave
    // the target address holding an unrelated resolver-function address
    // (real, but wrong -- a call through it would jump into the resolver,
    // not whatever the resolver would have picked), reject the file
    // outright, exactly like an unsupported DT_NEEDED depth or relocation
    // type already gets rejected elsewhere in this loader.
    for img in core::iter::once(&image).chain(libraries.iter()) {
        if !img.irelative_relocations.is_empty() {
            return Err("loader: this file needs GNU IFUNC resolver calls (R_X86_64_IRELATIVE), which KonjacOS's eager loader can't perform yet");
        }
    }

    // Real symbol resolution: GLOB_DAT/JUMP_SLOT/R_X86_64_64 entries from
    // every loaded file (the main executable's own .rela.dyn/.rela.plt,
    // and -- in principle, once a library can itself have DT_NEEDED
    // dependencies -- a library's) against the combined `exports` table
    // built above. This is the actual dynamic-linking step: everything
    // before it was mapping and self-contained fixups; this is what makes
    // a call from the main executable actually land inside a separately
    // loaded shared library.
    for img in core::iter::once(&image).chain(libraries.iter()) {
        for reloc in &img.extern_relocations {
            let sym_addr = exports.get(reloc.sym_name.as_str()).copied().ok_or("loader: undefined symbol -- a DT_NEEDED library doesn't export something this file needs")?;
            let value = if reloc.kind == R_X86_64_64 { sym_addr.wrapping_add(reloc.addend as u64) } else { sym_addr };
            unsafe { write_u64_at(pml4, reloc.target_vaddr, value)? };
        }
    }

    let stack_pages = USER_STACK_SIZE / PAGE_SIZE;
    for i in 0..stack_pages {
        let virt = USER_STACK_BASE + i * PAGE_SIZE;
        let phys = pmm::alloc_frame().ok_or("loader: out of memory mapping the stack")?;
        unsafe {
            paging::map_page_in(pml4, virt, phys, PAGE_WRITABLE | PAGE_USER);
        }
    }
    let stack_top = USER_STACK_BASE + USER_STACK_SIZE;
    let initial_sp = unsafe { build_initial_stack(pml4, stack_top, argv, envp, &image, 0)? };

    let id = task::spawn_user(name, argv[0].clone(), image.entry, initial_sp, pml4).ok_or("loader: out of task slots")?;
    Ok((id, image.format))
}
