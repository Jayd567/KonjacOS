//! ks, KonjacShell: a shell whose commands pass structured values to each
//! other instead of text. This crate is the language -- lexer, parser,
//! values, evaluator, display, and the commands that only need values --
//! with no kernel code in it. The kernel reaches it through [`Engine`] and
//! supplies files, the clock and the screen through [`Host`]. See
//! `docs/ks-design.md`.
//!
//! Nothing in here may panic on user input: the kernel runs this with
//! `panic = "abort"`, so a panic would stop the whole machine.

#![cfg_attr(not(test), no_std)]

extern crate alloc;

mod ast;
mod builtins;
pub mod display;
mod error;
mod eval;
mod json;
mod lexer;
mod parser;
mod sig;
mod time;
mod value;

#[cfg(test)]
mod tests;

pub use error::{ShellError, Span};
pub use eval::{Call, Command, Ctx, Engine, Runner};
pub use sig::{Shape, Signature};
pub use value::{Record, Value};
pub use time::from_parts as date_from_parts;

use alloc::string::String;
use alloc::vec::Vec;

/// One entry of a folder listing, or what `stat` says about a path.
#[derive(Clone, Debug)]
pub struct FileInfo {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    /// Nanoseconds since 1970, if the filesystem keeps it.
    pub modified: Option<i64>,
    pub created: Option<i64>,
}

/// What ks needs from the system it runs on. The kernel implements it
/// for real; the tests implement it with an in-memory disk.
pub trait Host {
    /// Writes text to the screen as it is (no newline added).
    fn print(&mut self, text: &str);
    fn cwd(&mut self) -> String;
    fn set_cwd(&mut self, path: &str) -> Result<(), String>;
    /// `path` made absolute, with `.` and `..` resolved.
    fn absolute(&mut self, path: &str) -> String;
    fn list_dir(&mut self, path: &str) -> Result<Vec<FileInfo>, String>;
    fn stat(&mut self, path: &str) -> Result<FileInfo, String>;
    fn read(&mut self, path: &str) -> Result<Vec<u8>, String>;
    fn write(&mut self, path: &str, data: &[u8]) -> Result<(), String>;
    /// Deletes a file, or a folder with everything in it.
    fn remove(&mut self, path: &str) -> Result<(), String>;
    fn rename(&mut self, from: &str, to: &str) -> Result<(), String>;
    /// Copies a file, or a folder with everything in it.
    fn copy(&mut self, from: &str, to: &str) -> Result<(), String>;
    fn mkdir(&mut self, path: &str) -> Result<(), String>;
    /// Now, in nanoseconds since 1970.
    fn now(&mut self) -> i64;
    /// Asks a yes/no question and waits for the answer.
    fn confirm(&mut self, question: &str) -> bool;
    /// Whether Ctrl+C was pressed since the last call.
    fn interrupted(&mut self) -> bool;
    /// Asks for the apex password before `what` runs.
    fn apex(&mut self, what: &str) -> bool;
    /// How many columns the screen has.
    fn width(&mut self) -> usize;
}
