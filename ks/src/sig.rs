//! Signatures: what arguments and flags a command takes. One signature
//! drives parsing (does `-r` take a value?), checking, `help`, and later
//! completion.

use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::fmt::Write;

use crate::value::Value;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shape {
    Any,
    Int,
    Float,
    /// An int or a float.
    Number,
    String,
    /// A string naming a file or folder.
    Path,
    Bool,
    Size,
    Duration,
    Date,
    List,
    Record,
    Table,
    Closure,
    /// `size > 1MB`: an expression where bare names are columns.
    Condition,
}

impl Shape {
    pub fn name(self) -> &'static str {
        match self {
            Shape::Any => "any",
            Shape::Int => "int",
            Shape::Float => "float",
            Shape::Number => "number",
            Shape::String => "string",
            Shape::Path => "path",
            Shape::Bool => "bool",
            Shape::Size => "size",
            Shape::Duration => "duration",
            Shape::Date => "date",
            Shape::List => "list",
            Shape::Record => "record",
            Shape::Table => "table",
            Shape::Closure => "closure",
            Shape::Condition => "condition",
        }
    }

    pub fn from_name(s: &str) -> Option<Shape> {
        Some(match s {
            "any" => Shape::Any,
            "int" => Shape::Int,
            "float" => Shape::Float,
            "number" => Shape::Number,
            "string" => Shape::String,
            "path" => Shape::Path,
            "bool" => Shape::Bool,
            "size" => Shape::Size,
            "duration" => Shape::Duration,
            "date" => Shape::Date,
            "list" => Shape::List,
            "record" => Shape::Record,
            "table" => Shape::Table,
            "closure" => Shape::Closure,
            _ => return None,
        })
    }

    /// Whether a bare word in this position stays text even if it looks
    /// like a number (a file called `2024` is still a file name).
    pub fn wants_text(self) -> bool {
        matches!(self, Shape::String | Shape::Path)
    }

    /// Whether `v` fits. Ints are accepted where floats are wanted, and
    /// any list where a table is.
    pub fn accepts(self, v: &Value) -> bool {
        match (self, v) {
            (Shape::Any, _) => true,
            (Shape::Int, Value::Int(_)) => true,
            (Shape::Float | Shape::Number, Value::Int(_) | Value::Float(_)) => true,
            (Shape::String | Shape::Path, Value::String(_)) => true,
            (Shape::Bool, Value::Bool(_)) => true,
            (Shape::Size, Value::Size(_)) => true,
            (Shape::Duration, Value::Duration(_)) => true,
            (Shape::Date, Value::Date(_)) => true,
            (Shape::List | Shape::Table, Value::List(_)) => true,
            (Shape::Record, Value::Record(_)) => true,
            (Shape::Closure | Shape::Condition, Value::Closure(_)) => true,
            _ => false,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Param {
    pub name: String,
    pub shape: Shape,
    pub optional: bool,
    pub default: Option<Value>,
    pub desc: String,
}

#[derive(Clone, Debug)]
pub struct Flag {
    pub long: String,
    pub short: Option<char>,
    /// `None` for a switch; the value's shape otherwise.
    pub arg: Option<Shape>,
    pub default: Option<Value>,
    pub desc: String,
}

#[derive(Clone, Debug)]
pub struct Signature {
    pub name: String,
    pub summary: String,
    pub params: Vec<Param>,
    /// `...rest`: any number of trailing arguments.
    pub rest: Option<Param>,
    pub flags: Vec<Flag>,
    /// Changes files: previewed, and (later) grouped into one commit.
    pub destructive: bool,
    /// Asks for the apex password first.
    pub apex: bool,
    /// A text command: gets everything after its name as one string.
    pub raw: bool,
    /// For `help`: files, tables, text, maths, system, ...
    pub category: &'static str,
}

impl Signature {
    pub fn build(name: &str, category: &'static str, summary: &str) -> Signature {
        Signature {
            name: name.to_string(),
            summary: summary.to_string(),
            params: Vec::new(),
            rest: None,
            flags: Vec::new(),
            destructive: false,
            apex: false,
            raw: false,
            category,
        }
    }

    pub fn required(mut self, name: &str, shape: Shape, desc: &str) -> Signature {
        self.params.push(Param { name: name.to_string(), shape, optional: false, default: None, desc: desc.to_string() });
        self
    }

    pub fn optional(mut self, name: &str, shape: Shape, desc: &str) -> Signature {
        self.params.push(Param { name: name.to_string(), shape, optional: true, default: None, desc: desc.to_string() });
        self
    }

    pub fn rest(mut self, name: &str, shape: Shape, desc: &str) -> Signature {
        self.rest = Some(Param { name: name.to_string(), shape, optional: true, default: None, desc: desc.to_string() });
        self
    }

    pub fn switch(mut self, long: &str, short: Option<char>, desc: &str) -> Signature {
        self.flags.push(Flag { long: long.to_string(), short, arg: None, default: None, desc: desc.to_string() });
        self
    }

    pub fn named(mut self, long: &str, short: Option<char>, shape: Shape, desc: &str) -> Signature {
        self.flags.push(Flag { long: long.to_string(), short, arg: Some(shape), default: None, desc: desc.to_string() });
        self
    }

    pub fn destructive(mut self) -> Signature {
        self.destructive = true;
        self
    }

    pub fn apex(mut self) -> Signature {
        self.apex = true;
        self
    }

    pub fn raw(mut self) -> Signature {
        self.raw = true;
        self
    }

    pub fn flag_by_long(&self, long: &str) -> Option<&Flag> {
        self.flags.iter().find(|f| f.long == long)
    }

    pub fn flag_by_short(&self, c: char) -> Option<&Flag> {
        self.flags.iter().find(|f| f.short == Some(c))
    }

    /// The shape of the `i`th positional argument.
    pub fn shape_of(&self, i: usize) -> Option<Shape> {
        match self.params.get(i) {
            Some(p) => Some(p.shape),
            None => self.rest.as_ref().map(|r| r.shape),
        }
    }

    /// `sort-by ...column [--reverse]`.
    pub fn usage(&self) -> String {
        let mut s = self.name.clone();
        if self.raw {
            s.push_str(" <text>");
            return s;
        }
        for p in &self.params {
            if p.optional {
                let _ = write!(s, " [{}]", p.name);
            } else {
                let _ = write!(s, " <{}>", p.name);
            }
        }
        if let Some(r) = &self.rest {
            let _ = write!(s, " ...{}", r.name);
        }
        for f in &self.flags {
            match f.arg {
                Some(shape) => {
                    let _ = write!(s, " [--{} <{}>]", f.long, shape.name());
                }
                None => {
                    let _ = write!(s, " [--{}]", f.long);
                }
            }
        }
        s
    }

    /// Everything `help <command>` shows.
    pub fn help(&self) -> String {
        let mut s = String::new();
        let _ = writeln!(s, "{}: {}", self.name, self.summary);
        let _ = writeln!(s);
        let _ = writeln!(s, "usage: {}", self.usage());
        let params: Vec<&Param> = self.params.iter().chain(self.rest.iter()).collect();
        if !params.is_empty() {
            let _ = writeln!(s);
            let _ = writeln!(s, "arguments:");
            for p in params {
                let mut left = String::new();
                let _ = write!(left, "{} ({})", p.name, p.shape.name());
                let _ = writeln!(s, "  {left:<22} {}", p.desc);
            }
        }
        if !self.flags.is_empty() {
            let _ = writeln!(s);
            let _ = writeln!(s, "flags:");
            for f in &self.flags {
                let mut left = String::new();
                match f.short {
                    Some(c) => {
                        let _ = write!(left, "-{c}, --{}", f.long);
                    }
                    None => {
                        let _ = write!(left, "    --{}", f.long);
                    }
                }
                if let Some(shape) = f.arg {
                    let _ = write!(left, " <{}>", shape.name());
                }
                let _ = writeln!(s, "  {left:<22} {}", f.desc);
            }
        }
        if self.apex || self.destructive {
            let _ = writeln!(s);
        }
        if self.destructive {
            let _ = writeln!(s, "changes files: shows what it will do and asks first");
        }
        if self.apex {
            let _ = writeln!(s, "asks for the apex password");
        }
        while s.ends_with('\n') {
            s.pop();
        }
        s
    }
}
