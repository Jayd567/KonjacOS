//! The syntax tree the parser builds and the evaluator walks. Every node
//! keeps its span so an error can point at it.

use alloc::rc::Rc;
use alloc::string::String;
use alloc::vec::Vec;

use crate::error::Span;
use crate::sig::Signature;
use crate::value::Value;

#[derive(Debug, Default)]
pub struct Block {
    pub stmts: Vec<Stmt>,
}

#[derive(Debug)]
pub enum Stmt {
    Pipeline(Pipeline),
    Let { name: String, mutable: bool, value: Pipeline, span: Span },
    Assign { name: String, value: Pipeline, span: Span },
    Def(Rc<Def>),
    If { cond: Expr, then: Rc<Block>, els: Option<Rc<Block>> },
    For { var: String, iter: Expr, body: Rc<Block> },
    While { cond: Expr, body: Rc<Block> },
    Break(Span),
    Continue(Span),
    Return(Option<Pipeline>, Span),
}

#[derive(Debug)]
pub struct Pipeline {
    pub elements: Vec<Element>,
}

#[derive(Debug)]
pub enum Element {
    Call(CallAst),
    Expr(Expr),
}

#[derive(Debug)]
pub struct CallAst {
    pub name: String,
    pub head: Span,
    pub args: Vec<Arg>,
    /// For text commands: everything after the name, as typed.
    pub raw: String,
    pub span: Span,
}

#[derive(Debug)]
pub enum Arg {
    Positional(Expr),
    Flag { name: String, value: Option<Expr>, span: Span },
}

#[derive(Debug)]
pub struct Expr {
    pub kind: ExprKind,
    pub span: Span,
}

#[derive(Debug)]
pub enum ExprKind {
    Lit(Value),
    /// `"text $var (expr) text"`.
    Interp(Vec<Part>),
    /// `$name.field.0`.
    Var(String, Vec<String>),
    /// A bare name in a row condition: a column of the current row.
    Column(Vec<String>),
    /// `( ... )`, then any `.field` after it.
    Sub(Rc<Block>, Vec<String>),
    List(Vec<Expr>),
    Record(Vec<(String, Expr)>),
    Closure(Rc<ClosureAst>),
    Binary(Rc<Expr>, Op, Span, Rc<Expr>),
    Not(Rc<Expr>),
    Neg(Rc<Expr>),
}

#[derive(Debug)]
pub enum Part {
    Text(String),
    Expr(Expr),
}

#[derive(Debug)]
pub struct ClosureAst {
    pub params: Vec<String>,
    pub body: Rc<Block>,
    /// The variables the body uses that it doesn't define itself: what
    /// a closure copies when it's made.
    pub uses: Vec<String>,
    pub span: Span,
}

#[derive(Debug)]
pub struct Def {
    pub sig: Signature,
    pub body: Rc<Block>,
    pub span: Span,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Concat,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    Glob,
    In,
    And,
    Or,
}

impl Op {
    pub fn from_word(w: &str) -> Option<Op> {
        Some(match w {
            "+" => Op::Add,
            "-" => Op::Sub,
            "*" => Op::Mul,
            "/" => Op::Div,
            "mod" => Op::Mod,
            "++" => Op::Concat,
            "==" => Op::Eq,
            "!=" => Op::Ne,
            "<" => Op::Lt,
            "<=" => Op::Le,
            ">" => Op::Gt,
            ">=" => Op::Ge,
            "=~" => Op::Glob,
            "in" => Op::In,
            "and" => Op::And,
            "or" => Op::Or,
            _ => return None,
        })
    }

    /// Binding strength: higher binds tighter.
    pub fn precedence(self) -> u8 {
        match self {
            Op::Or => 1,
            Op::And => 2,
            Op::Eq | Op::Ne | Op::Lt | Op::Le | Op::Gt | Op::Ge | Op::Glob => 4,
            Op::In => 5,
            Op::Add | Op::Sub | Op::Concat => 6,
            Op::Mul | Op::Div | Op::Mod => 7,
        }
    }

    pub fn is_comparison(self) -> bool {
        matches!(self, Op::Eq | Op::Ne | Op::Lt | Op::Le | Op::Gt | Op::Ge | Op::Glob | Op::In)
    }
}
