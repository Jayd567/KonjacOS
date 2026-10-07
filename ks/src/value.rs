//! Values: what commands take and return, and the rules for arithmetic,
//! comparison and reaching inside them.

use alloc::rc::Rc;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::cmp::Ordering;

use crate::ast::{ClosureAst, Op};
use crate::display;
use crate::error::ShellError;
use crate::time;

#[derive(Clone, Debug)]
pub enum Value {
    Nothing,
    Bool(bool),
    Int(i64),
    Float(f64),
    String(String),
    /// Bytes.
    Size(i64),
    /// Nanoseconds.
    Duration(i64),
    /// Nanoseconds since 1970.
    Date(i64),
    List(Vec<Value>),
    Record(Record),
    Closure(Rc<Closure>),
    Binary(Vec<u8>),
}

/// Named fields, in the order they were added.
#[derive(Clone, Debug, Default)]
pub struct Record {
    pub cols: Vec<(String, Value)>,
}

impl Record {
    pub fn new() -> Record {
        Record { cols: Vec::new() }
    }

    pub fn get(&self, name: &str) -> Option<&Value> {
        self.cols.iter().find(|(k, _)| k == name).map(|(_, v)| v)
    }

    /// Sets `name`, replacing it if it's already there.
    pub fn insert(&mut self, name: &str, v: Value) {
        match self.cols.iter_mut().find(|(k, _)| k == name) {
            Some(slot) => slot.1 = v,
            None => self.cols.push((name.to_string(), v)),
        }
    }

    pub fn push(mut self, name: &str, v: Value) -> Record {
        self.insert(name, v);
        self
    }

    pub fn remove(&mut self, name: &str) -> Option<Value> {
        let i = self.cols.iter().position(|(k, _)| k == name)?;
        Some(self.cols.remove(i).1)
    }

    /// "name, size, modified", for error hints.
    pub fn column_list(&self) -> String {
        let mut s = String::new();
        for (i, (k, _)) in self.cols.iter().enumerate() {
            if i > 0 {
                s.push_str(", ");
            }
            s.push_str(k);
        }
        s
    }
}

/// A block with parameters, and the variables it uses from where it was
/// written (copied when it was made, so it can run anywhere later).
#[derive(Debug)]
pub struct Closure {
    pub ast: Rc<ClosureAst>,
    pub captured: Vec<(String, Value)>,
}

impl Value {
    pub fn str(s: &str) -> Value {
        Value::String(s.to_string())
    }

    pub fn type_name(&self) -> &'static str {
        match self {
            Value::Nothing => "nothing",
            Value::Bool(_) => "bool",
            Value::Int(_) => "int",
            Value::Float(_) => "float",
            Value::String(_) => "string",
            Value::Size(_) => "size",
            Value::Duration(_) => "duration",
            Value::Date(_) => "date",
            Value::List(l) if !l.is_empty() && l.iter().all(|v| matches!(v, Value::Record(_))) => "table",
            Value::List(_) => "list",
            Value::Record(_) => "record",
            Value::Closure(_) => "closure",
            Value::Binary(_) => "binary",
        }
    }

    /// `a size`, `an int`: for messages.
    pub fn a_type(&self) -> &'static str {
        match self {
            Value::Nothing => "nothing",
            Value::Bool(_) => "a bool",
            Value::Int(_) => "an int",
            Value::Float(_) => "a float",
            Value::String(_) => "a string",
            Value::Size(_) => "a size",
            Value::Duration(_) => "a duration",
            Value::Date(_) => "a date",
            Value::List(l) if !l.is_empty() && l.iter().all(|v| matches!(v, Value::Record(_))) => "a table",
            Value::List(_) => "a list",
            Value::Record(_) => "a record",
            Value::Closure(_) => "a closure",
            Value::Binary(_) => "binary data",
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::String(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_int(&self) -> Option<i64> {
        match self {
            Value::Int(i) => Some(*i),
            _ => None,
        }
    }

    /// The value as text: strings as they are, everything else as the
    /// display would show it on one line.
    pub fn to_text(&self) -> String {
        match self {
            Value::String(s) => s.clone(),
            Value::List(l) => {
                let mut s = String::from("[");
                for (i, v) in l.iter().enumerate() {
                    if i > 0 {
                        s.push_str(", ");
                    }
                    s.push_str(&v.to_text());
                }
                s.push(']');
                s
            }
            Value::Record(r) => {
                let mut s = String::from("{");
                for (i, (k, v)) in r.cols.iter().enumerate() {
                    if i > 0 {
                        s.push_str(", ");
                    }
                    s.push_str(k);
                    s.push_str(": ");
                    s.push_str(&v.to_text());
                }
                s.push('}');
                s
            }
            other => display::scalar(other),
        }
    }

    /// `.name` or `.0` after a value: a record's field, a list's item, or
    /// a table's column.
    pub fn member(&self, name: &str) -> Result<Value, ShellError> {
        match self {
            Value::Record(r) => match r.get(name) {
                Some(v) => Ok(v.clone()),
                None => Err(no_column(name, r)),
            },
            Value::List(l) => {
                if let Ok(i) = name.parse::<usize>() {
                    return match l.get(i) {
                        Some(v) => Ok(v.clone()),
                        None => Err(ShellError::new(alloc::format!("there's no item {i}: the list has {}", l.len()))),
                    };
                }
                // A table's column, one value per row.
                let mut out = Vec::with_capacity(l.len());
                for (i, row) in l.iter().enumerate() {
                    match row {
                        Value::Record(r) => match r.get(name) {
                            Some(v) => out.push(v.clone()),
                            None => return Err(no_column(name, r).prefixed(&alloc::format!("row {i}"))),
                        },
                        other => return Err(ShellError::new(alloc::format!("can't get .{name}: item {i} is {}, not a record", other.a_type()))),
                    }
                }
                Ok(Value::List(out))
            }
            other => Err(ShellError::new(alloc::format!("can't get .{name} of {}", other.a_type()))),
        }
    }

    /// Whether this is `true`; anything that isn't a bool is an error.
    pub fn truthy(&self, what: &str) -> Result<bool, ShellError> {
        match self {
            Value::Bool(b) => Ok(*b),
            other => Err(ShellError::new(alloc::format!("{what} gave {}, not true or false", other.a_type()))),
        }
    }
}

pub fn no_column(name: &str, r: &Record) -> ShellError {
    let mut e = ShellError::new(alloc::format!("there's no column `{name}`"));
    if !r.cols.is_empty() {
        let mut h = String::from("the columns are: ");
        h.push_str(&r.column_list());
        e = e.hint(h);
    }
    e
}

fn type_rank(v: &Value) -> u8 {
    match v {
        Value::Nothing => 0,
        Value::Bool(_) => 1,
        Value::Int(_) | Value::Float(_) => 2,
        Value::String(_) => 3,
        Value::Size(_) => 4,
        Value::Duration(_) => 5,
        Value::Date(_) => 6,
        Value::List(_) => 7,
        Value::Record(_) => 8,
        Value::Binary(_) => 9,
        Value::Closure(_) => 10,
    }
}

fn cmp_f64(a: f64, b: f64) -> Ordering {
    a.partial_cmp(&b).unwrap_or_else(|| a.is_nan().cmp(&b.is_nan()))
}

/// A total order over all values, for sorting: by type first, then by
/// value. Strings compare ignoring case first, so `apple` sorts next to
/// `Apple` and before `banana`.
pub fn sort_cmp(a: &Value, b: &Value) -> Ordering {
    match (a, b) {
        (Value::Bool(x), Value::Bool(y)) => x.cmp(y),
        (Value::Int(x), Value::Int(y)) => x.cmp(y),
        (Value::Int(x), Value::Float(y)) => cmp_f64(*x as f64, *y),
        (Value::Float(x), Value::Int(y)) => cmp_f64(*x, *y as f64),
        (Value::Float(x), Value::Float(y)) => cmp_f64(*x, *y),
        (Value::String(x), Value::String(y)) => {
            let fold = x.chars().map(|c| c.to_ascii_lowercase()).cmp(y.chars().map(|c| c.to_ascii_lowercase()));
            fold.then_with(|| x.cmp(y))
        }
        (Value::Size(x), Value::Size(y)) | (Value::Duration(x), Value::Duration(y)) | (Value::Date(x), Value::Date(y)) => x.cmp(y),
        (Value::List(x), Value::List(y)) => {
            for (p, q) in x.iter().zip(y.iter()) {
                let o = sort_cmp(p, q);
                if o != Ordering::Equal {
                    return o;
                }
            }
            x.len().cmp(&y.len())
        }
        (Value::Record(x), Value::Record(y)) => {
            for ((ka, va), (kb, vb)) in x.cols.iter().zip(y.cols.iter()) {
                let o = ka.cmp(kb).then_with(|| sort_cmp(va, vb));
                if o != Ordering::Equal {
                    return o;
                }
            }
            x.cols.len().cmp(&y.cols.len())
        }
        (Value::Binary(x), Value::Binary(y)) => x.cmp(y),
        _ => type_rank(a).cmp(&type_rank(b)),
    }
}

pub fn equals(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Nothing, Value::Nothing) => true,
        (Value::Int(_) | Value::Float(_), Value::Int(_) | Value::Float(_)) => sort_cmp(a, b) == Ordering::Equal,
        (Value::String(x), Value::String(y)) => x == y,
        (Value::List(x), Value::List(y)) => x.len() == y.len() && x.iter().zip(y.iter()).all(|(p, q)| equals(p, q)),
        (Value::Record(x), Value::Record(y)) => {
            x.cols.len() == y.cols.len() && x.cols.iter().zip(y.cols.iter()).all(|((ka, va), (kb, vb))| ka == kb && equals(va, vb))
        }
        (Value::Closure(x), Value::Closure(y)) => Rc::ptr_eq(x, y),
        _ => type_rank(a) == type_rank(b) && sort_cmp(a, b) == Ordering::Equal,
    }
}

fn is_number(v: &Value) -> bool {
    matches!(v, Value::Int(_) | Value::Float(_))
}

fn as_f64(v: &Value) -> f64 {
    match v {
        Value::Int(i) => *i as f64,
        Value::Float(f) => *f,
        _ => 0.0,
    }
}

/// The hint for comparing or adding a unit-less number to a size or a
/// duration: "5 has no unit; did you mean 5MB?".
fn unit_hint(number: &Value, other: &Value) -> Option<String> {
    let unit = match other {
        Value::Size(_) => "MB",
        Value::Duration(_) => "s",
        _ => return None,
    };
    let n = display::scalar(number);
    let mut h = n.clone();
    h.push_str(" has no unit; did you mean ");
    h.push_str(&n);
    h.push_str(unit);
    h.push('?');
    Some(h)
}

fn mismatch(op: Op, a: &Value, b: &Value) -> ShellError {
    let verb = match op {
        Op::Add => "add",
        Op::Sub => "subtract",
        Op::Mul => "multiply",
        Op::Div | Op::Mod => "divide",
        Op::Concat => "join",
        Op::And | Op::Or => "combine",
        _ => "compare",
    };
    let mut msg = String::from("can't ");
    msg.push_str(verb);
    msg.push(' ');
    msg.push_str(a.a_type());
    msg.push_str(if matches!(op, Op::And | Op::Or | Op::Concat) { " and " } else { " with " });
    msg.push_str(b.a_type());
    let mut e = ShellError::new(msg);
    let hint = if is_number(a) { unit_hint(a, b) } else if is_number(b) { unit_hint(b, a) } else { None };
    if let Some(h) = hint {
        e = e.hint(h);
    }
    e
}

fn overflow() -> ShellError {
    ShellError::new("the result is too big")
}

fn round_f64(x: f64) -> Result<i64, ShellError> {
    if !x.is_finite() || x > 9.2e18 || x < -9.2e18 {
        return Err(overflow());
    }
    Ok(if x >= 0.0 { (x + 0.5) as i64 } else { (x - 0.5) as i64 })
}

/// `a op b`, for every operator except `and` and `or` (which the
/// evaluator short-circuits).
pub fn binary(op: Op, a: &Value, b: &Value) -> Result<Value, ShellError> {
    use Value::*;
    let res = match op {
        Op::Eq | Op::Ne => {
            if (is_number(a) && matches!(b, Size(_) | Duration(_))) || (is_number(b) && matches!(a, Size(_) | Duration(_))) {
                return Err(mismatch(op, a, b));
            }
            let e = equals(a, b);
            return Ok(Bool(if op == Op::Eq { e } else { !e }));
        }
        Op::Lt | Op::Le | Op::Gt | Op::Ge => {
            let ord = match (a, b) {
                _ if is_number(a) && is_number(b) => sort_cmp(a, b),
                (String(x), String(y)) => x.as_str().cmp(y.as_str()),
                (Size(x), Size(y)) | (Duration(x), Duration(y)) | (Date(x), Date(y)) => x.cmp(y),
                (Bool(x), Bool(y)) => x.cmp(y),
                _ => return Err(mismatch(op, a, b)),
            };
            return Ok(Bool(match op {
                Op::Lt => ord == Ordering::Less,
                Op::Le => ord != Ordering::Greater,
                Op::Gt => ord == Ordering::Greater,
                _ => ord != Ordering::Less,
            }));
        }
        Op::Glob => match (a, b) {
            (String(text), String(pat)) => Some(Bool(glob_match(pat, text))),
            _ => None,
        },
        Op::In => match b {
            List(l) => Some(Bool(l.iter().any(|v| equals(v, a)))),
            String(s) => match a {
                String(sub) => Some(Bool(s.contains(sub.as_str()))),
                _ => None,
            },
            Record(r) => match a {
                String(k) => Some(Bool(r.get(k).is_some())),
                _ => None,
            },
            _ => None,
        },
        Op::Concat => match (a, b) {
            (List(x), List(y)) => {
                let mut l = x.clone();
                l.extend(y.iter().cloned());
                Some(List(l))
            }
            (List(x), y) => {
                let mut l = x.clone();
                l.push(y.clone());
                Some(List(l))
            }
            (String(x), String(y)) => {
                let mut s = x.clone();
                s.push_str(y);
                Some(String(s))
            }
            _ => None,
        },
        Op::Add => match (a, b) {
            (Int(x), Int(y)) => Some(Int(x.checked_add(*y).ok_or_else(overflow)?)),
            _ if is_number(a) && is_number(b) => Some(Float(as_f64(a) + as_f64(b))),
            (Size(x), Size(y)) => Some(Size(x.checked_add(*y).ok_or_else(overflow)?)),
            (Duration(x), Duration(y)) => Some(Duration(x.checked_add(*y).ok_or_else(overflow)?)),
            (Date(x), Duration(y)) | (Duration(y), Date(x)) => Some(Date(x.checked_add(*y).ok_or_else(overflow)?)),
            (String(x), String(y)) => {
                let mut s = x.clone();
                s.push_str(y);
                Some(String(s))
            }
            _ => None,
        },
        Op::Sub => match (a, b) {
            (Int(x), Int(y)) => Some(Int(x.checked_sub(*y).ok_or_else(overflow)?)),
            _ if is_number(a) && is_number(b) => Some(Float(as_f64(a) - as_f64(b))),
            (Size(x), Size(y)) => Some(Size(x.checked_sub(*y).ok_or_else(overflow)?)),
            (Duration(x), Duration(y)) => Some(Duration(x.checked_sub(*y).ok_or_else(overflow)?)),
            (Date(x), Duration(y)) => Some(Date(x.checked_sub(*y).ok_or_else(overflow)?)),
            (Date(x), Date(y)) => Some(Duration(x.checked_sub(*y).ok_or_else(overflow)?)),
            _ => None,
        },
        Op::Mul => match (a, b) {
            (Int(x), Int(y)) => Some(Int(x.checked_mul(*y).ok_or_else(overflow)?)),
            _ if is_number(a) && is_number(b) => Some(Float(as_f64(a) * as_f64(b))),
            (Size(x), Int(n)) | (Int(n), Size(x)) => Some(Size(x.checked_mul(*n).ok_or_else(overflow)?)),
            (Duration(x), Int(n)) | (Int(n), Duration(x)) => Some(Duration(x.checked_mul(*n).ok_or_else(overflow)?)),
            (Size(x), Float(f)) | (Float(f), Size(x)) => Some(Size(round_f64(*x as f64 * f)?)),
            (Duration(x), Float(f)) | (Float(f), Duration(x)) => Some(Duration(round_f64(*x as f64 * f)?)),
            _ => None,
        },
        Op::Div => {
            let zero = matches!(b, Int(0) | Size(0) | Duration(0)) || matches!(b, Float(f) if *f == 0.0);
            if zero && (is_number(b) || matches!(b, Size(_) | Duration(_))) && matches!(a, Int(_) | Float(_) | Size(_) | Duration(_)) {
                return Err(ShellError::new("division by zero"));
            }
            match (a, b) {
                (Int(x), Int(y)) if x.checked_rem(*y) == Some(0) => Some(Int(x.checked_div(*y).ok_or_else(overflow)?)),
                _ if is_number(a) && is_number(b) => Some(Float(as_f64(a) / as_f64(b))),
                (Size(x), Int(n)) | (Duration(x), Int(n)) => {
                    let v = x.checked_div(*n).ok_or_else(overflow)?;
                    Some(if matches!(a, Size(_)) { Size(v) } else { Duration(v) })
                }
                (Size(x), Float(f)) => Some(Size(round_f64(*x as f64 / f)?)),
                (Duration(x), Float(f)) => Some(Duration(round_f64(*x as f64 / f)?)),
                (Size(x), Size(y)) | (Duration(x), Duration(y)) => Some(Float(*x as f64 / *y as f64)),
                _ => None,
            }
        }
        Op::Mod => match (a, b) {
            (Int(_), Int(0)) => return Err(ShellError::new("division by zero")),
            (Int(x), Int(y)) => Some(Int(x.checked_rem_euclid(*y).ok_or_else(overflow)?)),
            _ => None,
        },
        Op::And | Op::Or => match (a, b) {
            (Bool(x), Bool(y)) => Some(Bool(if op == Op::And { *x && *y } else { *x || *y })),
            _ => None,
        },
    };
    res.ok_or_else(|| mismatch(op, a, b))
}

/// Whether `text` matches `pattern`, where `*` is any run of characters
/// and `?` is any one. Ignores case: `*.wad` matches `DOOM1.WAD`.
pub fn glob_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().map(|c| c.to_ascii_lowercase()).collect();
    let t: Vec<char> = text.chars().map(|c| c.to_ascii_lowercase()).collect();
    let (mut pi, mut ti) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some((pi, ti));
            pi += 1;
        } else if let Some((sp, st)) = star {
            pi = sp + 1;
            ti = st + 1;
            star = Some((sp, st + 1));
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

/// The value a bare word stands for when it looks like one: a number,
/// size, duration, date, `true`, `false` or `null`. `None` means it's
/// just text.
pub fn parse_literal(word: &str) -> Option<Value> {
    match word {
        "true" => return Some(Value::Bool(true)),
        "false" => return Some(Value::Bool(false)),
        "null" => return Some(Value::Nothing),
        _ => {}
    }
    let first = *word.as_bytes().first()?;
    if !(first.is_ascii_digit() || ((first == b'-' || first == b'+') && word.len() > 1)) {
        return None;
    }
    if let Some(d) = time::parse_date(word) {
        return Some(Value::Date(d));
    }
    let (neg, body) = match first {
        b'-' => (true, &word[1..]),
        b'+' => (false, &word[1..]),
        _ => (false, word),
    };
    if let Some(hex) = body.strip_prefix("0x").or_else(|| body.strip_prefix("0X")) {
        let n = i64::from_str_radix(&hex.replace('_', ""), 16).ok()?;
        return Some(Value::Int(if neg { n.checked_neg()? } else { n }));
    }
    // The number, then the unit.
    let b = body.as_bytes();
    let mut i = 0;
    let mut digits = 0;
    let mut fractional = false;
    while i < b.len() && (b[i].is_ascii_digit() || b[i] == b'_') {
        digits += b[i].is_ascii_digit() as usize;
        i += 1;
    }
    if i < b.len() && b[i] == b'.' && b.get(i + 1).is_some_and(|c| c.is_ascii_digit()) {
        fractional = true;
        i += 1;
        while i < b.len() && (b[i].is_ascii_digit() || b[i] == b'_') {
            digits += 1;
            i += 1;
        }
    }
    if digits == 0 {
        return None;
    }
    if i < b.len() && (b[i] == b'e' || b[i] == b'E') {
        let mut j = i + 1;
        if j < b.len() && (b[j] == b'-' || b[j] == b'+') {
            j += 1;
        }
        if j < b.len() && b[j].is_ascii_digit() {
            while j < b.len() && b[j].is_ascii_digit() {
                j += 1;
            }
            fractional = true;
            i = j;
        }
    }
    let number = body[..i].replace('_', "");
    let unit = &body[i..];
    let sign = if neg { -1 } else { 1 };
    if unit.is_empty() {
        return if fractional {
            let f: f64 = number.parse().ok()?;
            Some(Value::Float(if neg { -f } else { f }))
        } else {
            let n: i64 = number.parse().ok()?;
            Some(Value::Int(n * sign))
        };
    }
    let mut lower = String::with_capacity(unit.len());
    for c in unit.chars() {
        lower.push(c.to_ascii_lowercase());
    }
    let (mult, is_size): (i64, bool) = match lower.as_str() {
        "b" => (1, true),
        "kb" => (1_000, true),
        "mb" => (1_000_000, true),
        "gb" => (1_000_000_000, true),
        "tb" => (1_000_000_000_000, true),
        "kib" => (1 << 10, true),
        "mib" => (1 << 20, true),
        "gib" => (1 << 30, true),
        "tib" => (1 << 40, true),
        "ns" => (1, false),
        "us" => (1_000, false),
        "ms" => (1_000_000, false),
        "s" | "sec" => (time::NS_PER_SEC, false),
        "min" => (60 * time::NS_PER_SEC, false),
        "h" | "hr" => (3_600 * time::NS_PER_SEC, false),
        "d" | "day" | "days" => (86_400 * time::NS_PER_SEC, false),
        "wk" => (604_800 * time::NS_PER_SEC, false),
        _ => return None,
    };
    let amount = if fractional {
        let f: f64 = number.parse().ok()?;
        round_f64(f * mult as f64 * sign as f64).ok()?
    } else {
        let n: i64 = number.parse().ok()?;
        n.checked_mul(mult)?.checked_mul(sign)?
    };
    Some(if is_size { Value::Size(amount) } else { Value::Duration(amount) })
}
