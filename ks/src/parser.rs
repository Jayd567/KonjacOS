//! Tokens -> syntax tree. Commands are parsed using their signatures, so
//! the parser knows `sort-by size --reverse` is one argument and a switch,
//! that `filter`'s argument is a row condition, and that a text command
//! wants the rest of the line as it was typed. Unknown commands and flags
//! are errors here, before anything runs.

use alloc::rc::Rc;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use crate::ast::*;
use crate::error::{ShellError, Span};
use crate::eval::Engine;
use crate::lexer::{lex, Tok, Token};
use crate::sig::{Flag, Param, Shape, Signature};
use crate::value::{parse_literal, Value};

/// How deeply blocks and expressions may nest. The parser and evaluator
/// both recurse, and the kernel's stack is finite.
const MAX_DEPTH: usize = 64;

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Normal,
    /// In a row condition: bare names are columns.
    Row,
}

pub struct Parser<'a> {
    toks: Vec<Token>,
    pos: usize,
    src: &'a str,
    src_id: u32,
    engine: &'a Engine,
    /// `def`s seen so far in this source.
    defs: Vec<Signature>,
    depth: usize,
    /// One list per closure being parsed: the variables it uses.
    uses: Vec<Vec<String>>,
}

/// Parses a whole source: a line from the prompt, or a script.
pub fn parse(engine: &Engine, src: &str, src_id: u32) -> Result<Block, ShellError> {
    let toks = lex(src, src_id, 0)?;
    let mut p = Parser { toks, pos: 0, src, src_id, engine, defs: Vec::new(), depth: 0, uses: Vec::new() };
    let block = p.block()?;
    match &p.peek().tok {
        Tok::Eof => Ok(block),
        _ => Err(p.unexpected()),
    }
}

fn describe(t: &Tok) -> String {
    match t {
        Tok::Word(w) => {
            let mut s = String::from("`");
            s.push_str(w);
            s.push('`');
            s
        }
        Tok::DStr(_) | Tok::SStr(_) => "a string".to_string(),
        Tok::Pipe => "`|`".to_string(),
        Tok::Semi => "`;`".to_string(),
        Tok::Newline => "the end of the line".to_string(),
        Tok::LParen => "`(`".to_string(),
        Tok::RParen => "`)`".to_string(),
        Tok::LBracket => "`[`".to_string(),
        Tok::RBracket => "`]`".to_string(),
        Tok::LBrace => "`{`".to_string(),
        Tok::RBrace => "`}`".to_string(),
        Tok::Comma => "`,`".to_string(),
        Tok::Eof => "the end of the line".to_string(),
    }
}

fn is_ident(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// How many single-character edits turn `a` into `b`.
fn distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut cur = Vec::with_capacity(b.len() + 1);
        cur.push(i + 1);
        for (j, cb) in b.iter().enumerate() {
            let sub = prev[j] + if ca == *cb { 0 } else { 1 };
            cur.push(sub.min(prev[j + 1] + 1).min(cur[j] + 1));
        }
        prev = cur;
    }
    prev[b.len()]
}

/// Whether a word at the start of a pipeline step begins an expression
/// rather than naming a command.
fn starts_expression(w: &str) -> bool {
    w.starts_with('$') || w.starts_with("-$") || w == "not" || parse_literal(w).is_some()
}

/// Whether running `word` means running a program or script from disk.
fn looks_like_program(word: &str) -> bool {
    let lower: String = word.chars().map(|c| c.to_ascii_lowercase()).collect();
    word.contains('/') || [".exe", ".elf", ".bin", ".ks"].iter().any(|e| lower.ends_with(e))
}

impl<'a> Parser<'a> {
    fn peek(&self) -> &Token {
        // The token list always ends in Eof, and `pos` never passes it.
        &self.toks[self.pos.min(self.toks.len() - 1)]
    }

    fn peek_at(&self, n: usize) -> &Token {
        &self.toks[(self.pos + n).min(self.toks.len() - 1)]
    }

    fn next(&mut self) -> Token {
        let t = self.peek().clone();
        if self.pos < self.toks.len() - 1 {
            self.pos += 1;
        }
        t
    }

    fn span(&self) -> Span {
        self.peek().span
    }

    fn prev_span(&self) -> Span {
        if self.pos == 0 {
            return self.span();
        }
        self.toks[self.pos - 1].span
    }

    fn unexpected(&self) -> ShellError {
        let t = self.peek();
        let mut m = String::from("unexpected ");
        m.push_str(&describe(&t.tok));
        ShellError::at(m, t.span)
    }

    fn expect(&mut self, want: Tok, what: &str) -> Result<Span, ShellError> {
        if self.peek().tok == want {
            return Ok(self.next().span);
        }
        let mut m = String::from("expected ");
        m.push_str(what);
        m.push_str(", found ");
        m.push_str(&describe(&self.peek().tok));
        Err(ShellError::at(m, self.span()))
    }

    fn is_word(&self, w: &str) -> bool {
        matches!(&self.peek().tok, Tok::Word(x) if x == w)
    }

    fn skip_newlines(&mut self) {
        while self.peek().tok == Tok::Newline {
            self.next();
        }
    }

    fn enter(&mut self) -> Result<(), ShellError> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err(ShellError::at("this is nested too deeply", self.span()));
        }
        Ok(())
    }

    fn leave(&mut self) {
        self.depth -= 1;
    }

    fn text(&self, span: Span) -> &str {
        self.src.get(span.start as usize..span.end as usize).unwrap_or("")
    }

    fn signature(&self, name: &str) -> Option<&Signature> {
        self.defs.iter().rev().find(|s| s.name == name).or_else(|| self.engine.signature(name))
    }

    fn note_use(&mut self, name: &str) {
        for list in self.uses.iter_mut() {
            if !list.iter().any(|n| n == name) {
                list.push(name.to_string());
            }
        }
    }

    // --- Statements ----------------------------------------------------

    /// Statements until `}`, `)` or the end.
    fn block(&mut self) -> Result<Block, ShellError> {
        self.enter()?;
        let mut stmts = Vec::new();
        loop {
            while matches!(self.peek().tok, Tok::Newline | Tok::Semi) {
                self.next();
            }
            if matches!(self.peek().tok, Tok::Eof | Tok::RBrace | Tok::RParen) {
                break;
            }
            stmts.push(self.statement()?);
            match self.peek().tok {
                Tok::Newline | Tok::Semi | Tok::Eof | Tok::RBrace | Tok::RParen => {}
                _ => return Err(self.unexpected()),
            }
        }
        self.leave();
        Ok(Block { stmts })
    }

    /// `{ statements }`.
    fn braced_block(&mut self) -> Result<Rc<Block>, ShellError> {
        self.expect(Tok::LBrace, "`{`")?;
        let b = self.block()?;
        self.expect(Tok::RBrace, "`}`")?;
        Ok(Rc::new(b))
    }

    fn statement(&mut self) -> Result<Stmt, ShellError> {
        let start = self.span();
        let word = match &self.peek().tok {
            Tok::Word(w) => w.clone(),
            _ => return Ok(Stmt::Pipeline(self.pipeline()?)),
        };
        match word.as_str() {
            "let" | "mut" => {
                self.next();
                let name = self.var_name()?;
                self.expect(Tok::Word("=".to_string()), "`=`")?;
                let value = self.value_pipeline()?;
                Ok(Stmt::Let { name, mutable: word == "mut", value, span: start.to(self.prev_span()) })
            }
            "def" => self.def(),
            "if" => self.if_stmt(),
            "for" => {
                self.next();
                let var = self.var_name()?;
                if !self.is_word("in") {
                    return Err(ShellError::at("expected `in`", self.span()));
                }
                self.next();
                let iter = self.expr(Mode::Normal, 0)?;
                let body = self.braced_block()?;
                Ok(Stmt::For { var, iter, body })
            }
            "while" => {
                self.next();
                let cond = self.expr(Mode::Normal, 0)?;
                let body = self.braced_block()?;
                Ok(Stmt::While { cond, body })
            }
            "break" => Ok(Stmt::Break(self.next().span)),
            "continue" => Ok(Stmt::Continue(self.next().span)),
            "return" => {
                let span = self.next().span;
                let value = match self.peek().tok {
                    Tok::Newline | Tok::Semi | Tok::Eof | Tok::RBrace | Tok::RParen => None,
                    _ => Some(self.pipeline()?),
                };
                Ok(Stmt::Return(value, span))
            }
            _ => {
                // `name = value` (or `$name = value`).
                let next_is_eq = matches!(&self.peek_at(1).tok, Tok::Word(w) if w == "=") && self.peek_at(1).spaced;
                let bare = word.strip_prefix('$').unwrap_or(&word);
                if next_is_eq && is_ident(bare) && (word.starts_with('$') || self.signature(&word).is_none()) {
                    let name = bare.to_string();
                    self.next();
                    self.next();
                    let value = self.value_pipeline()?;
                    return Ok(Stmt::Assign { name, value, span: start.to(self.prev_span()) });
                }
                Ok(Stmt::Pipeline(self.pipeline()?))
            }
        }
    }

    /// The right side of `let x = ...`: a pipeline, except that a lone
    /// word that isn't a command is just text (`let name = konjac`).
    fn value_pipeline(&mut self) -> Result<Pipeline, ShellError> {
        let t = self.peek().clone();
        if let Tok::Word(w) = &t.tok {
            let ends = matches!(self.peek_at(1).tok, Tok::Newline | Tok::Semi | Tok::Eof | Tok::RParen | Tok::RBrace);
            if ends && !starts_expression(w) && self.signature(w).is_none() {
                self.next();
                let e = Expr { kind: ExprKind::Lit(Value::String(w.clone())), span: t.span };
                return Ok(Pipeline { elements: alloc::vec![Element::Expr(e)] });
            }
        }
        self.pipeline()
    }

    fn var_name(&mut self) -> Result<String, ShellError> {
        let t = self.next();
        if let Tok::Word(w) = &t.tok {
            let name = w.strip_prefix('$').unwrap_or(w);
            if is_ident(name) {
                return Ok(name.to_string());
            }
        }
        Err(ShellError::at("expected a variable name", t.span))
    }

    fn if_stmt(&mut self) -> Result<Stmt, ShellError> {
        self.next(); // `if`
        let cond = self.expr(Mode::Normal, 0)?;
        let then = self.braced_block()?;
        // `else` may start the next line.
        let save = self.pos;
        self.skip_newlines();
        if !self.is_word("else") {
            self.pos = save;
            return Ok(Stmt::If { cond, then, els: None });
        }
        self.next();
        let els = if self.is_word("if") {
            let nested = self.if_stmt()?;
            Rc::new(Block { stmts: alloc::vec![nested] })
        } else {
            self.braced_block()?
        };
        Ok(Stmt::If { cond, then, els: Some(els) })
    }

    /// `def name [params] { body }`.
    fn def(&mut self) -> Result<Stmt, ShellError> {
        let start = self.next().span;
        let name_tok = self.next();
        let name = match &name_tok.tok {
            Tok::Word(w) if is_ident(w) => w.clone(),
            Tok::DStr(s) | Tok::SStr(s) if !s.is_empty() => s.clone(),
            _ => return Err(ShellError::at("expected the function's name", name_tok.span)),
        };
        let mut sig = Signature::build(&name, "custom", "");
        self.expect(Tok::LBracket, "`[` and the parameters")?;
        // Gather the words between the brackets, splitting `name:type`.
        let mut words: Vec<(String, Span)> = Vec::new();
        loop {
            let t = self.next();
            match t.tok {
                Tok::RBracket => break,
                Tok::Comma | Tok::Newline => {}
                Tok::Word(w) => {
                    let mut rest = w.as_str();
                    while let Some(i) = rest.find(':') {
                        if i > 0 {
                            words.push((rest[..i].to_string(), t.span));
                        }
                        words.push((":".to_string(), t.span));
                        rest = &rest[i + 1..];
                    }
                    if !rest.is_empty() {
                        words.push((rest.to_string(), t.span));
                    }
                }
                Tok::DStr(s) | Tok::SStr(s) => {
                    let mut q = String::from("\"");
                    q.push_str(&s);
                    words.push((q, t.span));
                }
                Tok::Eof => return Err(ShellError::at("the parameters have no closing `]`", t.span)),
                _ => return Err(ShellError::at("unexpected in the parameters", t.span)),
            }
        }
        let mut i = 0;
        while i < words.len() {
            let (w, span) = (words[i].0.clone(), words[i].1);
            i += 1;
            // `: type`
            let mut shape = None;
            if words.get(i).is_some_and(|(x, _)| x == ":") {
                let (tname, tspan) = match words.get(i + 1) {
                    Some(x) => x.clone(),
                    None => return Err(ShellError::at("expected a type after `:`", span)),
                };
                match Shape::from_name(&tname) {
                    Some(s) => shape = Some(s),
                    None => return Err(ShellError::at("unknown type", tspan).hint("types: any int float number string path bool size duration date list record table closure")),
                }
                i += 2;
            }
            // `= default`
            let mut default = None;
            if words.get(i).is_some_and(|(x, _)| x == "=") {
                let (d, _) = match words.get(i + 1) {
                    Some(x) => x.clone(),
                    None => return Err(ShellError::at("expected a default value after `=`", span)),
                };
                default = Some(match d.strip_prefix('"') {
                    Some(text) => Value::String(text.to_string()),
                    None => parse_literal(&d).unwrap_or(Value::String(d)),
                });
                i += 2;
            }
            if let Some(long) = w.strip_prefix("--") {
                if !is_ident(long) {
                    return Err(ShellError::at("expected a flag name", span));
                }
                sig.flags.push(Flag { long: long.to_string(), short: None, arg: shape, default, desc: String::new() });
            } else if let Some(rest) = w.strip_prefix("...") {
                if !is_ident(rest) {
                    return Err(ShellError::at("expected a parameter name", span));
                }
                sig.rest = Some(Param { name: rest.to_string(), shape: shape.unwrap_or(Shape::Any), optional: true, default: None, desc: String::new() });
            } else {
                let (pname, optional) = match w.strip_suffix('?') {
                    Some(n) => (n, true),
                    None => (w.as_str(), default.is_some()),
                };
                if !is_ident(pname) {
                    return Err(ShellError::at("expected a parameter name", span));
                }
                if !optional && sig.params.iter().any(|p| p.optional) {
                    return Err(ShellError::at("a required parameter can't come after an optional one", span));
                }
                sig.params.push(Param { name: pname.to_string(), shape: shape.unwrap_or(Shape::Any), optional, default, desc: String::new() });
            }
        }
        // Known before the body, so it can call itself.
        self.defs.push(sig.clone());
        let body = self.braced_block()?;
        Ok(Stmt::Def(Rc::new(Def { sig, body, span: start.to(self.prev_span()) })))
    }

    // --- Pipelines and calls ------------------------------------------

    fn pipeline(&mut self) -> Result<Pipeline, ShellError> {
        let mut elements = Vec::new();
        loop {
            elements.push(self.element()?);
            if self.peek().tok != Tok::Pipe {
                break;
            }
            self.next();
            self.skip_newlines();
        }
        Ok(Pipeline { elements })
    }

    fn element(&mut self) -> Result<Element, ShellError> {
        let t = self.peek().clone();
        match &t.tok {
            Tok::Word(w) if !starts_expression(w) => Ok(Element::Call(self.call()?)),
            Tok::Pipe | Tok::Eof | Tok::Newline | Tok::Semi | Tok::RParen | Tok::RBrace => {
                Err(ShellError::at("expected a command", t.span))
            }
            _ => {
                let e = self.expr(Mode::Normal, 0)?;
                match self.peek().tok {
                    Tok::Pipe | Tok::Newline | Tok::Semi | Tok::Eof | Tok::RParen | Tok::RBrace => Ok(Element::Expr(e)),
                    _ => Err(self.unexpected()),
                }
            }
        }
    }

    fn at_call_end(&self) -> bool {
        matches!(self.peek().tok, Tok::Pipe | Tok::Semi | Tok::Newline | Tok::Eof | Tok::RParen | Tok::RBrace | Tok::RBracket)
    }

    fn call(&mut self) -> Result<CallAst, ShellError> {
        let head_tok = self.next();
        let first = match &head_tok.tok {
            Tok::Word(w) => w.clone(),
            _ => return Err(ShellError::at("expected a command", head_tok.span)),
        };
        let mut head = head_tok.span;
        let mut name = first.clone();
        // Two-word commands: `str upcase`, `math sum`.
        if let Tok::Word(second) = &self.peek().tok {
            let mut two = first.clone();
            two.push(' ');
            two.push_str(second);
            if self.signature(&two).is_some() {
                name = two;
                head = head.to(self.next().span);
            }
        }
        let sig = match self.signature(&name) {
            Some(s) => s.clone(),
            None => return self.unknown_command(&first, head),
        };
        let mut call = CallAst { name, head, args: Vec::new(), raw: String::new(), span: head };
        if sig.raw {
            let start = self.span();
            let mut end = None;
            while !matches!(self.peek().tok, Tok::Pipe | Tok::Semi | Tok::Newline | Tok::Eof | Tok::RParen | Tok::RBrace) {
                end = Some(self.next().span);
            }
            if let Some(end) = end {
                let s = start.to(end);
                call.raw = self.text(s).to_string();
                call.span = head.to(s);
            }
            return Ok(call);
        }
        let mut npos = 0;
        while !self.at_call_end() {
            let t = self.peek().clone();
            if let Tok::Word(w) = &t.tok {
                if let Some(long) = w.strip_prefix("--").filter(|l| !l.is_empty()) {
                    self.next();
                    let (long, inline) = match long.split_once('=') {
                        Some((l, v)) => (l, Some(v)),
                        None => (long, None),
                    };
                    let flag = match sig.flag_by_long(long) {
                        Some(f) => f.clone(),
                        None => return Err(self.unknown_flag(&sig, w, t.span)),
                    };
                    let value = self.flag_value(&flag, inline, t.span)?;
                    call.args.push(Arg::Flag { name: flag.long.clone(), value, span: t.span });
                    continue;
                }
                let short = w.len() >= 2 && w.starts_with('-') && !w[1..].starts_with(|c: char| c.is_ascii_digit() || c == '$' || c == '.');
                if short {
                    self.next();
                    let letters: Vec<char> = w[1..].chars().collect();
                    for (k, c) in letters.iter().enumerate() {
                        let flag = match sig.flag_by_short(*c) {
                            Some(f) => f.clone(),
                            None => return Err(self.unknown_flag(&sig, w, t.span)),
                        };
                        if flag.arg.is_some() && k + 1 != letters.len() {
                            return Err(ShellError::at("a flag that takes a value has to come last", t.span));
                        }
                        let value = self.flag_value(&flag, None, t.span)?;
                        call.args.push(Arg::Flag { name: flag.long.clone(), value, span: t.span });
                    }
                    continue;
                }
            }
            let shape = match sig.shape_of(npos) {
                Some(s) => s,
                None => {
                    let mut m = String::from(&call.name);
                    m.push_str(" doesn't take this many arguments");
                    return Err(ShellError::at(m, t.span).hint(sig.usage()));
                }
            };
            let e = if shape == Shape::Condition { self.condition()? } else { self.arg(shape)? };
            call.args.push(Arg::Positional(e));
            npos += 1;
        }
        let required = sig.params.iter().filter(|p| !p.optional).count();
        if npos < required {
            let missing = &sig.params[npos];
            let mut m = String::from(&call.name);
            m.push_str(" needs <");
            m.push_str(&missing.name);
            m.push('>');
            return Err(ShellError::at(m, head).hint(sig.usage()));
        }
        call.span = head.to(self.prev_span());
        Ok(call)
    }

    fn flag_value(&mut self, flag: &Flag, inline: Option<&str>, span: Span) -> Result<Option<Expr>, ShellError> {
        let shape = match flag.arg {
            None => {
                if inline.is_some() {
                    return Err(ShellError::at("this flag doesn't take a value", span));
                }
                return Ok(None);
            }
            Some(s) => s,
        };
        if let Some(text) = inline {
            let v = if shape.wants_text() { Value::str(text) } else { parse_literal(text).unwrap_or(Value::str(text)) };
            return Ok(Some(Expr { kind: ExprKind::Lit(v), span }));
        }
        if self.at_call_end() {
            let mut m = String::from("--");
            m.push_str(&flag.long);
            m.push_str(" needs a value");
            return Err(ShellError::at(m, span));
        }
        Ok(Some(self.arg(shape)?))
    }

    fn unknown_flag(&self, sig: &Signature, word: &str, span: Span) -> ShellError {
        let mut m = String::from(&sig.name);
        m.push_str(" has no flag ");
        m.push_str(word);
        let mut e = ShellError::at(m, span);
        if sig.flags.is_empty() {
            e = e.hint("it doesn't take any flags");
        } else {
            let mut h = String::from("flags:");
            for f in &sig.flags {
                h.push_str(" --");
                h.push_str(&f.long);
            }
            e = e.hint(h);
        }
        e
    }

    fn unknown_command(&mut self, word: &str, head: Span) -> Result<CallAst, ShellError> {
        if looks_like_program(word) {
            // `./hello.exe args` is `run ./hello.exe args`; `x.ks` is
            // `source x.ks`.
            let is_script = word.to_ascii_lowercase().ends_with(".ks");
            let runner = if is_script { "source" } else { "run" };
            if self.signature(runner).is_some() {
                let mut end = head;
                while !matches!(self.peek().tok, Tok::Pipe | Tok::Semi | Tok::Newline | Tok::Eof | Tok::RParen | Tok::RBrace) {
                    end = self.next().span;
                }
                let raw = self.text(head.to(end)).to_string();
                let mut args = Vec::new();
                if is_script {
                    args.push(Arg::Positional(Expr { kind: ExprKind::Lit(Value::str(word)), span: head }));
                }
                return Ok(CallAst { name: runner.to_string(), head, args, raw, span: head.to(end) });
            }
        }
        let mut m = String::from("unknown command `");
        m.push_str(word);
        m.push('`');
        let mut e = ShellError::at(m, head);
        // `str` alone: list what it can be followed by.
        let mut prefix = String::from(word);
        prefix.push(' ');
        let subs: Vec<String> = self.engine.command_names().filter(|n| n.starts_with(&prefix)).collect();
        if !subs.is_empty() {
            let mut h = String::from("try: ");
            for (i, s) in subs.iter().enumerate() {
                if i > 0 {
                    h.push_str(", ");
                }
                h.push_str(s);
            }
            return Err(e.hint(h));
        }
        let mut best: Option<(usize, String)> = None;
        for n in self.engine.command_names().chain(self.defs.iter().map(|d| d.name.clone())) {
            let d = distance(word, &n);
            if d <= 2 && best.as_ref().is_none_or(|(bd, _)| d < *bd) {
                best = Some((d, n));
            }
        }
        e = match best {
            Some((_, n)) => {
                let mut h = String::from("did you mean `");
                h.push_str(&n);
                h.push_str("`?");
                e.hint(h)
            }
            None => e.hint("`help` lists every command"),
        };
        Err(e)
    }

    /// A row condition: the rest of the step, as an expression whose bare
    /// names are columns. Becomes a closure over the row.
    fn condition(&mut self) -> Result<Expr, ShellError> {
        if self.peek().tok == Tok::LBrace {
            return self.arg(Shape::Closure);
        }
        if let Tok::Word(w) = &self.peek().tok {
            // `filter $f` where $f holds a closure, if nothing follows.
            if w.starts_with('$') {
                if matches!(self.peek_at(1).tok, Tok::Pipe | Tok::Semi | Tok::Newline | Tok::Eof | Tok::RParen | Tok::RBrace) {
                    return self.arg(Shape::Any);
                }
            }
        }
        self.uses.push(Vec::new());
        let e = self.expr(Mode::Row, 0);
        let mut uses = self.uses.pop().unwrap_or_default();
        let e = e?;
        let span = e.span;
        if !uses.iter().any(|u| u == "in") {
            uses.push("in".to_string());
        }
        let body = Block { stmts: alloc::vec![Stmt::Pipeline(Pipeline { elements: alloc::vec![Element::Expr(e)] })] };
        Ok(Expr { kind: ExprKind::Closure(Rc::new(ClosureAst { params: Vec::new(), body: Rc::new(body), uses, span })), span })
    }

    // --- Expressions --------------------------------------------------

    /// One argument: a word, string, variable, `( )`, `[ ]` or `{ }`.
    fn arg(&mut self, shape: Shape) -> Result<Expr, ShellError> {
        let t = self.peek().clone();
        if let Tok::Word(w) = &t.tok {
            if !w.starts_with('$') && !w.starts_with("-$") {
                self.next();
                let v = if shape.wants_text() { Value::str(w) } else { parse_literal(w).unwrap_or_else(|| Value::str(w)) };
                return Ok(Expr { kind: ExprKind::Lit(v), span: t.span });
            }
        }
        if shape == Shape::Closure && t.tok == Tok::LBrace {
            return self.brace(true);
        }
        self.atom(Mode::Normal)
    }

    fn expr(&mut self, mode: Mode, min_prec: u8) -> Result<Expr, ShellError> {
        self.enter()?;
        let mut lhs = if self.is_word("not") {
            let s = self.next().span;
            let inner = self.expr(mode, 3)?;
            let span = s.to(inner.span);
            Expr { kind: ExprKind::Not(Rc::new(inner)), span }
        } else {
            self.atom(mode)?
        };
        loop {
            let op = match &self.peek().tok {
                Tok::Word(w) => match Op::from_word(w) {
                    Some(op) if op.precedence() > min_prec => op,
                    _ => break,
                },
                _ => break,
            };
            let op_span = self.next().span;
            self.skip_newlines_in_parens();
            let mut rhs = self.expr(mode, op.precedence())?;
            // In a row condition, a bare name right of a comparison is
            // text: `type == file`.
            if mode == Mode::Row && op.is_comparison() {
                if let ExprKind::Column(_) = &rhs.kind {
                    let word = self.text(rhs.span).to_string();
                    rhs = Expr { kind: ExprKind::Lit(Value::String(word)), span: rhs.span };
                }
            }
            let span = lhs.span.to(rhs.span);
            lhs = Expr { kind: ExprKind::Binary(Rc::new(lhs), op, op_span, Rc::new(rhs)), span };
        }
        self.leave();
        Ok(lhs)
    }

    fn skip_newlines_in_parens(&mut self) {
        // Inside ( ) a long expression may continue on the next line.
        if self.depth > 1 {
            self.skip_newlines();
        }
    }

    fn atom(&mut self, mode: Mode) -> Result<Expr, ShellError> {
        let t = self.peek().clone();
        let span = t.span;
        match t.tok {
            Tok::Word(w) => {
                self.next();
                if let Some(var) = w.strip_prefix("-$") {
                    let inner = self.var_expr(var, span)?;
                    return Ok(Expr { kind: ExprKind::Neg(Rc::new(inner)), span });
                }
                if let Some(var) = w.strip_prefix('$') {
                    return self.var_expr(var, span);
                }
                if Op::from_word(&w).is_some() && w != "in" {
                    let mut m = String::from("expected a value before `");
                    m.push_str(&w);
                    m.push('`');
                    return Err(ShellError::at(m, span));
                }
                if let Some(v) = parse_literal(&w) {
                    return Ok(Expr { kind: ExprKind::Lit(v), span });
                }
                if mode == Mode::Row {
                    self.note_use("in");
                    let path = w.split('.').map(|s| s.to_string()).collect();
                    return Ok(Expr { kind: ExprKind::Column(path), span });
                }
                Ok(Expr { kind: ExprKind::Lit(Value::String(w)), span })
            }
            Tok::DStr(raw) => {
                self.next();
                self.interpolate(&raw, span)
            }
            Tok::SStr(s) => {
                self.next();
                Ok(Expr { kind: ExprKind::Lit(Value::String(s)), span })
            }
            Tok::LParen => {
                self.next();
                let block = self.block()?;
                let end = self.expect(Tok::RParen, "`)`")?;
                let mut members = Vec::new();
                // `(ls | first).name`
                while let Tok::Word(w) = &self.peek().tok {
                    if self.peek().spaced || !w.starts_with('.') {
                        break;
                    }
                    let w = w.clone();
                    self.next();
                    for m in w[1..].split('.') {
                        if m.is_empty() {
                            return Err(ShellError::at("expected a field name after `.`", self.prev_span()));
                        }
                        members.push(m.to_string());
                    }
                }
                Ok(Expr { kind: ExprKind::Sub(Rc::new(block), members), span: span.to(end).to(self.prev_span()) })
            }
            Tok::LBracket => self.list(),
            Tok::LBrace => self.brace(false),
            _ => {
                let mut m = String::from("expected a value, found ");
                m.push_str(&describe(&t.tok));
                Err(ShellError::at(m, span))
            }
        }
    }

    /// `name.field.0` (after the `$`).
    fn var_expr(&mut self, text: &str, span: Span) -> Result<Expr, ShellError> {
        let mut parts = text.split('.');
        let name = parts.next().unwrap_or("");
        if !is_ident(name) {
            return Err(ShellError::at("expected a variable name after `$`", span));
        }
        let mut members = Vec::new();
        for m in parts {
            if m.is_empty() {
                return Err(ShellError::at("expected a field name after `.`", span));
            }
            members.push(m.to_string());
        }
        self.note_use(name);
        Ok(Expr { kind: ExprKind::Var(name.to_string(), members), span })
    }

    fn list(&mut self) -> Result<Expr, ShellError> {
        let start = self.next().span;
        let mut items = Vec::new();
        loop {
            while matches!(self.peek().tok, Tok::Newline | Tok::Comma) {
                self.next();
            }
            if self.peek().tok == Tok::RBracket {
                break;
            }
            if self.peek().tok == Tok::Eof {
                return Err(ShellError::at("this list has no closing `]`", start));
            }
            items.push(self.expr(Mode::Normal, 0)?);
        }
        let end = self.next().span;
        Ok(Expr { kind: ExprKind::List(items), span: start.to(end) })
    }

    /// `{ ... }`: a record (`{name: value}`) or a closure (`{|x| ...}` or
    /// a plain block).
    fn brace(&mut self, want_closure: bool) -> Result<Expr, ShellError> {
        let start = self.span();
        // Look past `{` and any newlines.
        let mut k = 1;
        while self.peek_at(k).tok == Tok::Newline {
            k += 1;
        }
        let first = self.peek_at(k).tok.clone();
        let second = self.peek_at(k + 1).tok.clone();
        let is_record = !want_closure
            && match &first {
                Tok::RBrace => true,
                Tok::Word(w) if !w.starts_with('$') => {
                    (w.len() > 1 && w.ends_with(':')) || matches!(&second, Tok::Word(x) if x.starts_with(':')) || w.split_once(':').is_some_and(|(a, _)| is_ident(a))
                }
                Tok::DStr(_) | Tok::SStr(_) => matches!(&second, Tok::Word(x) if x.starts_with(':')),
                _ => false,
            };
        if is_record {
            return self.record();
        }
        self.next(); // `{`
        self.skip_newlines();
        let mut params = Vec::new();
        if self.peek().tok == Tok::Pipe {
            self.next();
            loop {
                let t = self.next();
                match &t.tok {
                    Tok::Pipe => break,
                    Tok::Comma => {}
                    Tok::Word(w) if is_ident(w.strip_prefix('$').unwrap_or(w)) => params.push(w.strip_prefix('$').unwrap_or(w).to_string()),
                    _ => return Err(ShellError::at("expected a parameter name or `|`", t.span)),
                }
            }
        }
        self.uses.push(Vec::new());
        let body = self.block();
        let uses = self.uses.pop().unwrap_or_default();
        let body = body?;
        let end = self.expect(Tok::RBrace, "`}`")?;
        let uses = uses.into_iter().filter(|u| !params.contains(u)).collect();
        let span = start.to(end);
        Ok(Expr { kind: ExprKind::Closure(Rc::new(ClosureAst { params, body: Rc::new(body), uses, span })), span })
    }

    fn record(&mut self) -> Result<Expr, ShellError> {
        let start = self.next().span; // `{`
        let mut fields: Vec<(String, Expr)> = Vec::new();
        loop {
            while matches!(self.peek().tok, Tok::Newline | Tok::Comma) {
                self.next();
            }
            if self.peek().tok == Tok::RBrace {
                break;
            }
            let t = self.next();
            let (key, glued): (String, Option<String>) = match &t.tok {
                Tok::Word(w) => match w.split_once(':') {
                    Some((k, v)) => (k.to_string(), Some(v.to_string())),
                    None => (w.clone(), None),
                },
                Tok::DStr(s) | Tok::SStr(s) => (s.clone(), None),
                Tok::Eof => return Err(ShellError::at("this record has no closing `}`", start)),
                _ => return Err(ShellError::at("expected a field name", t.span)),
            };
            let value_text = match glued {
                Some(v) => Some(v),
                None => {
                    // The `:` is the start of the next word.
                    let c = self.next();
                    match &c.tok {
                        Tok::Word(w) if w.starts_with(':') => Some(w[1..].to_string()),
                        _ => return Err(ShellError::at("expected `:` after the field name", c.span)),
                    }
                }
            };
            let value = match value_text {
                // `name:value` in one word.
                Some(v) if !v.is_empty() => {
                    let span = self.prev_span();
                    if let Some(var) = v.strip_prefix('$') {
                        self.var_expr(var, span)?
                    } else {
                        Expr { kind: ExprKind::Lit(parse_literal(&v).unwrap_or(Value::String(v))), span }
                    }
                }
                _ => self.expr(Mode::Normal, 0)?,
            };
            if fields.iter().any(|(k, _)| *k == key) {
                return Err(ShellError::at("this field is already in the record", t.span));
            }
            fields.push((key, value));
        }
        let end = self.next().span;
        Ok(Expr { kind: ExprKind::Record(fields), span: start.to(end) })
    }

    /// `"text $var (expr)"` -> text and expressions, joined at run time.
    fn interpolate(&mut self, raw: &str, span: Span) -> Result<Expr, ShellError> {
        let base = span.start as usize + 1;
        let b = raw.as_bytes();
        let mut parts = Vec::new();
        let mut buf = String::new();
        let mut i = 0;
        while i < b.len() {
            let c = b[i];
            if c == b'\\' && i + 1 < b.len() {
                let e = b[i + 1];
                match e {
                    b'n' => buf.push('\n'),
                    b't' => buf.push('\t'),
                    b'r' => buf.push('\r'),
                    b'0' => buf.push('\0'),
                    b'e' => buf.push('\u{1b}'),
                    b'"' | b'\\' | b'$' | b'(' | b')' => buf.push(e as char),
                    _ => {
                        buf.push('\\');
                        i += 1;
                        continue;
                    }
                }
                i += 2;
                continue;
            }
            if c == b'$' && b.get(i + 1).is_some_and(|n| n.is_ascii_alphabetic() || *n == b'_') {
                let mut j = i + 1;
                while j < b.len() && (b[j].is_ascii_alphanumeric() || b[j] == b'_' || b[j] == b'-' || (b[j] == b'.' && b.get(j + 1).is_some_and(|n| n.is_ascii_alphanumeric() || *n == b'_'))) {
                    j += 1;
                }
                if !buf.is_empty() {
                    parts.push(Part::Text(core::mem::take(&mut buf)));
                }
                let vspan = Span::new(self.src_id, base + i, base + j);
                parts.push(Part::Expr(self.var_expr(&raw[i + 1..j], vspan)?));
                i = j;
                continue;
            }
            if c == b'(' {
                // Find the matching `)`, skipping nested strings.
                let mut depth = 0;
                let mut j = i;
                let mut quote: Option<u8> = None;
                while j < b.len() {
                    match (quote, b[j]) {
                        (Some(q), x) if x == q => quote = None,
                        (Some(_), b'\\') => j += 1,
                        (Some(_), _) => {}
                        (None, b'\'') => quote = Some(b'\''),
                        (None, b'(') => depth += 1,
                        (None, b')') => {
                            depth -= 1;
                            if depth == 0 {
                                break;
                            }
                        }
                        _ => {}
                    }
                    j += 1;
                }
                if j >= b.len() {
                    return Err(ShellError::at("this `(` has no closing `)`", Span::new(self.src_id, base + i, base + i + 1)));
                }
                if !buf.is_empty() {
                    parts.push(Part::Text(core::mem::take(&mut buf)));
                }
                let inner = &raw[i + 1..j];
                let toks = lex(inner, self.src_id, base + i + 1)?;
                let saved_toks = core::mem::replace(&mut self.toks, toks);
                let saved_pos = core::mem::replace(&mut self.pos, 0);
                let block = self.block();
                let leftover = self.peek().tok != Tok::Eof;
                let err = if leftover { Some(self.unexpected()) } else { None };
                self.toks = saved_toks;
                self.pos = saved_pos;
                let block = block?;
                if let Some(e) = err {
                    return Err(e);
                }
                let espan = Span::new(self.src_id, base + i, base + j + 1);
                parts.push(Part::Expr(Expr { kind: ExprKind::Sub(Rc::new(block), Vec::new()), span: espan }));
                i = j + 1;
                continue;
            }
            // A whole UTF-8 character.
            let len = match c {
                0x00..=0x7f => 1,
                0xc0..=0xdf => 2,
                0xe0..=0xef => 3,
                _ => 4,
            };
            let end = (i + len).min(b.len());
            buf.push_str(raw.get(i..end).unwrap_or("\u{fffd}"));
            i = end;
        }
        if parts.is_empty() {
            return Ok(Expr { kind: ExprKind::Lit(Value::String(buf)), span });
        }
        if !buf.is_empty() {
            parts.push(Part::Text(buf));
        }
        Ok(Expr { kind: ExprKind::Interp(parts), span })
    }
}
