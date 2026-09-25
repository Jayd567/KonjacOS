//! The file-I/O half of the libc shim (see `libc_shim.rs` for the
//! memory-allocation half): `fopen`/`fclose`/`fread`/`fwrite`/`fseek`/
//! `ftell`/`feof`/`rewind`/`fputs`/`fgets`, plus `stdout`/`stderr`, for
//! freestanding C code (`csrc/`) compiled into the kernel.
//!
//! There's no real streaming I/O underneath this: `fat16.rs` only offers
//! "read this whole file into a `Vec`" and "write this whole `Vec` as this
//! file's entire contents", not partial reads/writes into an open file. So
//! every open file here is really just that whole-file `Vec<u8>` plus a
//! cursor (`CFile`), read from disk once at `fopen` and written back once
//! at `fclose` if anything changed -- everything in between (`fread`,
//! `fwrite`, `fseek`, ...) just operates on the in-memory copy. That's a
//! perfectly normal way to implement `FILE*` I/O on top of a filesystem
//! that doesn't offer partial I/O directly, and it's more than adequate
//! for what a WAD file (read once, in fairly large chunks, essentially
//! never written) actually needs.
//!
//! `FILE*` itself is kept fully opaque to C -- these functions hand out
//! and accept a bare `*mut u8`, matching how portable C code always treats
//! `FILE*` (an incomplete type it never looks inside), and cast it back to
//! `*mut CFile` internally. `stdout`/`stderr` are handled by the exact
//! same `CFile` type with a `console: true` flag rather than as a special
//! case scattered through every function: writes to them go straight to
//! `print!` instead of being buffered, and reads always report EOF (there's
//! no stdin hookup -- input comes through `keyboard.rs`/`mouse.rs`'s own
//! callback-style APIs, not a text stream).

extern crate alloc;

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;
use core::ffi::CStr;

use crate::fat16;

/// What C sees as `FILE*`: an opaque pointer. Never dereferenced as
/// anything but `*mut CFile` internally, and only by the functions in this
/// module.
type RawFile = *mut u8;

struct CFile {
    data: Vec<u8>,
    pos: usize,
    /// `None` for a console-backed pseudo-file (`stdout`/`stderr`) that
    /// was never actually loaded from or destined for the filesystem.
    path: Option<String>,
    dirty: bool,
    writable: bool,
    /// `fwrite` bypasses `data` entirely and prints straight to the
    /// console when this is set; `fread` always returns 0 (EOF).
    console: bool,
}

impl CFile {
    fn into_raw(self) -> RawFile {
        Box::into_raw(Box::new(self)) as RawFile
    }

    /// # Safety
    /// `raw` must be a live pointer this module itself handed out via
    /// `into_raw` (from `fopen` or the `stdout`/`stderr` statics) that
    /// hasn't already been passed to `fclose`.
    unsafe fn from_raw<'a>(raw: RawFile) -> &'a mut CFile {
        unsafe { &mut *(raw as *mut CFile) }
    }
}

// `stdout`/`stderr`: plain mutable globals, exactly what C's `extern FILE
// *stdout;` expects to link against -- not functions, not lazily
// initialized on first use, just a pointer value that's `null` until
// `init` runs (early in `kstart`, well before any C code that could
// reference them) and constant after. `#[allow(non_upper_case_globals)]`
// since the C-visible names are fixed by convention, not something this
// side gets to rename.
#[unsafe(no_mangle)]
#[allow(non_upper_case_globals)]
pub static mut stdout: RawFile = core::ptr::null_mut();
#[unsafe(no_mangle)]
#[allow(non_upper_case_globals)]
pub static mut stderr: RawFile = core::ptr::null_mut();

/// # Safety
/// Must run exactly once, after the heap is ready, before any C code that
/// might reference `stdout`/`stderr` runs.
pub unsafe fn init() {
    let out = CFile { data: Vec::new(), pos: 0, path: None, dirty: false, writable: true, console: true };
    let err = CFile { data: Vec::new(), pos: 0, path: None, dirty: false, writable: true, console: true };
    unsafe {
        stdout = out.into_raw();
        stderr = err.into_raw();
    }
}

/// Converts raw bytes (a C string's contents, with no guaranteed encoding
/// at all) into an owned `String`, substituting the Unicode replacement
/// character for anything non-ASCII rather than attempting real UTF-8
/// decoding. Deliberately *not* `alloc::string::String::from_utf8_lossy`:
/// that pulls in a codepath that needs `_Unwind_Resume`, which this
/// `panic = "abort"` kernel has no personality routine for and can't link
/// against -- the same underlying gotcha as `alloc::format!`, documented
/// in the top-level README's toolchain notes. C-side strings reaching this
/// shim (paths, printf output) are ASCII in every case that matters here,
/// so the simplification costs nothing in practice.
fn bytes_to_string_lossy(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len());
    for &b in bytes {
        s.push(if b.is_ascii() { b as char } else { '\u{FFFD}' });
    }
    s
}

/// Reads a NUL-terminated C string at `ptr` into an owned `String` via
/// [`bytes_to_string_lossy`].
///
/// # Safety
/// `ptr` must be non-null and point at a valid NUL-terminated string.
unsafe fn c_str_lossy(ptr: *const u8) -> String {
    let cstr = unsafe { CStr::from_ptr(ptr as *const core::ffi::c_char) };
    bytes_to_string_lossy(cstr.to_bytes())
}

/// # Safety
/// `path`/`mode` must be non-null, valid, NUL-terminated C strings.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fopen(path: *const u8, mode: *const u8) -> RawFile {
    if path.is_null() || mode.is_null() {
        return core::ptr::null_mut();
    }
    let path = unsafe { c_str_lossy(path) };
    let mode = unsafe { c_str_lossy(mode) };

    // Only the leading letter and a trailing/embedded '+' matter here --
    // "b" (binary) is accepted but meaningless, since there's no text/
    // binary distinction on this filesystem to begin with.
    let plus = mode.contains('+');
    let base = mode.chars().next().unwrap_or('\0');

    let (data, pos, writable) = match base {
        'r' => match fat16::read_file(&path) {
            Ok(data) => (data, 0, plus),
            Err(_) => return core::ptr::null_mut(),
        },
        'w' => (Vec::new(), 0, true),
        'a' => {
            // Append: start from whatever's already there (or empty, if
            // the file doesn't exist yet -- "a" creates it), positioned
            // at the end.
            let data = fat16::read_file(&path).unwrap_or_default();
            let pos = data.len();
            (data, pos, true)
        }
        _ => return core::ptr::null_mut(), // Unrecognized mode.
    };

    CFile { data, pos, path: Some(path), dirty: base != 'r', writable, console: false }.into_raw()
}

/// # Safety
/// `file` must be a live handle from `fopen` (or `stdout`/`stderr`) that
/// hasn't already been closed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fclose(file: RawFile) -> i32 {
    if file.is_null() {
        return -1;
    }
    // Safety: reconstructing the `Box` this handle's `into_raw` leaked --
    // exactly once, per the caller's obligation not to reuse `file` after
    // this call.
    let file = unsafe { Box::from_raw(file as *mut CFile) };
    if file.console {
        return 0; // Nothing to flush -- console writes already happened live.
    }
    if file.dirty && file.writable {
        if let Some(path) = &file.path {
            if fat16::write_file(path, &file.data).is_err() {
                return -1;
            }
        }
    }
    0
}

/// # Safety
/// `ptr` must point at a valid, writable buffer of at least `size *
/// nmemb` bytes; `file` must be a live handle from `fopen`/`stdout`/
/// `stderr`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fread(ptr: *mut u8, size: usize, nmemb: usize, file: RawFile) -> usize {
    if ptr.is_null() || file.is_null() || size == 0 || nmemb == 0 {
        return 0;
    }
    let f = unsafe { CFile::from_raw(file) };
    if f.console {
        return 0; // No stdin hookup -- always EOF.
    }

    let requested = size.saturating_mul(nmemb);
    let available = f.data.len().saturating_sub(f.pos);
    let to_copy = requested.min(available);
    let elems = to_copy / size;
    let byte_len = elems * size;

    if byte_len > 0 {
        // Safety: `to_copy`/`byte_len` bytes are available starting at
        // `f.pos` (just computed above), and `ptr` has room for at least
        // that many per the caller's obligation.
        unsafe {
            core::ptr::copy_nonoverlapping(f.data.as_ptr().add(f.pos), ptr, byte_len);
        }
        f.pos += byte_len;
    }
    elems
}

/// # Safety
/// `ptr` must point at a valid, readable buffer of at least `size *
/// nmemb` bytes; `file` must be a live handle from `fopen`/`stdout`/
/// `stderr`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fwrite(ptr: *const u8, size: usize, nmemb: usize, file: RawFile) -> usize {
    if ptr.is_null() || file.is_null() || size == 0 || nmemb == 0 {
        return 0;
    }
    let f = unsafe { CFile::from_raw(file) };
    if !f.writable {
        return 0;
    }

    let byte_len = size.saturating_mul(nmemb);
    // Safety: `ptr` has `byte_len` valid bytes per the caller's obligation.
    let bytes = unsafe { core::slice::from_raw_parts(ptr, byte_len) };

    if f.console {
        crate::print!("{}", bytes_to_string_lossy(bytes));
        return nmemb;
    }

    let end = f.pos + byte_len;
    if end > f.data.len() {
        f.data.resize(end, 0);
    }
    f.data[f.pos..end].copy_from_slice(bytes);
    f.pos = end;
    f.dirty = true;
    nmemb
}

const SEEK_SET: i32 = 0;
const SEEK_CUR: i32 = 1;
const SEEK_END: i32 = 2;

/// # Safety
/// `file` must be a live handle from `fopen`/`stdout`/`stderr`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fseek(file: RawFile, offset: i64, whence: i32) -> i32 {
    if file.is_null() {
        return -1;
    }
    let f = unsafe { CFile::from_raw(file) };
    let base: i64 = match whence {
        SEEK_SET => 0,
        SEEK_CUR => f.pos as i64,
        SEEK_END => f.data.len() as i64,
        _ => return -1,
    };
    let new_pos = base + offset;
    if new_pos < 0 {
        return -1;
    }
    f.pos = new_pos as usize;
    0
}

/// # Safety
/// `file` must be a live handle from `fopen`/`stdout`/`stderr`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ftell(file: RawFile) -> i64 {
    if file.is_null() {
        return -1;
    }
    unsafe { CFile::from_raw(file).pos as i64 }
}

/// # Safety
/// `file` must be a live handle from `fopen`/`stdout`/`stderr`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rewind(file: RawFile) {
    if file.is_null() {
        return;
    }
    unsafe { CFile::from_raw(file).pos = 0 };
}

/// # Safety
/// `file` must be a live handle from `fopen`/`stdout`/`stderr`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn feof(file: RawFile) -> i32 {
    if file.is_null() {
        return 1;
    }
    let f = unsafe { CFile::from_raw(file) };
    i32::from(!f.console && f.pos >= f.data.len())
}

/// # Safety
/// `s` must be a valid, NUL-terminated C string; `file` must be a live
/// handle from `fopen`/`stdout`/`stderr`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fputs(s: *const u8, file: RawFile) -> i32 {
    if s.is_null() || file.is_null() {
        return -1;
    }
    let cstr = unsafe { CStr::from_ptr(s as *const core::ffi::c_char) };
    let bytes = cstr.to_bytes();
    let written = unsafe { fwrite(bytes.as_ptr(), 1, bytes.len(), file) };
    if written == bytes.len() { 0 } else { -1 }
}

/// # Safety
/// `buf` must point at a writable buffer of at least `size` bytes; `file`
/// must be a live handle from `fopen`/`stdout`/`stderr`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fgets(buf: *mut u8, size: i32, file: RawFile) -> *mut u8 {
    if buf.is_null() || file.is_null() || size <= 0 {
        return core::ptr::null_mut();
    }
    let f = unsafe { CFile::from_raw(file) };
    if f.console || f.pos >= f.data.len() {
        return core::ptr::null_mut(); // EOF with nothing read.
    }

    let cap = (size as usize) - 1; // Room for the NUL terminator.
    let remaining = &f.data[f.pos..];
    let newline_at = remaining.iter().position(|&b| b == b'\n');
    let take = match newline_at {
        Some(i) => (i + 1).min(cap), // Include the newline itself, like real fgets.
        None => remaining.len().min(cap),
    };

    // Safety: `take <= cap == size - 1`, so writing `take` bytes plus a
    // NUL terminator fits within the caller-provided `size`-byte buffer.
    unsafe {
        core::ptr::copy_nonoverlapping(remaining.as_ptr(), buf, take);
        buf.add(take).write(0);
    }
    f.pos += take;
    buf
}

/// What `printf`/`puts`/`putchar` (`csrc/printf.c`) actually call to reach
/// the screen -- bytes in, straight to the console, lossily reinterpreted
/// as UTF-8 (C strings carry no encoding guarantee at all).
///
/// # Safety
/// `ptr` must point at `len` valid, readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn konjac_write(ptr: *const u8, len: usize) {
    if ptr.is_null() || len == 0 {
        return;
    }
    let bytes = unsafe { core::slice::from_raw_parts(ptr, len) };
    crate::print!("{}", bytes_to_string_lossy(bytes));
}
