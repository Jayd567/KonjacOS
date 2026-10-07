//! Positions in the source, and the error every failure becomes.

use alloc::string::String;

/// A range of bytes in one source: a line typed at the prompt, or a
/// script. `src` indexes the engine's list of sources, so an error inside
/// a function defined three lines ago still points at the right text.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Span {
    pub src: u32,
    pub start: u32,
    pub end: u32,
}

impl Span {
    pub fn new(src: u32, start: usize, end: usize) -> Span {
        Span { src, start: start as u32, end: end as u32 }
    }

    /// From the start of `self` to the end of `other`.
    pub fn to(self, other: Span) -> Span {
        if other.src != self.src {
            return self;
        }
        Span { src: self.src, start: self.start.min(other.start), end: self.end.max(other.end) }
    }
}

/// What went wrong, and where. `span` points at the part of the line to
/// blame; `hint` is shown under it.
#[derive(Clone, Debug)]
pub struct ShellError {
    pub msg: String,
    pub span: Option<Span>,
    pub hint: Option<String>,
}

impl ShellError {
    pub fn new(msg: impl Into<String>) -> ShellError {
        ShellError { msg: msg.into(), span: None, hint: None }
    }

    pub fn at(msg: impl Into<String>, span: Span) -> ShellError {
        ShellError { msg: msg.into(), span: Some(span), hint: None }
    }

    pub fn hint(mut self, hint: impl Into<String>) -> ShellError {
        self.hint = Some(hint.into());
        self
    }

    /// Points at `span` unless something more precise was already set.
    pub fn or_at(mut self, span: Span) -> ShellError {
        if self.span.is_none() {
            self.span = Some(span);
        }
        self
    }

    /// Puts `prefix: ` in front of the message, as in "each: open: ...".
    pub fn prefixed(mut self, prefix: &str) -> ShellError {
        let mut m = String::from(prefix);
        m.push_str(": ");
        m.push_str(&self.msg);
        self.msg = m;
        self
    }
}

impl From<String> for ShellError {
    fn from(s: String) -> ShellError {
        ShellError::new(s)
    }
}

impl From<&str> for ShellError {
    fn from(s: &str) -> ShellError {
        ShellError::new(s)
    }
}
