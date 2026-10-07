//! Runs the syntax tree. Each pipeline step finishes before the next
//! starts, and an error stops everything after it (docs/ks-design.md,
//! "Pipelines and errors").

use alloc::collections::BTreeMap;
use alloc::rc::Rc;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::fmt::Write;

use crate::ast::*;
use crate::builtins;
use crate::error::{ShellError, Span};
use crate::parser;
use crate::sig::{Shape, Signature};
use crate::value::{self, Closure, Record, Value};
use crate::Host;

/// How deeply functions and closures may call each other.
const MAX_CALLS: usize = 128;

pub type CmdFn = fn(&mut Ctx, &Call, Value) -> Result<Value, ShellError>;

#[derive(Clone, Copy)]
pub enum Runner {
    /// Takes values, returns a value.
    Native(CmdFn),
    /// One of the kernel's original commands: gets the rest of the line as
    /// typed, prints, and returns nothing.
    Text(fn(&str)),
}

pub struct Command {
    pub sig: Signature,
    pub run: Runner,
}

/// A command's arguments, evaluated.
pub struct Call {
    pub name: String,
    pub head: Span,
    pub span: Span,
    pub positional: Vec<(Value, Span)>,
    pub flags: Vec<(String, Option<Value>, Span)>,
    pub raw: String,
}

impl Call {
    pub fn pos(&self, i: usize) -> Option<&Value> {
        self.positional.get(i).map(|(v, _)| v)
    }

    pub fn pos_span(&self, i: usize) -> Span {
        self.positional.get(i).map(|(_, s)| *s).unwrap_or(self.head)
    }

    pub fn has(&self, flag: &str) -> bool {
        self.flags.iter().any(|(n, _, _)| n == flag)
    }

    pub fn flag(&self, flag: &str) -> Option<&Value> {
        self.flags.iter().find(|(n, _, _)| n == flag).and_then(|(_, v, _)| v.as_ref())
    }

    /// The `i`th argument as a string (the parser already checked it is
    /// one, if the signature says so).
    pub fn str_at(&self, i: usize) -> Option<&str> {
        self.pos(i).and_then(|v| v.as_str())
    }

    pub fn int_at(&self, i: usize) -> Option<i64> {
        self.pos(i).and_then(|v| v.as_int())
    }

    pub fn err(&self, msg: impl Into<String>) -> ShellError {
        ShellError::at(msg, self.head)
    }
}

struct Var {
    name: String,
    value: Value,
    mutable: bool,
}

pub struct Engine {
    cmds: BTreeMap<String, Command>,
    defs: BTreeMap<String, Rc<Def>>,
    /// Innermost last. The first is the prompt's: it lasts the session.
    scopes: Vec<Vec<Var>>,
    /// Every line and script parsed, so errors can quote them.
    sources: Vec<Rc<str>>,
    calls: usize,
}

impl Default for Engine {
    fn default() -> Self {
        Engine::new()
    }
}

/// Non-error ways out of a block.
pub(crate) enum Flow {
    Err(ShellError),
    Break(Span),
    Continue(Span),
    Return(Value),
}

impl From<ShellError> for Flow {
    fn from(e: ShellError) -> Flow {
        Flow::Err(e)
    }
}

fn flow_to_err(f: Flow) -> ShellError {
    match f {
        Flow::Err(e) => e,
        Flow::Break(s) => ShellError::at("`break` outside a loop", s),
        Flow::Continue(s) => ShellError::at("`continue` outside a loop", s),
        Flow::Return(_) => ShellError::new("`return` outside a function"),
    }
}

impl Engine {
    pub fn new() -> Engine {
        let mut e = Engine { cmds: BTreeMap::new(), defs: BTreeMap::new(), scopes: alloc::vec![Vec::new()], sources: Vec::new(), calls: 0 };
        builtins::register(&mut e);
        e
    }

    pub fn register(&mut self, sig: Signature, run: Runner) {
        self.cmds.insert(sig.name.clone(), Command { sig, run });
    }

    /// Registers `alias` as another name for `target`.
    pub fn alias(&mut self, alias: &str, target: &str) {
        if let Some(c) = self.cmds.get(target) {
            let mut sig = c.sig.clone();
            sig.name = alias.to_string();
            let mut s = String::from("same as `");
            s.push_str(target);
            s.push('`');
            sig.summary = s;
            let run = c.run;
            self.register(sig, run);
        }
    }

    pub fn signature(&self, name: &str) -> Option<&Signature> {
        self.defs.get(name).map(|d| &d.sig).or_else(|| self.cmds.get(name).map(|c| &c.sig))
    }

    pub fn command_names(&self) -> impl Iterator<Item = String> + '_ {
        self.cmds.keys().cloned().chain(self.defs.keys().filter(|k| !self.cmds.contains_key(*k)).cloned())
    }

    /// Every command's signature, built-ins first then `def`s.
    pub fn signatures(&self) -> impl Iterator<Item = &Signature> + '_ {
        self.cmds.values().map(|c| &c.sig).chain(self.defs.values().map(|d| &d.sig))
    }

    fn add_source(&mut self, text: &str) -> u32 {
        self.sources.push(Rc::from(text));
        (self.sources.len() - 1) as u32
    }

    /// Sets a variable at the prompt's level.
    pub fn set_var(&mut self, name: &str, value: Value) {
        let globals = &mut self.scopes[0];
        match globals.iter_mut().find(|v| v.name == name) {
            Some(v) => v.value = value,
            None => globals.push(Var { name: name.to_string(), value, mutable: false }),
        }
    }

    /// Parses and runs a line from the prompt. Results of all but the last
    /// statement are shown as they're made; the last is returned.
    pub fn run(&mut self, src: &str, host: &mut dyn Host) -> Result<Value, ShellError> {
        self.run_source(src, host, true)
    }

    /// Parses and runs a script. Only what it prints is shown.
    pub fn run_script(&mut self, src: &str, host: &mut dyn Host) -> Result<Value, ShellError> {
        self.run_source(src, host, false)
    }

    fn run_source(&mut self, src: &str, host: &mut dyn Host, show: bool) -> Result<Value, ShellError> {
        let id = self.add_source(src);
        let block = parser::parse(self, src, id)?;
        // Leave nothing behind from a run that was cut short.
        self.scopes.truncate(1);
        self.calls = 0;
        let mut ctx = Ctx { engine: self, host };
        let r = ctx.run_stmts(&block, Value::Nothing, show);
        let r = match r {
            Ok(v) | Err(Flow::Return(v)) => Ok(v),
            Err(f) => Err(flow_to_err(f)),
        };
        self.scopes.truncate(1);
        r
    }

    /// The error as the user sees it: the message, then the line it came
    /// from with the blamed part underlined.
    pub fn render_error(&self, e: &ShellError) -> String {
        let mut out = String::from("error: ");
        out.push_str(&e.msg);
        let span = match e.span {
            Some(s) => s,
            None => {
                if let Some(h) = &e.hint {
                    out.push_str("\n  ");
                    out.push_str(h);
                }
                return out;
            }
        };
        let src = match self.sources.get(span.src as usize) {
            Some(s) => s.clone(),
            None => return out,
        };
        let start = (span.start as usize).min(src.len());
        let end = (span.end as usize).clamp(start, src.len());
        let line_start = src[..start].rfind('\n').map(|i| i + 1).unwrap_or(0);
        let line_end = src[start..].find('\n').map(|i| start + i).unwrap_or(src.len());
        let line = src[line_start..line_end].trim_end_matches('\r');
        let multi = src.contains('\n');
        let gutter = if multi {
            let n = src[..line_start].matches('\n').count() + 1;
            let mut g = String::new();
            let _ = write!(g, "{n:>3} | ");
            g
        } else {
            String::from("  | ")
        };
        out.push('\n');
        out.push_str(&gutter);
        out.push_str(line);
        out.push('\n');
        for _ in 0..gutter.len() - 2 {
            out.push(' ');
        }
        out.push_str("| ");
        let col = src[line_start..start].chars().count();
        let width = src[start..end.min(line_end)].chars().count().max(1);
        for _ in 0..col {
            out.push(' ');
        }
        for _ in 0..width {
            out.push('^');
        }
        if let Some(h) = &e.hint {
            out.push(' ');
            out.push_str(h);
        }
        out
    }
}

/// What a command gets: the engine (to run closures) and the host.
pub struct Ctx<'a> {
    pub engine: &'a mut Engine,
    pub host: &'a mut dyn Host,
}

impl<'a> Ctx<'a> {
    pub fn check_interrupt(&mut self) -> Result<(), ShellError> {
        if self.host.interrupted() {
            return Err(ShellError::new("stopped (Ctrl+C)"));
        }
        Ok(())
    }

    fn lookup(&self, name: &str) -> Option<&Value> {
        for scope in self.engine.scopes.iter().rev() {
            if let Some(v) = scope.iter().rev().find(|v| v.name == name) {
                return Some(&v.value);
            }
        }
        None
    }

    fn declare(&mut self, name: &str, value: Value, mutable: bool) {
        if let Some(scope) = self.engine.scopes.last_mut() {
            scope.push(Var { name: name.to_string(), value, mutable });
        }
    }

    /// Runs `f` in a new scope, which is dropped afterwards whatever
    /// happens.
    fn scoped<T>(&mut self, f: impl FnOnce(&mut Self) -> T) -> T {
        self.engine.scopes.push(Vec::new());
        let r = f(self);
        self.engine.scopes.pop();
        r
    }

    fn run_stmts(&mut self, block: &Block, input: Value, show: bool) -> Result<Value, Flow> {
        let mut last = Value::Nothing;
        let n = block.stmts.len();
        for (i, stmt) in block.stmts.iter().enumerate() {
            last = self.stmt(stmt, &input)?;
            if show && i + 1 < n && !matches!(last, Value::Nothing) {
                let w = self.host.width();
                let mut text = crate::display::render(&last, w);
                text.push('\n');
                self.host.print(&text);
            }
        }
        Ok(last)
    }

    fn block(&mut self, block: &Block, input: Value) -> Result<Value, Flow> {
        self.scoped(|c| c.run_stmts(block, input, false))
    }

    fn stmt(&mut self, stmt: &Stmt, input: &Value) -> Result<Value, Flow> {
        match stmt {
            Stmt::Pipeline(p) => self.pipeline(p, input.clone()),
            Stmt::Let { name, mutable, value, .. } => {
                let v = self.pipeline(value, Value::Nothing)?;
                self.declare(name, v, *mutable);
                Ok(Value::Nothing)
            }
            Stmt::Assign { name, value, span } => {
                let v = self.pipeline(value, Value::Nothing)?;
                for scope in self.engine.scopes.iter_mut().rev() {
                    if let Some(var) = scope.iter_mut().rev().find(|x| x.name == *name) {
                        if !var.mutable {
                            let mut m = String::from("`");
                            m.push_str(name);
                            m.push_str("` was made with `let`, so it can't change");
                            let mut h = String::from("make it with `mut ");
                            h.push_str(name);
                            h.push_str(" = ...` instead");
                            return Err(ShellError::at(m, *span).hint(h).into());
                        }
                        var.value = v;
                        return Ok(Value::Nothing);
                    }
                }
                let mut m = String::from("there's no variable `");
                m.push_str(name);
                m.push('`');
                let mut h = String::from("make one with `mut ");
                h.push_str(name);
                h.push_str(" = ...`");
                Err(ShellError::at(m, *span).hint(h).into())
            }
            Stmt::Def(def) => {
                self.engine.defs.insert(def.sig.name.clone(), def.clone());
                Ok(Value::Nothing)
            }
            Stmt::If { cond, then, els } => {
                let c = self.expr(cond)?;
                if c.truthy("the `if` condition").map_err(|e| e.or_at(cond.span))? {
                    self.block(then, Value::Nothing)
                } else if let Some(els) = els {
                    self.block(els, Value::Nothing)
                } else {
                    Ok(Value::Nothing)
                }
            }
            Stmt::For { var, iter, body } => {
                let items = match self.expr(iter)? {
                    Value::List(l) => l,
                    Value::Nothing => Vec::new(),
                    other => {
                        let mut m = String::from("can't loop over ");
                        m.push_str(other.a_type());
                        let mut e = ShellError::at(m, iter.span);
                        if matches!(other, Value::String(_)) {
                            e = e.hint("to loop over a command's output, put it in parentheses: for x in (ls) { ... }");
                        }
                        return Err(e.into());
                    }
                };
                for item in items {
                    self.check_interrupt()?;
                    let r = self.scoped(|c| {
                        c.declare(var, item, false);
                        c.run_stmts(body, Value::Nothing, false)
                    });
                    match r {
                        Ok(_) | Err(Flow::Continue(_)) => {}
                        Err(Flow::Break(_)) => break,
                        Err(f) => return Err(f),
                    }
                }
                Ok(Value::Nothing)
            }
            Stmt::While { cond, body } => {
                loop {
                    self.check_interrupt()?;
                    let c = self.expr(cond)?;
                    if !c.truthy("the `while` condition").map_err(|e| e.or_at(cond.span))? {
                        break;
                    }
                    match self.block(body, Value::Nothing) {
                        Ok(_) | Err(Flow::Continue(_)) => {}
                        Err(Flow::Break(_)) => break,
                        Err(f) => return Err(f),
                    }
                }
                Ok(Value::Nothing)
            }
            Stmt::Break(s) => Err(Flow::Break(*s)),
            Stmt::Continue(s) => Err(Flow::Continue(*s)),
            Stmt::Return(value, _) => {
                let v = match value {
                    Some(p) => self.pipeline(p, input.clone())?,
                    None => Value::Nothing,
                };
                Err(Flow::Return(v))
            }
        }
    }

    fn pipeline(&mut self, p: &Pipeline, input: Value) -> Result<Value, Flow> {
        let mut v = input;
        for (i, el) in p.elements.iter().enumerate() {
            self.check_interrupt()?;
            v = match el {
                Element::Call(c) => self.call(c, v)?,
                Element::Expr(e) if i == 0 => self.expr(e)?,
                Element::Expr(e) => self.scoped(|c| {
                    c.declare("in", v, false);
                    c.expr(e)
                })?,
            };
        }
        Ok(v)
    }

    fn call(&mut self, c: &CallAst, input: Value) -> Result<Value, Flow> {
        let mut call = Call { name: c.name.clone(), head: c.head, span: c.span, positional: Vec::new(), flags: Vec::new(), raw: c.raw.clone() };
        for arg in &c.args {
            match arg {
                Arg::Positional(e) => {
                    let v = self.expr(e)?;
                    call.positional.push((v, e.span));
                }
                Arg::Flag { name, value, span } => {
                    let v = match value {
                        Some(e) => Some(self.expr(e)?),
                        None => None,
                    };
                    call.flags.push((name.clone(), v, *span));
                }
            }
        }
        if let Some(def) = self.engine.defs.get(&c.name).cloned() {
            check_args(&def.sig, &call)?;
            return self.call_def(&def, &call, input).map_err(Flow::Err);
        }
        let (run, apex) = match self.engine.cmds.get(&c.name) {
            Some(cmd) => {
                check_args(&cmd.sig, &call)?;
                (cmd.run, cmd.sig.apex)
            }
            None => {
                let mut m = String::from("unknown command `");
                m.push_str(&c.name);
                m.push('`');
                return Err(ShellError::at(m, c.head).into());
            }
        };
        if apex && !self.host.apex(&c.name) {
            return Err(ShellError::at("apex authentication failed", c.head).into());
        }
        match run {
            Runner::Text(f) => {
                f(&call.raw);
                Ok(Value::Nothing)
            }
            Runner::Native(f) => f(self, &call, input).map_err(|e| {
                let mut prefix = String::from(&c.name);
                prefix.push(':');
                let e = if e.msg.starts_with(&prefix) { e } else { e.prefixed(&c.name) };
                Flow::Err(e.or_at(c.head))
            }),
        }
    }

    fn enter_call(&mut self, span: Span) -> Result<(), ShellError> {
        if self.engine.calls >= MAX_CALLS {
            return Err(ShellError::at("too many calls inside each other (is something calling itself forever?)", span));
        }
        self.engine.calls += 1;
        Ok(())
    }

    fn call_def(&mut self, def: &Def, call: &Call, input: Value) -> Result<Value, ShellError> {
        self.enter_call(call.head)?;
        let mut frame = Vec::new();
        let sig = &def.sig;
        for (i, p) in sig.params.iter().enumerate() {
            let v = call.pos(i).cloned().or_else(|| p.default.clone()).unwrap_or(Value::Nothing);
            frame.push(Var { name: p.name.replace('-', "_"), value: v, mutable: false });
        }
        if let Some(rest) = &sig.rest {
            let items = call.positional.iter().skip(sig.params.len()).map(|(v, _)| v.clone()).collect();
            frame.push(Var { name: rest.name.replace('-', "_"), value: Value::List(items), mutable: false });
        }
        for f in &sig.flags {
            let v = match f.arg {
                None => Value::Bool(call.has(&f.long)),
                Some(_) => call.flag(&f.long).cloned().or_else(|| f.default.clone()).unwrap_or(Value::Nothing),
            };
            frame.push(Var { name: f.long.replace('-', "_"), value: v, mutable: false });
        }
        frame.push(Var { name: "in".to_string(), value: input.clone(), mutable: false });
        let saved = core::mem::replace(&mut self.engine.scopes, alloc::vec![frame]);
        let r = self.run_stmts(&def.body, input, false);
        self.engine.scopes = saved;
        self.engine.calls -= 1;
        match r {
            Ok(v) | Err(Flow::Return(v)) => Ok(v),
            Err(f) => Err(flow_to_err(f)),
        }
    }

    /// Runs a closure with `args` as its parameters and `input` as `$in`.
    pub fn call_closure(&mut self, c: &Closure, args: Vec<Value>, input: Value) -> Result<Value, ShellError> {
        self.check_interrupt()?;
        self.enter_call(c.ast.span)?;
        let mut frame: Vec<Var> = c.captured.iter().map(|(n, v)| Var { name: n.clone(), value: v.clone(), mutable: false }).collect();
        let mut args = args.into_iter();
        for p in &c.ast.params {
            frame.push(Var { name: p.clone(), value: args.next().unwrap_or(Value::Nothing), mutable: false });
        }
        frame.push(Var { name: "in".to_string(), value: input.clone(), mutable: false });
        let saved = core::mem::replace(&mut self.engine.scopes, alloc::vec![frame]);
        let r = self.run_stmts(&c.ast.body, input, false);
        self.engine.scopes = saved;
        self.engine.calls -= 1;
        match r {
            Ok(v) | Err(Flow::Return(v)) => Ok(v),
            Err(f) => Err(flow_to_err(f)),
        }
    }

    /// Parses and runs `src` (a script's text) in this session.
    pub fn run_script(&mut self, src: &str) -> Result<Value, ShellError> {
        let id = self.engine.add_source(src);
        let block = parser::parse(self.engine, src, id)?;
        self.enter_call(Span::default())?;
        let r = self.scoped(|c| c.run_stmts(&block, Value::Nothing, false));
        self.engine.calls -= 1;
        match r {
            Ok(v) | Err(Flow::Return(v)) => Ok(v),
            Err(f) => Err(flow_to_err(f)),
        }
    }

    fn expr(&mut self, e: &Expr) -> Result<Value, Flow> {
        Ok(match &e.kind {
            ExprKind::Lit(v) => v.clone(),
            ExprKind::Interp(parts) => {
                let mut s = String::new();
                for p in parts {
                    match p {
                        Part::Text(t) => s.push_str(t),
                        Part::Expr(x) => s.push_str(&self.expr(x)?.to_text()),
                    }
                }
                Value::String(s)
            }
            ExprKind::Var(name, members) => {
                let v = match self.lookup(name) {
                    Some(v) => v,
                    None => {
                        let mut m = String::from("there's no variable `$");
                        m.push_str(name);
                        m.push('`');
                        return Err(ShellError::at(m, e.span).into());
                    }
                };
                follow(v, members, e.span)?
            }
            ExprKind::Column(path) => {
                let row = match self.lookup("in") {
                    Some(v) => v,
                    None => return Err(ShellError::at("there's no row here", e.span).into()),
                };
                if !matches!(row, Value::Record(_)) {
                    let mut m = String::from("can't get column `");
                    m.push_str(path.first().map(|s| s.as_str()).unwrap_or(""));
                    m.push_str("` of ");
                    m.push_str(row.a_type());
                    return Err(ShellError::at(m, e.span).into());
                }
                follow(row, path, e.span)?
            }
            ExprKind::Sub(block, members) => {
                let v = self.block(block, Value::Nothing)?;
                if members.is_empty() {
                    v
                } else {
                    follow(&v, members, e.span)?
                }
            }
            ExprKind::List(items) => {
                let mut out = Vec::with_capacity(items.len());
                for i in items {
                    out.push(self.expr(i)?);
                }
                Value::List(out)
            }
            ExprKind::Record(fields) => {
                let mut r = Record::new();
                for (k, x) in fields {
                    let v = self.expr(x)?;
                    r.insert(k, v);
                }
                Value::Record(r)
            }
            ExprKind::Closure(ast) => {
                let mut captured = Vec::new();
                for name in &ast.uses {
                    if let Some(v) = self.lookup(name) {
                        captured.push((name.clone(), v.clone()));
                    }
                }
                Value::Closure(Rc::new(Closure { ast: ast.clone(), captured }))
            }
            ExprKind::Binary(l, op, _, r) => {
                let lv = self.expr(l)?;
                if matches!(op, Op::And | Op::Or) {
                    let lb = lv.truthy("the left side of `and`/`or`").map_err(|x| x.or_at(l.span))?;
                    if (*op == Op::And && !lb) || (*op == Op::Or && lb) {
                        return Ok(Value::Bool(lb));
                    }
                    let rb = self.expr(r)?.truthy("the right side of `and`/`or`").map_err(|x| x.or_at(r.span))?;
                    return Ok(Value::Bool(rb));
                }
                let rv = self.expr(r)?;
                value::binary(*op, &lv, &rv).map_err(|x| x.or_at(e.span))?
            }
            ExprKind::Not(x) => {
                let v = self.expr(x)?;
                Value::Bool(!v.truthy("`not`").map_err(|y| y.or_at(x.span))?)
            }
            ExprKind::Neg(x) => match self.expr(x)? {
                Value::Int(i) => Value::Int(i.checked_neg().ok_or_else(|| ShellError::at("the result is too big", e.span))?),
                Value::Float(f) => Value::Float(-f),
                Value::Size(s) => Value::Size(s.checked_neg().ok_or_else(|| ShellError::at("the result is too big", e.span))?),
                Value::Duration(d) => Value::Duration(d.checked_neg().ok_or_else(|| ShellError::at("the result is too big", e.span))?),
                other => {
                    let mut m = String::from("can't make ");
                    m.push_str(other.a_type());
                    m.push_str(" negative");
                    return Err(ShellError::at(m, e.span).into());
                }
            },
        })
    }
}

fn follow(v: &Value, members: &[String], span: Span) -> Result<Value, ShellError> {
    let mut cur = v.clone();
    for m in members {
        cur = cur.member(m).map_err(|e| e.or_at(span))?;
    }
    Ok(cur)
}

/// Checks evaluated arguments against the signature's types.
fn check_args(sig: &Signature, call: &Call) -> Result<(), ShellError> {
    for (i, (v, span)) in call.positional.iter().enumerate() {
        let (shape, pname) = match sig.params.get(i) {
            Some(p) => (p.shape, &p.name),
            None => match &sig.rest {
                Some(r) => (r.shape, &r.name),
                None => continue,
            },
        };
        if !shape.accepts(v) {
            let mut m = String::from(&sig.name);
            m.push_str(": <");
            m.push_str(pname);
            m.push_str("> should be ");
            m.push_str(shape_article(shape));
            m.push_str(", not ");
            m.push_str(v.a_type());
            return Err(ShellError::at(m, *span));
        }
    }
    for (name, v, span) in &call.flags {
        if let (Some(f), Some(v)) = (sig.flag_by_long(name), v) {
            if let Some(shape) = f.arg {
                if !shape.accepts(v) {
                    let mut m = String::from(&sig.name);
                    m.push_str(": --");
                    m.push_str(name);
                    m.push_str(" should be ");
                    m.push_str(shape_article(shape));
                    m.push_str(", not ");
                    m.push_str(v.a_type());
                    return Err(ShellError::at(m, *span));
                }
            }
        }
    }
    Ok(())
}

fn shape_article(s: Shape) -> &'static str {
    match s {
        Shape::Any => "anything",
        Shape::Int => "an int",
        Shape::Float => "a float",
        Shape::Number => "a number",
        Shape::String => "a string",
        Shape::Path => "a path",
        Shape::Bool => "a bool",
        Shape::Size => "a size",
        Shape::Duration => "a duration",
        Shape::Date => "a date",
        Shape::List => "a list",
        Shape::Record => "a record",
        Shape::Table => "a table",
        Shape::Closure => "a closure",
        Shape::Condition => "a condition",
    }
}
