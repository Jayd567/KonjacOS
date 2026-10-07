//! The commands that only need values and the [`Host`](crate::Host):
//! everything except what the kernel adds (processes, memory, disks, its
//! original text commands).

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write;

use crate::error::ShellError;
use crate::eval::Engine;
use crate::value::Value;

mod core_cmds;
mod data;
mod files;
mod text;

pub fn register(e: &mut Engine) {
    core_cmds::register(e);
    data::register(e);
    text::register(e);
    files::register(e);
}

/// The input as a list of items: a list as it is, nothing as no items,
/// anything else is an error.
pub fn items(input: Value, what: &str) -> Result<Vec<Value>, ShellError> {
    match input {
        Value::List(l) => Ok(l),
        Value::Nothing => Ok(Vec::new()),
        other => {
            let mut m = String::from(what);
            m.push_str(" needs a list or a table as input, not ");
            m.push_str(other.a_type());
            Err(ShellError::new(m))
        }
    }
}

/// Adds "(item 3 of 7)" to an error that happened on one item.
pub fn on_item(mut e: ShellError, i: usize, n: usize) -> ShellError {
    let _ = write!(e.msg, " (item {} of {n})", i + 1);
    e
}

/// "3 files", "1 file".
pub fn plural(n: usize, one: &str, many: &str) -> String {
    let mut s = String::new();
    let _ = write!(s, "{n} {}", if n == 1 { one } else { many });
    s
}
