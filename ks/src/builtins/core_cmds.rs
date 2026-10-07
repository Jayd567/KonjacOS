//! echo, print, help, describe, do, source, date now.

use alloc::string::{String, ToString};
use alloc::vec::Vec;

use crate::display;
use crate::error::ShellError;
use crate::eval::{Call, Ctx, Engine, Runner};
use crate::sig::{Shape, Signature};
use crate::value::{Record, Value};

pub fn register(e: &mut Engine) {
    e.register(
        Signature::build("echo", "core", "return its arguments (several are joined with spaces)").rest("value", Shape::Any, "what to return"),
        Runner::Native(echo),
    );
    e.register(
        Signature::build("print", "core", "show values on the screen now, instead of at the end").rest("value", Shape::Any, "what to show").switch("no-newline", Some('n'), "don't end the line"),
        Runner::Native(print),
    );
    e.register(
        Signature::build("help", "core", "list every command, or explain one").rest("command", Shape::String, "the command to explain"),
        Runner::Native(help),
    );
    e.register(Signature::build("describe", "core", "the type of the input"), Runner::Native(describe));
    e.register(
        Signature::build("do", "core", "run a closure").required("closure", Shape::Closure, "what to run").rest("args", Shape::Any, "its parameters"),
        Runner::Native(do_),
    );
    e.register(
        Signature::build("source", "core", "run a .ks script in this shell").required("file", Shape::Path, "the script"),
        Runner::Native(source),
    );
    e.register(Signature::build("date now", "core", "the current date and time"), Runner::Native(date_now));
}

fn echo(_: &mut Ctx, call: &Call, _: Value) -> Result<Value, ShellError> {
    Ok(match call.positional.len() {
        0 => Value::Nothing,
        1 => call.positional[0].0.clone(),
        _ => {
            let mut s = String::new();
            for (i, (v, _)) in call.positional.iter().enumerate() {
                if i > 0 {
                    s.push(' ');
                }
                s.push_str(&v.to_text());
            }
            Value::String(s)
        }
    })
}

fn print(ctx: &mut Ctx, call: &Call, input: Value) -> Result<Value, ShellError> {
    let w = ctx.host.width();
    let mut s = String::new();
    if call.positional.is_empty() {
        s = display::render(&input, w);
    }
    for (i, (v, _)) in call.positional.iter().enumerate() {
        if i > 0 {
            s.push(' ');
        }
        s.push_str(&display::render(v, w));
    }
    if !call.has("no-newline") {
        s.push('\n');
    }
    ctx.host.print(&s);
    Ok(Value::Nothing)
}

fn help(ctx: &mut Ctx, call: &Call, _: Value) -> Result<Value, ShellError> {
    if !call.positional.is_empty() {
        let mut name = String::new();
        for (i, (v, _)) in call.positional.iter().enumerate() {
            if i > 0 {
                name.push(' ');
            }
            name.push_str(&v.to_text());
        }
        return match ctx.engine.signature(&name) {
            Some(sig) => Ok(Value::String(sig.help())),
            None => {
                let mut m = String::from("there's no command `");
                m.push_str(&name);
                m.push('`');
                Err(ShellError::at(m, call.pos_span(0)))
            }
        };
    }
    let mut rows: Vec<Value> = ctx
        .engine
        .signatures()
        .map(|s| Value::Record(Record::new().push("name", Value::str(&s.name)).push("group", Value::str(s.category)).push("summary", Value::str(&s.summary))))
        .collect();
    // By group, then name, so related commands sit together.
    rows.sort_by(|a, b| {
        let key = |v: &Value| match v {
            Value::Record(r) => (r.get("group").map(|x| x.to_text()).unwrap_or_default(), r.get("name").map(|x| x.to_text()).unwrap_or_default()),
            _ => (String::new(), String::new()),
        };
        key(a).cmp(&key(b))
    });
    Ok(Value::List(rows))
}

fn describe(_: &mut Ctx, _: &Call, input: Value) -> Result<Value, ShellError> {
    Ok(Value::String(input.type_name().to_string()))
}

fn do_(ctx: &mut Ctx, call: &Call, input: Value) -> Result<Value, ShellError> {
    match call.pos(0) {
        Some(Value::Closure(c)) => {
            let c = c.clone();
            let args = call.positional.iter().skip(1).map(|(v, _)| v.clone()).collect();
            ctx.call_closure(&c, args, input)
        }
        _ => Err(call.err("needs a closure")),
    }
}

fn source(ctx: &mut Ctx, call: &Call, _: Value) -> Result<Value, ShellError> {
    let path = call.str_at(0).unwrap_or("");
    let data = ctx.host.read(path).map_err(|e| ShellError::at(e, call.pos_span(0)))?;
    let text = core::str::from_utf8(&data).map_err(|_| ShellError::at("the script isn't text (UTF-8)", call.pos_span(0)))?;
    ctx.run_script(text)
}

fn date_now(ctx: &mut Ctx, _: &Call, _: Value) -> Result<Value, ShellError> {
    Ok(Value::Date(ctx.host.now()))
}
