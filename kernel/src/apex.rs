//! apex (Admin Permission EXecute): a `sudo`-style gate for commands the
//! shell's command table flags `requires_apex`. The first time any such
//! command runs, there's no password on disk yet, so this walks the user
//! through choosing one; every time after that it prompts for the
//! existing password. A successful check is cached for a few minutes
//! (like `sudo`'s own timestamp cache) so a burst of admin commands
//! doesn't re-prompt for each one.
//!
//! The password itself is never stored -- only its SHA-256 hash
//! ([`sha256`]), as a 64-character hex string in `/APEX.PWD` on the FAT16
//! disk. That file lives at the true root regardless of the shell's
//! current directory (`authenticate` always addresses it as `/APEX.PWD`,
//! an absolute path), so `cd`-ing around never changes which file apex
//! reads or writes.

extern crate alloc;

use alloc::string::String;
use core::sync::atomic::{AtomicU64, Ordering};

use crate::sha256::sha256_hex;
use crate::timer;
use crate::{print, println};

const PASSWORD_PATH: &str = "/APEX.PWD";
const MAX_PASSWORD_LEN: usize = 64;
const MAX_ATTEMPTS: u32 = 3;
/// How long a successful check stays valid before the next apex command
/// prompts again. Chosen to match `sudo`'s traditional default.
const CACHE_SECONDS: u64 = 5 * 60;

/// Ticks (see `timer.rs`) until which the cached authentication is still
/// valid. `0` means "never authenticated" -- also what a fresh boot starts
/// at, so every session starts locked out, same as `sudo` not trusting a
/// previous login across reboots.
static AUTHENTICATED_UNTIL: AtomicU64 = AtomicU64::new(0);

fn is_cached() -> bool {
    let until = AUTHENTICATED_UNTIL.load(Ordering::Relaxed);
    until != 0 && timer::ticks() < until
}

fn cache_success() {
    let until = timer::ticks() + CACHE_SECONDS * timer::HZ as u64;
    AUTHENTICATED_UNTIL.store(until, Ordering::Relaxed);
}

/// Whether an apex command could run right now without prompting.
/// Doesn't itself prompt -- see [`authenticate`] for that.
pub fn is_authenticated() -> bool {
    is_cached()
}

/// Reads a line from the keyboard without echoing typed characters back
/// (an `*` is printed per character instead, for feedback that *something*
/// was typed, without showing what). Blocks -- same shape as the shell's
/// own input loop, just without command dispatch or cursor blinking.
fn read_password_line() -> String {
    let mut buf = String::new();
    loop {
        match crate::keyboard::read_char() {
            Some(b'\n') => {
                println!();
                return buf;
            }
            Some(0x08) => {
                if buf.pop().is_some() {
                    print!("\u{8}");
                }
            }
            Some(byte) if byte.is_ascii_graphic() || byte == b' ' => {
                if buf.len() < MAX_PASSWORD_LEN {
                    buf.push(byte as char);
                    print!("*");
                }
            }
            Some(_) => {} // Unmapped control byte; ignore.
            None => unsafe {
                core::arch::asm!("hlt");
            },
        }
    }
}

fn prompt_password(prompt: &str) -> String {
    print!("{prompt}");
    read_password_line()
}

/// `Ok(Some(hash))` if a password's been set, `Ok(None)` if this is a
/// fresh disk with none yet, `Err` for anything else (corrupt file,
/// unmounted filesystem, ...).
fn load_stored_hash() -> Result<Option<String>, &'static str> {
    match crate::fat16::read_file(PASSWORD_PATH) {
        Ok(data) => {
            let text = core::str::from_utf8(&data).map_err(|_| "apex: password file is corrupt (not valid text)")?;
            Ok(Some(String::from(text.trim())))
        }
        Err("no such file or directory") => Ok(None),
        Err(e) => Err(e),
    }
}

fn save_hash(hash_hex: &str) -> Result<(), &'static str> {
    crate::fat16::write_file(PASSWORD_PATH, hash_hex.as_bytes())
}

/// First-time setup: choose and confirm a new password, hash it, and
/// save it. Returns whether setup succeeded (and, on success, leaves the
/// caller authenticated -- no reason to make someone who just proved they
/// know the password they picked seconds ago type it again immediately).
fn setup_password(command_name: &str) -> bool {
    println!("[apex] no password set yet -- this is a one-time setup, needed to run '{command_name}'.");
    let first = prompt_password("New apex password: ");
    let second = prompt_password("Confirm password: ");

    if first.is_empty() {
        println!("apex: empty password not allowed, nothing saved.");
        return false;
    }
    if first != second {
        println!("apex: passwords didn't match, nothing saved.");
        return false;
    }

    match save_hash(&sha256_hex(first.as_bytes())) {
        Ok(()) => {
            println!("apex password set.");
            cache_success();
            true
        }
        Err(e) => {
            println!("apex: failed to save password: {e}");
            false
        }
    }
}

/// Prompts for the existing password, up to [`MAX_ATTEMPTS`] times.
fn check_password(command_name: &str, stored_hash: &str) -> bool {
    for _ in 0..MAX_ATTEMPTS {
        print!("[apex] password for '{command_name}': ");
        let entered = read_password_line();
        if sha256_hex(entered.as_bytes()) == stored_hash {
            cache_success();
            return true;
        }
        println!("Sorry, try again.");
    }
    println!("apex: too many failed attempts.");
    false
}

/// The actual gate: returns whether `command_name` (an apex-flagged
/// command) should be allowed to run. Prompts interactively when needed
/// (first-time setup, or the existing password, or nothing at all if a
/// recent check is still cached) -- callers just get a yes/no.
pub fn authenticate(command_name: &str) -> bool {
    if is_cached() {
        return true;
    }

    match load_stored_hash() {
        Ok(Some(hash)) => check_password(command_name, &hash),
        Ok(None) => setup_password(command_name),
        Err(e) => {
            println!("apex: error reading password file: {e}");
            false
        }
    }
}
