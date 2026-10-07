//! JSON <-> values, for `from json`, `to json` and `open x.json`.

use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::fmt::Write;

use crate::display;
use crate::error::ShellError;
use crate::time;
use crate::value::{Record, Value};

const MAX_DEPTH: usize = 64;

pub fn parse(text: &str) -> Result<Value, ShellError> {
    let mut p = Json { b: text.as_bytes(), s: text, i: 0 };
    p.ws();
    let v = p.value(0)?;
    p.ws();
    if p.i < p.b.len() {
        return Err(p.err("unexpected text after the JSON value"));
    }
    Ok(v)
}

struct Json<'a> {
    b: &'a [u8],
    s: &'a str,
    i: usize,
}

impl Json<'_> {
    fn err(&self, what: &str) -> ShellError {
        // Line and column, for long files.
        let before = &self.s[..self.i.min(self.s.len())];
        let line = before.matches('\n').count() + 1;
        let col = before.len() - before.rfind('\n').map(|n| n + 1).unwrap_or(0) + 1;
        let mut m = String::from("bad JSON: ");
        m.push_str(what);
        let _ = write!(m, " (line {line}, column {col})");
        ShellError::new(m)
    }

    fn ws(&mut self) {
        while self.i < self.b.len() && matches!(self.b[self.i], b' ' | b'\t' | b'\n' | b'\r') {
            self.i += 1;
        }
    }

    fn eat(&mut self, word: &str) -> bool {
        if self.b[self.i..].starts_with(word.as_bytes()) {
            self.i += word.len();
            return true;
        }
        false
    }

    fn value(&mut self, depth: usize) -> Result<Value, ShellError> {
        if depth > MAX_DEPTH {
            return Err(self.err("nested too deeply"));
        }
        let c = match self.b.get(self.i) {
            Some(c) => *c,
            None => return Err(self.err("it ends too soon")),
        };
        match c {
            b'{' => {
                self.i += 1;
                let mut r = Record::new();
                self.ws();
                if self.eat("}") {
                    return Ok(Value::Record(r));
                }
                loop {
                    self.ws();
                    if self.b.get(self.i) != Some(&b'"') {
                        return Err(self.err("expected a \"key\""));
                    }
                    let k = self.string()?;
                    self.ws();
                    if !self.eat(":") {
                        return Err(self.err("expected `:`"));
                    }
                    self.ws();
                    let v = self.value(depth + 1)?;
                    r.insert(&k, v);
                    self.ws();
                    if self.eat(",") {
                        continue;
                    }
                    if self.eat("}") {
                        return Ok(Value::Record(r));
                    }
                    return Err(self.err("expected `,` or `}`"));
                }
            }
            b'[' => {
                self.i += 1;
                let mut l = Vec::new();
                self.ws();
                if self.eat("]") {
                    return Ok(Value::List(l));
                }
                loop {
                    self.ws();
                    l.push(self.value(depth + 1)?);
                    self.ws();
                    if self.eat(",") {
                        continue;
                    }
                    if self.eat("]") {
                        return Ok(Value::List(l));
                    }
                    return Err(self.err("expected `,` or `]`"));
                }
            }
            b'"' => Ok(Value::String(self.string()?)),
            b't' if self.eat("true") => Ok(Value::Bool(true)),
            b'f' if self.eat("false") => Ok(Value::Bool(false)),
            b'n' if self.eat("null") => Ok(Value::Nothing),
            b'-' | b'0'..=b'9' => {
                let start = self.i;
                let mut float = false;
                while self.i < self.b.len() && matches!(self.b[self.i], b'-' | b'+' | b'.' | b'e' | b'E' | b'0'..=b'9') {
                    float |= matches!(self.b[self.i], b'.' | b'e' | b'E');
                    self.i += 1;
                }
                let text = &self.s[start..self.i];
                if !float {
                    if let Ok(n) = text.parse::<i64>() {
                        return Ok(Value::Int(n));
                    }
                }
                text.parse::<f64>().map(Value::Float).map_err(|_| self.err("bad number"))
            }
            _ => Err(self.err("unexpected character")),
        }
    }

    fn hex4(&mut self) -> Result<u32, ShellError> {
        let h = self.s.get(self.i..self.i + 4).ok_or_else(|| self.err("bad \\u escape"))?;
        let n = u32::from_str_radix(h, 16).map_err(|_| self.err("bad \\u escape"))?;
        self.i += 4;
        Ok(n)
    }

    fn string(&mut self) -> Result<String, ShellError> {
        self.i += 1; // `"`
        let mut out = String::new();
        loop {
            let start = self.i;
            while self.i < self.b.len() && self.b[self.i] != b'"' && self.b[self.i] != b'\\' {
                self.i += 1;
            }
            out.push_str(&self.s[start..self.i]);
            match self.b.get(self.i) {
                None => return Err(self.err("a string has no closing \"")),
                Some(b'"') => {
                    self.i += 1;
                    return Ok(out);
                }
                _ => {}
            }
            self.i += 1; // `\`
            let e = match self.b.get(self.i) {
                Some(e) => *e,
                None => return Err(self.err("it ends too soon")),
            };
            self.i += 1;
            match e {
                b'n' => out.push('\n'),
                b't' => out.push('\t'),
                b'r' => out.push('\r'),
                b'b' => out.push('\u{8}'),
                b'f' => out.push('\u{c}'),
                b'/' => out.push('/'),
                b'\\' => out.push('\\'),
                b'"' => out.push('"'),
                b'u' => {
                    let mut c = self.hex4()?;
                    if (0xd800..0xdc00).contains(&c) && self.eat("\\u") {
                        let low = self.hex4()?;
                        c = 0x10000 + ((c - 0xd800) << 10) + (low.wrapping_sub(0xdc00) & 0x3ff);
                    }
                    out.push(char::from_u32(c).unwrap_or('\u{fffd}'));
                }
                _ => return Err(self.err("bad escape")),
            }
        }
    }
}

fn quote(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// `v` as JSON; `indent` spaces per level, or all on one line if 0.
pub fn write(v: &Value, indent: usize) -> Result<String, ShellError> {
    let mut out = String::new();
    emit(&mut out, v, indent, 0)?;
    Ok(out)
}

fn newline(out: &mut String, indent: usize, level: usize) {
    if indent > 0 {
        out.push('\n');
        for _ in 0..indent * level {
            out.push(' ');
        }
    }
}

fn emit(out: &mut String, v: &Value, indent: usize, level: usize) -> Result<(), ShellError> {
    if level > MAX_DEPTH {
        return Err(ShellError::new("nested too deeply for JSON"));
    }
    match v {
        Value::Nothing => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Int(i) | Value::Size(i) | Value::Duration(i) => {
            let _ = write!(out, "{i}");
        }
        Value::Float(f) if f.is_finite() => out.push_str(&display::float(*f)),
        Value::Float(_) => out.push_str("null"),
        Value::String(s) => quote(out, s),
        Value::Date(d) => quote(out, &time::iso_date(*d)),
        Value::List(l) => {
            if l.is_empty() {
                out.push_str("[]");
                return Ok(());
            }
            out.push('[');
            for (i, x) in l.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                newline(out, indent, level + 1);
                emit(out, x, indent, level + 1)?;
            }
            newline(out, indent, level);
            out.push(']');
        }
        Value::Record(r) => {
            if r.cols.is_empty() {
                out.push_str("{}");
                return Ok(());
            }
            out.push('{');
            for (i, (k, x)) in r.cols.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                newline(out, indent, level + 1);
                quote(out, k);
                out.push(':');
                if indent > 0 {
                    out.push(' ');
                }
                emit(out, x, indent, level + 1)?;
            }
            newline(out, indent, level);
            out.push('}');
        }
        Value::Closure(_) => return Err(ShellError::new("a closure can't be turned into JSON")),
        Value::Binary(_) => return Err(ShellError::new("binary data can't be turned into JSON".to_string())),
    }
    Ok(())
}
