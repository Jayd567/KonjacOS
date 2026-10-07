//! Values -> text for the screen: tables with aligned columns, records as
//! two columns, sizes as `4.2 MB`, dates as `2026-10-06 14:30:05`.

use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::fmt::Write;

use crate::time;
use crate::value::Value;

/// `4.2 MB`, `830 B`: powers of 1000, one decimal.
pub fn size(bytes: i64) -> String {
    let neg = bytes < 0;
    let b = bytes.unsigned_abs() as u128;
    let mut s = String::new();
    if neg {
        s.push('-');
    }
    if b < 1000 {
        let _ = write!(s, "{b} B");
        return s;
    }
    const UNITS: [&str; 6] = ["KB", "MB", "GB", "TB", "PB", "EB"];
    let mut div: u128 = 1000;
    let mut unit = 0;
    // Tenths of a unit, rounded; move up a unit if that rounds to 1000.0.
    loop {
        let tenths = (b * 10 + div / 2) / div;
        if tenths < 10_000 || unit + 1 == UNITS.len() {
            let _ = write!(s, "{}.{} {}", tenths / 10, tenths % 10, UNITS[unit]);
            return s;
        }
        div *= 1000;
        unit += 1;
    }
}

/// `3h 12min`, `2s 500ms`, `250ms`: the two largest units.
pub fn duration(ns: i64) -> String {
    if ns == 0 {
        return "0s".to_string();
    }
    let mut s = String::new();
    if ns < 0 {
        s.push('-');
    }
    let mut rest = ns.unsigned_abs();
    const UNITS: [(u64, &str); 7] = [
        (86_400_000_000_000, "day"),
        (3_600_000_000_000, "h"),
        (60_000_000_000, "min"),
        (1_000_000_000, "s"),
        (1_000_000, "ms"),
        (1_000, "us"),
        (1, "ns"),
    ];
    let mut shown = 0;
    for (n, name) in UNITS {
        if rest >= n {
            if shown > 0 {
                s.push(' ');
            }
            let _ = write!(s, "{}{name}", rest / n);
            rest %= n;
            shown += 1;
            if shown == 2 {
                break;
            }
        } else if shown > 0 {
            // `1h 0min` says nothing that `1h` doesn't.
            break;
        }
    }
    s
}

pub fn float(f: f64) -> String {
    let mut s = String::new();
    let _ = write!(s, "{f}");
    if f.is_finite() && !s.contains('.') && !s.contains('e') {
        s.push_str(".0");
    }
    s
}

/// A value that fits on one line, as text.
pub fn scalar(v: &Value) -> String {
    match v {
        Value::Nothing => String::new(),
        Value::Bool(b) => (if *b { "true" } else { "false" }).to_string(),
        Value::Int(i) => {
            let mut s = String::new();
            let _ = write!(s, "{i}");
            s
        }
        Value::Float(f) => float(*f),
        Value::String(s) => s.clone(),
        Value::Size(b) => size(*b),
        Value::Duration(d) => duration(*d),
        Value::Date(d) => time::format_date(*d),
        Value::List(l) => {
            let mut s = String::new();
            let _ = write!(s, "[{} item{}]", l.len(), if l.len() == 1 { "" } else { "s" });
            s
        }
        Value::Record(r) => {
            let mut s = String::new();
            let _ = write!(s, "{{{} field{}}}", r.cols.len(), if r.cols.len() == 1 { "" } else { "s" });
            s
        }
        Value::Closure(_) => "<closure>".to_string(),
        Value::Binary(b) => {
            let mut s = String::new();
            let _ = write!(s, "<{} bytes of binary data>", b.len());
            s
        }
    }
}

/// One table cell: like [`scalar`], but on one line.
fn cell(v: &Value) -> String {
    let s = scalar(v);
    if s.contains('\n') || s.contains('\r') || s.contains('\t') {
        return s.chars().map(|c| if c == '\n' || c == '\r' || c == '\t' { ' ' } else { c }).collect();
    }
    s
}

fn right_aligned(v: &Value) -> bool {
    matches!(v, Value::Int(_) | Value::Float(_) | Value::Size(_) | Value::Duration(_))
}

fn width_of(s: &str) -> usize {
    s.chars().count()
}

/// `s` cut to `w` characters, ending in `...` if it was cut.
fn fit(s: &str, w: usize) -> String {
    if width_of(s) <= w {
        return s.to_string();
    }
    if w <= 3 {
        return s.chars().take(w).collect();
    }
    let mut out: String = s.chars().take(w - 3).collect();
    out.push_str("...");
    out
}

fn pad(out: &mut String, s: &str, w: usize, right: bool) {
    let n = width_of(s);
    if right {
        for _ in n..w {
            out.push(' ');
        }
        out.push_str(s);
    } else {
        out.push_str(s);
        for _ in n..w {
            out.push(' ');
        }
    }
}

/// Renders a table: a header, a rule, then the rows, with an index column.
/// Columns that don't fit `width` are narrowed, then dropped from the
/// right (and named underneath).
fn table(headers: &[String], rows: &[Vec<(String, bool)>], width: usize) -> String {
    let n = headers.len();
    let mut widths: Vec<usize> = headers.iter().map(|h| width_of(h)).collect();
    let mut right = alloc::vec![false; n];
    for row in rows {
        for (i, (c, r)) in row.iter().enumerate() {
            widths[i] = widths[i].max(width_of(c));
            right[i] |= *r;
        }
    }
    let total = |ws: &[usize]| ws.iter().sum::<usize>() + 2 * ws.len().saturating_sub(1);
    let width = width.max(20);
    // Narrow the widest column, a character at a time, down to 10.
    while total(&widths) > width {
        let (i, &w) = match widths.iter().enumerate().skip(1).max_by_key(|(_, w)| **w) {
            Some(x) => x,
            None => break,
        };
        if w <= 10 {
            break;
        }
        widths[i] = w - 1;
    }
    let mut shown = n;
    while shown > 1 && total(&widths[..shown]) > width {
        shown -= 1;
    }
    let mut out = String::new();
    for i in 0..shown {
        if i > 0 {
            out.push_str("  ");
        }
        pad(&mut out, &fit(&headers[i], widths[i]), widths[i], right[i]);
    }
    trim_line(&mut out);
    out.push('\n');
    for i in 0..shown {
        if i > 0 {
            out.push_str("  ");
        }
        for _ in 0..widths[i] {
            out.push('-');
        }
    }
    for row in rows {
        out.push('\n');
        for i in 0..shown {
            if i > 0 {
                out.push_str("  ");
            }
            let (c, r) = row.get(i).map(|(c, _)| (c.as_str(), right[i])).unwrap_or(("", false));
            pad(&mut out, &fit(c, widths[i]), widths[i], r);
        }
        trim_line(&mut out);
    }
    if shown < n {
        out.push_str("\n(not shown: ");
        for (k, h) in headers[shown..].iter().enumerate() {
            if k > 0 {
                out.push_str(", ");
            }
            out.push_str(h);
        }
        out.push_str("; use `select` to pick columns)");
    }
    out
}

fn trim_line(out: &mut String) {
    while out.ends_with(' ') {
        out.pop();
    }
}

fn index_cell(i: usize) -> (String, bool) {
    let mut s = String::new();
    let _ = write!(s, "{i}");
    (s, true)
}

/// A whole value, for the screen, `width` columns wide.
pub fn render(v: &Value, width: usize) -> String {
    match v {
        Value::List(l) if l.is_empty() => "(empty list)".to_string(),
        Value::List(l) if l.iter().all(|x| matches!(x, Value::Record(_))) => {
            // A table: the columns of every row, in order of appearance.
            let mut cols: Vec<String> = Vec::new();
            for row in l {
                if let Value::Record(r) = row {
                    for (k, _) in &r.cols {
                        if !cols.iter().any(|c| c == k) {
                            cols.push(k.clone());
                        }
                    }
                }
            }
            let mut headers = alloc::vec!["#".to_string()];
            headers.extend(cols.iter().cloned());
            let rows: Vec<Vec<(String, bool)>> = l
                .iter()
                .enumerate()
                .map(|(i, row)| {
                    let mut cells = alloc::vec![index_cell(i)];
                    if let Value::Record(r) = row {
                        for c in &cols {
                            match r.get(c) {
                                Some(v) => cells.push((cell(v), right_aligned(v))),
                                None => cells.push((String::new(), false)),
                            }
                        }
                    }
                    cells
                })
                .collect();
            table(&headers, &rows, width)
        }
        Value::List(l) => {
            let headers = alloc::vec!["#".to_string(), "value".to_string()];
            let rows: Vec<Vec<(String, bool)>> = l.iter().enumerate().map(|(i, v)| alloc::vec![index_cell(i), (cell(v), right_aligned(v))]).collect();
            table(&headers, &rows, width)
        }
        Value::Record(r) if r.cols.is_empty() => "(empty record)".to_string(),
        Value::Record(r) => {
            let kw = r.cols.iter().map(|(k, _)| width_of(k)).max().unwrap_or(0).min(width / 2);
            let mut out = String::new();
            for (i, (k, v)) in r.cols.iter().enumerate() {
                if i > 0 {
                    out.push('\n');
                }
                pad(&mut out, &fit(k, kw), kw, false);
                out.push_str("  ");
                out.push_str(&fit(&cell(v), width.saturating_sub(kw + 2).max(10)));
                trim_line(&mut out);
            }
            out
        }
        Value::Binary(b) => {
            // A short hex dump.
            let mut out = scalar(v);
            for (row, chunk) in b.chunks(16).take(8).enumerate() {
                let _ = write!(out, "\n{:08x} ", row * 16);
                for byte in chunk {
                    let _ = write!(out, " {byte:02x}");
                }
            }
            if b.len() > 128 {
                out.push_str("\n...");
            }
            out
        }
        other => scalar(other),
    }
}
