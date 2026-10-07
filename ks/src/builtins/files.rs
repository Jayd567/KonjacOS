//! Files: ls, cd, pwd, open, save, mkdir, delete, move, copy, stat.

use alloc::string::String;
use alloc::vec::Vec;

use super::plural;
use crate::display;
use crate::error::ShellError;
use crate::eval::{Call, Ctx, Engine, Runner};
use crate::json;
use crate::sig::{Shape, Signature};
use crate::value::{glob_match, Record, Value};
use crate::FileInfo;

pub fn register(e: &mut Engine) {
    let f = "files";
    e.register(
        Signature::build("ls", f, "list a folder as a table: name, type, size, modified")
            .optional("path", Shape::Path, "the folder, or a pattern like *.txt or docs/*.md")
            .switch("long", Some('l'), "add when each was created"),
        Runner::Native(ls),
    );
    e.register(Signature::build("cd", f, "change the current folder (`cd` alone goes to /)").optional("path", Shape::Path, "where to go"), Runner::Native(cd));
    e.register(Signature::build("pwd", f, "the current folder"), Runner::Native(pwd));
    e.register(
        Signature::build("open", f, "read a file: text as a string, .json as values, anything else as binary").required("path", Shape::Path, "the file").switch("raw", Some('r'), "don't parse .json"),
        Runner::Native(open),
    );
    e.register(Signature::build("cat", f, "a file's text").required("path", Shape::Path, "the file"), Runner::Native(cat));
    e.register(
        Signature::build("save", f, "write the input to a file (text, binary, or .json from any values)")
            .required("path", Shape::Path, "the file")
            .switch("force", Some('f'), "overwrite the file if it's there")
            .switch("append", Some('a'), "add to the end of the file")
            .destructive(),
        Runner::Native(save),
    );
    e.register(Signature::build("mkdir", f, "make folders").rest("path", Shape::Path, "the folders to make"), Runner::Native(mkdir));
    e.register(
        Signature::build("delete", f, "delete files and folders, named or piped in (a table with a name column)")
            .rest("path", Shape::Path, "what to delete")
            .switch("yes", Some('y'), "don't ask first")
            .switch("dry-run", Some('n'), "show what would be deleted, and stop")
            .destructive()
            .apex(),
        Runner::Native(delete),
    );
    e.register(
        Signature::build("move", f, "move or rename: move a b, move a b folder/, or ls *.txt | move folder/").rest("path", Shape::Path, "what to move, then where").destructive(),
        Runner::Native(move_),
    );
    e.register(
        Signature::build("copy", f, "copy: copy a b, copy a b folder/, or ls *.txt | copy folder/").rest("path", Shape::Path, "what to copy, then where").destructive(),
        Runner::Native(copy),
    );
    e.register(Signature::build("stat", f, "what's known about a file or folder").required("path", Shape::Path, "the file or folder"), Runner::Native(stat));
    e.alias("rm", "delete");
    e.alias("mv", "move");
    e.alias("cp", "copy");
}

fn host_err(e: String, call: &Call, i: usize) -> ShellError {
    ShellError::at(e, call.pos_span(i))
}

fn join(dir: &str, name: &str) -> String {
    let mut s = String::from(dir);
    if !s.is_empty() && !s.ends_with('/') {
        s.push('/');
    }
    s.push_str(name);
    s
}

fn basename(path: &str) -> &str {
    let p = path.trim_end_matches('/');
    p.rsplit('/').next().unwrap_or(p)
}

fn row(name: String, info: &FileInfo, long: bool) -> Value {
    let date = |d: Option<i64>| d.map(Value::Date).unwrap_or(Value::Nothing);
    let mut r = Record::new()
        .push("name", Value::String(name))
        .push("type", Value::str(if info.is_dir { "dir" } else { "file" }))
        .push("size", Value::Size(info.size as i64))
        .push("modified", date(info.modified));
    if long {
        r.insert("created", date(info.created));
    }
    Value::Record(r)
}

fn ls(ctx: &mut Ctx, call: &Call, _: Value) -> Result<Value, ShellError> {
    let arg = call.str_at(0).unwrap_or("");
    let long = call.has("long");
    // `*.txt` or `docs/*.md`: list the folder, keep what matches.
    let (dir, pattern) = match arg.rsplit_once('/') {
        Some((d, p)) if p.contains('*') || p.contains('?') => (if d.is_empty() { "/" } else { d }, Some(p)),
        _ if arg.contains('*') || arg.contains('?') => ("", Some(arg)),
        _ => (arg, None),
    };
    let listed = if dir.is_empty() { "." } else { dir };
    let mut entries = ctx.host.list_dir(listed).map_err(|e| host_err(e, call, 0))?;
    if let Some(p) = pattern {
        entries.retain(|e| glob_match(p, &e.name));
    }
    entries.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then_with(|| a.name.to_ascii_lowercase().cmp(&b.name.to_ascii_lowercase())));
    // Names are paths from here, so `ls docs | delete` finds them.
    let prefix = if pattern.is_some() { dir } else { arg };
    let prefix = if matches!(prefix, "." | "./") { "" } else { prefix };
    Ok(Value::List(entries.iter().map(|e| row(join(prefix, &e.name), e, long)).collect()))
}

fn cd(ctx: &mut Ctx, call: &Call, _: Value) -> Result<Value, ShellError> {
    let path = call.str_at(0).unwrap_or("/");
    ctx.host.set_cwd(path).map_err(|e| host_err(e, call, 0))?;
    Ok(Value::Nothing)
}

fn pwd(ctx: &mut Ctx, _: &Call, _: Value) -> Result<Value, ShellError> {
    Ok(Value::String(ctx.host.cwd()))
}

fn open(ctx: &mut Ctx, call: &Call, _: Value) -> Result<Value, ShellError> {
    let path = call.str_at(0).unwrap_or("");
    let data = ctx.host.read(path).map_err(|e| host_err(e, call, 0))?;
    let json = path.to_ascii_lowercase().ends_with(".json") && !call.has("raw");
    match String::from_utf8(data) {
        Ok(text) if json => json::parse(&text).map_err(|e| e.or_at(call.pos_span(0))),
        Ok(text) => Ok(Value::String(text)),
        Err(e) => Ok(Value::Binary(e.into_bytes())),
    }
}

fn cat(ctx: &mut Ctx, call: &Call, _: Value) -> Result<Value, ShellError> {
    let path = call.str_at(0).unwrap_or("");
    let data = ctx.host.read(path).map_err(|e| host_err(e, call, 0))?;
    match String::from_utf8(data) {
        Ok(text) => Ok(Value::String(text)),
        Err(e) => Ok(Value::Binary(e.into_bytes())),
    }
}

fn save(ctx: &mut Ctx, call: &Call, input: Value) -> Result<Value, ShellError> {
    let path = call.str_at(0).unwrap_or("");
    let mut bytes = match input {
        Value::String(s) => s.into_bytes(),
        Value::Binary(b) => b,
        Value::Nothing => return Err(call.err("there's nothing to save: pipe something into it")),
        v if path.to_ascii_lowercase().ends_with(".json") => json::write(&v, 2)?.into_bytes(),
        v => {
            let mut m = String::from("can't save ");
            m.push_str(v.a_type());
            m.push_str(" as it is");
            return Err(call.err(m).hint("turn it into text first, e.g. `to json`, or save to a .json file"));
        }
    };
    let exists = ctx.host.stat(path).is_ok();
    if call.has("append") && exists {
        let mut old = ctx.host.read(path).map_err(|e| host_err(e, call, 0))?;
        old.append(&mut bytes);
        bytes = old;
    } else if exists && !call.has("force") {
        return Err(ShellError::at("there's already a file there", call.pos_span(0)).hint("add --force to overwrite it, or --append to add to it"));
    }
    ctx.host.write(path, &bytes).map_err(|e| host_err(e, call, 0))?;
    Ok(Value::Nothing)
}

fn mkdir(ctx: &mut Ctx, call: &Call, _: Value) -> Result<Value, ShellError> {
    for (i, (v, _)) in call.positional.iter().enumerate() {
        ctx.host.mkdir(&v.to_text()).map_err(|e| host_err(e, call, i))?;
    }
    Ok(Value::Nothing)
}

/// The paths a command was given: piped in (a table with a `name`
/// column, a list of paths, a record, a path) or, if nothing was piped,
/// named as arguments.
fn paths_from(input: &Value, call: &Call, args: &[(Value, crate::Span)]) -> Result<(Vec<String>, bool), ShellError> {
    fn one(v: &Value) -> Result<String, ShellError> {
        match v {
            Value::String(s) => Ok(s.clone()),
            Value::Record(r) => match r.get("name") {
                Some(Value::String(s)) => Ok(s.clone()),
                _ => Err(ShellError::new("a record piped in needs a `name` column with the path")),
            },
            other => Err(ShellError::new(alloc::format!("expected a path, found {}", other.a_type()))),
        }
    }
    match input {
        Value::Nothing => Ok((args.iter().map(|(v, _)| v.to_text()).collect(), false)),
        _ if !args.is_empty() && !matches!(call.name.as_str(), "move" | "mv" | "copy" | "cp") => {
            Err(call.err("give the paths as arguments or pipe them in, not both"))
        }
        Value::List(l) => {
            let mut out = Vec::with_capacity(l.len());
            for (i, v) in l.iter().enumerate() {
                out.push(one(v).map_err(|e| super::on_item(e, i, l.len()))?);
            }
            Ok((out, true))
        }
        v => Ok((alloc::vec![one(v)?], true)),
    }
}

fn delete(ctx: &mut Ctx, call: &Call, input: Value) -> Result<Value, ShellError> {
    let (paths, piped) = paths_from(&input, call, &call.positional)?;
    if paths.is_empty() {
        return Ok(Value::Nothing);
    }
    // Look everything up before touching anything: one missing file
    // stops the whole command, with nothing deleted.
    let mut infos = Vec::with_capacity(paths.len());
    for (i, p) in paths.iter().enumerate() {
        let info = ctx.host.stat(p).map_err(|e| {
            let mut m = String::from(p);
            m.push_str(": ");
            m.push_str(&e);
            let e = ShellError::new(m).hint("nothing was deleted");
            if piped { e } else { e.or_at(call.pos_span(i)) }
        })?;
        infos.push(info);
    }
    if call.has("dry-run") {
        return Ok(Value::List(paths.iter().zip(infos.iter()).map(|(p, info)| row(p.clone(), info, false)).collect()));
    }
    let files = infos.iter().filter(|i| !i.is_dir).count();
    let dirs = infos.len() - files;
    let bytes: u64 = infos.iter().filter(|i| !i.is_dir).map(|i| i.size).sum();
    let ask = piped || paths.len() > 1 || dirs > 0;
    if ask && !call.has("yes") {
        let mut q = String::from("delete ");
        if files > 0 {
            q.push_str(&plural(files, "file", "files"));
            q.push_str(" (");
            q.push_str(&display::size(bytes as i64));
            q.push(')');
        }
        if dirs > 0 {
            if files > 0 {
                q.push_str(" and ");
            }
            q.push_str(&plural(dirs, "folder", "folders"));
            q.push_str(" with everything in them");
        }
        q.push('?');
        if !ctx.host.confirm(&q) {
            ctx.host.print("nothing deleted\n");
            return Ok(Value::Nothing);
        }
    }
    for (done, p) in paths.iter().enumerate() {
        ctx.check_interrupt().map_err(|e| {
            let mut e = e;
            e.msg.push_str(&alloc::format!(" after deleting {}", plural(done, "item", "items")));
            e
        })?;
        if let Err(e) = ctx.host.remove(p) {
            let mut m = String::from("couldn't delete ");
            m.push_str(p);
            m.push_str(": ");
            m.push_str(&e);
            let mut err = ShellError::new(m);
            if done > 0 {
                err = err.hint(alloc::format!("{} deleted before this", plural(done, "item was", "items were")));
            }
            return Err(err);
        }
    }
    Ok(Value::Nothing)
}

/// `move`/`copy`: sources and a destination. Into a folder if the
/// destination is one.
fn transfer(ctx: &mut Ctx, call: &Call, input: Value, moving: bool) -> Result<Value, ShellError> {
    let args = &call.positional;
    let (sources, dest) = if matches!(input, Value::Nothing) {
        if args.len() < 2 {
            return Err(call.err("needs what to move and where to").hint(if moving { "move <from> <to>" } else { "copy <from> <to>" }));
        }
        let (dest, srcs) = args.split_last().map(|(d, s)| (d.0.to_text(), s)).unwrap_or_default();
        (paths_from(&Value::Nothing, call, srcs)?.0, dest)
    } else {
        if args.len() != 1 {
            return Err(call.err("with paths piped in, give just the destination"));
        }
        (paths_from(&input, call, &[])?.0, args[0].0.to_text())
    };
    let into_dir = dest.ends_with('/') || ctx.host.stat(&dest).map(|i| i.is_dir).unwrap_or(false);
    if sources.len() > 1 && !into_dir {
        return Err(ShellError::at("several things can only go into a folder", call.pos_span(args.len().saturating_sub(1))));
    }
    // Plan every move first, so a clash stops it before anything moves.
    let mut plan = Vec::with_capacity(sources.len());
    for s in &sources {
        ctx.host.stat(s).map_err(|e| ShellError::new(alloc::format!("{s}: {e}")).hint("nothing was changed"))?;
        let to = if into_dir { join(&dest, basename(s)) } else { dest.clone() };
        if ctx.host.stat(&to).is_ok() {
            return Err(ShellError::new(alloc::format!("{to} is already there")).hint("nothing was changed"));
        }
        plan.push((s.clone(), to));
    }
    for (done, (from, to)) in plan.iter().enumerate() {
        let r = if moving { ctx.host.rename(from, to) } else { ctx.host.copy(from, to) };
        if let Err(e) = r {
            let mut err = ShellError::new(alloc::format!("couldn't {} {from} to {to}: {e}", if moving { "move" } else { "copy" }));
            if done > 0 {
                err = err.hint(alloc::format!("{} done before this", plural(done, "item was", "items were")));
            }
            return Err(err);
        }
    }
    Ok(Value::Nothing)
}

fn move_(ctx: &mut Ctx, call: &Call, input: Value) -> Result<Value, ShellError> {
    transfer(ctx, call, input, true)
}

fn copy(ctx: &mut Ctx, call: &Call, input: Value) -> Result<Value, ShellError> {
    transfer(ctx, call, input, false)
}

fn stat(ctx: &mut Ctx, call: &Call, _: Value) -> Result<Value, ShellError> {
    let path = call.str_at(0).unwrap_or("");
    let info = ctx.host.stat(path).map_err(|e| host_err(e, call, 0))?;
    let abs = ctx.host.absolute(path);
    match row(abs, &info, true) {
        Value::Record(r) => Ok(Value::Record(r)),
        v => Ok(v),
    }
}
