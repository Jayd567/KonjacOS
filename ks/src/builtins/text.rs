//! Text and conversions: lines, split, str ..., from/to json, into ...

use alloc::string::String;
use alloc::vec::Vec;

use crate::error::ShellError;
use crate::eval::{Call, Ctx, Engine, Runner};
use crate::json;
use crate::sig::{Shape, Signature};
use crate::value::{parse_literal, Value};

pub fn register(e: &mut Engine) {
    let t = "text";
    e.register(Signature::build("lines", t, "split text into a list of lines"), Runner::Native(lines));
    e.register(Signature::build("split", t, "split text into a list at a separator").required("separator", Shape::String, "where to split"), Runner::Native(split));
    e.register(
        Signature::build("str contains", t, "whether the text contains something").required("text", Shape::String, "what to look for").switch("ignore-case", Some('i'), "ignore upper and lower case"),
        Runner::Native(str_contains),
    );
    e.register(Signature::build("str starts-with", t, "whether the text starts with something").required("text", Shape::String, "the start"), Runner::Native(str_starts));
    e.register(Signature::build("str ends-with", t, "whether the text ends with something").required("text", Shape::String, "the end"), Runner::Native(str_ends));
    e.register(Signature::build("str upcase", t, "the text in upper case"), Runner::Native(str_upcase));
    e.register(Signature::build("str downcase", t, "the text in lower case"), Runner::Native(str_downcase));
    e.register(Signature::build("str trim", t, "the text without spaces at either end"), Runner::Native(str_trim));
    e.register(Signature::build("str length", t, "how many characters the text has"), Runner::Native(str_length));
    e.register(
        Signature::build("str replace", t, "replace every copy of some text with other text").required("find", Shape::String, "what to replace").required("with", Shape::String, "what to put instead"),
        Runner::Native(str_replace),
    );
    e.register(Signature::build("str join", t, "join a list into one text").optional("separator", Shape::String, "what to put between items"), Runner::Native(str_join));
    e.register(Signature::build("from json", t, "parse JSON text into values"), Runner::Native(from_json));
    e.register(Signature::build("to json", t, "turn values into JSON text").switch("raw", Some('r'), "all on one line"), Runner::Native(to_json));
    let c = "conversions";
    e.register(Signature::build("into int", c, "convert to an int"), Runner::Native(into_int));
    e.register(Signature::build("into float", c, "convert to a float"), Runner::Native(into_float));
    e.register(Signature::build("into size", c, "convert to a size (an int is a number of bytes)"), Runner::Native(into_size));
    e.register(Signature::build("into string", c, "convert to text"), Runner::Native(into_string));
}

/// Applies `f` to a string, or to every string in a list.
fn map_str(input: Value, f: &dyn Fn(&str) -> Value) -> Result<Value, ShellError> {
    match input {
        Value::String(s) => Ok(f(&s)),
        Value::List(l) => {
            let mut out = Vec::with_capacity(l.len());
            for (i, v) in l.iter().enumerate() {
                match v {
                    Value::String(s) => out.push(f(s)),
                    other => return Err(super::on_item(ShellError::new(alloc::format!("expected a string, found {}", other.a_type())), i, l.len())),
                }
            }
            Ok(Value::List(out))
        }
        other => Err(ShellError::new(alloc::format!("needs a string, not {}", other.a_type()))),
    }
}

fn text_input(input: Value) -> Result<String, ShellError> {
    match input {
        Value::String(s) => Ok(s),
        Value::Binary(b) => String::from_utf8(b).map_err(|_| ShellError::new("the input isn't text (UTF-8)")),
        other => Err(ShellError::new(alloc::format!("needs a string, not {}", other.a_type()))),
    }
}

fn lines(_: &mut Ctx, _: &Call, input: Value) -> Result<Value, ShellError> {
    let s = text_input(input)?;
    Ok(Value::List(s.lines().map(Value::str).collect()))
}

fn split(_: &mut Ctx, call: &Call, input: Value) -> Result<Value, ShellError> {
    let sep = call.str_at(0).unwrap_or("");
    if sep.is_empty() {
        return Err(ShellError::at("the separator can't be empty", call.pos_span(0)));
    }
    let sep = String::from(sep);
    map_str(input, &|s| Value::List(s.split(sep.as_str()).map(Value::str).collect()))
}

fn lower(s: &str) -> String {
    s.chars().flat_map(|c| c.to_lowercase()).collect()
}

fn str_contains(_: &mut Ctx, call: &Call, input: Value) -> Result<Value, ShellError> {
    let needle = String::from(call.str_at(0).unwrap_or(""));
    if call.has("ignore-case") {
        let n = lower(&needle);
        map_str(input, &|s| Value::Bool(lower(s).contains(n.as_str())))
    } else {
        map_str(input, &|s| Value::Bool(s.contains(needle.as_str())))
    }
}

fn str_starts(_: &mut Ctx, call: &Call, input: Value) -> Result<Value, ShellError> {
    let p = String::from(call.str_at(0).unwrap_or(""));
    map_str(input, &|s| Value::Bool(s.starts_with(p.as_str())))
}

fn str_ends(_: &mut Ctx, call: &Call, input: Value) -> Result<Value, ShellError> {
    let p = String::from(call.str_at(0).unwrap_or(""));
    map_str(input, &|s| Value::Bool(s.ends_with(p.as_str())))
}

fn str_upcase(_: &mut Ctx, _: &Call, input: Value) -> Result<Value, ShellError> {
    map_str(input, &|s| Value::String(s.chars().flat_map(|c| c.to_uppercase()).collect()))
}

fn str_downcase(_: &mut Ctx, _: &Call, input: Value) -> Result<Value, ShellError> {
    map_str(input, &|s| Value::String(lower(s)))
}

fn str_trim(_: &mut Ctx, _: &Call, input: Value) -> Result<Value, ShellError> {
    map_str(input, &|s| Value::str(s.trim()))
}

fn str_length(_: &mut Ctx, _: &Call, input: Value) -> Result<Value, ShellError> {
    map_str(input, &|s| Value::Int(s.chars().count() as i64))
}

fn str_replace(_: &mut Ctx, call: &Call, input: Value) -> Result<Value, ShellError> {
    let find = String::from(call.str_at(0).unwrap_or(""));
    let with = String::from(call.str_at(1).unwrap_or(""));
    if find.is_empty() {
        return Err(ShellError::at("can't replace empty text", call.pos_span(0)));
    }
    map_str(input, &|s| Value::String(s.replace(find.as_str(), &with)))
}

fn str_join(_: &mut Ctx, call: &Call, input: Value) -> Result<Value, ShellError> {
    let sep = call.str_at(0).unwrap_or("");
    let l = super::items(input, "str join")?;
    let mut out = String::new();
    for (i, v) in l.iter().enumerate() {
        if i > 0 {
            out.push_str(sep);
        }
        out.push_str(&v.to_text());
    }
    Ok(Value::String(out))
}

fn from_json(_: &mut Ctx, _: &Call, input: Value) -> Result<Value, ShellError> {
    json::parse(&text_input(input)?)
}

fn to_json(_: &mut Ctx, call: &Call, input: Value) -> Result<Value, ShellError> {
    Ok(Value::String(json::write(&input, if call.has("raw") { 0 } else { 2 })?))
}

fn cant(v: &Value, to: &str) -> ShellError {
    let mut m = String::from("can't turn ");
    m.push_str(v.a_type());
    m.push_str(" into ");
    m.push_str(to);
    ShellError::new(m)
}

/// Applies a conversion to a value, or to every item of a list.
fn map_values(input: Value, f: &dyn Fn(&Value) -> Result<Value, ShellError>) -> Result<Value, ShellError> {
    match input {
        Value::List(l) => {
            let n = l.len();
            let mut out = Vec::with_capacity(n);
            for (i, v) in l.iter().enumerate() {
                out.push(f(v).map_err(|e| super::on_item(e, i, n))?);
            }
            Ok(Value::List(out))
        }
        v => f(&v),
    }
}

fn into_int(_: &mut Ctx, _: &Call, input: Value) -> Result<Value, ShellError> {
    map_values(input, &|v| {
        Ok(Value::Int(match v {
            Value::Int(i) | Value::Size(i) | Value::Duration(i) | Value::Date(i) => *i,
            Value::Float(f) if f.is_finite() && f.abs() < 9.2e18 => *f as i64,
            Value::Bool(b) => *b as i64,
            Value::String(s) => match parse_literal(s.trim()) {
                Some(Value::Int(i)) => i,
                Some(Value::Float(f)) if f.abs() < 9.2e18 => f as i64,
                _ => return Err(ShellError::new(alloc::format!("`{s}` isn't a number"))),
            },
            other => return Err(cant(other, "an int")),
        }))
    })
}

fn into_float(_: &mut Ctx, _: &Call, input: Value) -> Result<Value, ShellError> {
    map_values(input, &|v| {
        Ok(Value::Float(match v {
            Value::Int(i) => *i as f64,
            Value::Float(f) => *f,
            Value::String(s) => match parse_literal(s.trim()) {
                Some(Value::Int(i)) => i as f64,
                Some(Value::Float(f)) => f,
                _ => return Err(ShellError::new(alloc::format!("`{s}` isn't a number"))),
            },
            other => return Err(cant(other, "a float")),
        }))
    })
}

fn into_size(_: &mut Ctx, _: &Call, input: Value) -> Result<Value, ShellError> {
    map_values(input, &|v| {
        Ok(Value::Size(match v {
            Value::Int(i) | Value::Size(i) => *i,
            Value::String(s) => match parse_literal(s.trim()) {
                Some(Value::Size(b)) | Some(Value::Int(b)) => b,
                _ => return Err(ShellError::new(alloc::format!("`{s}` isn't a size")).hint("sizes look like 512B, 4KiB or 1.5MB")),
            },
            other => return Err(cant(other, "a size")),
        }))
    })
}

fn into_string(_: &mut Ctx, _: &Call, input: Value) -> Result<Value, ShellError> {
    map_values(input, &|v| Ok(Value::String(v.to_text())))
}
