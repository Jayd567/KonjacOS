//! Commands on lists and tables: filter, sort-by, select, get, first,
//! each, math, ...

use alloc::rc::Rc;
use alloc::string::String;
use alloc::vec::Vec;

use super::{items, on_item};
use crate::error::ShellError;
use crate::eval::{Call, Ctx, Engine, Runner};
use crate::sig::{Shape, Signature};
use crate::value::{self, binary, equals, no_column, sort_cmp, Closure, Record, Value};
use crate::ast::Op;

pub fn register(e: &mut Engine) {
    let t = "tables";
    e.register(
        Signature::build("filter", t, "keep the rows where a condition is true: filter size > 1MB").required("condition", Shape::Condition, "a condition on each row, or a closure"),
        Runner::Native(filter),
    );
    e.register(
        Signature::build("sort-by", t, "sort a table by one or more columns").rest("column", Shape::String, "the columns to sort by, most important first").switch("reverse", Some('r'), "largest first"),
        Runner::Native(sort_by),
    );
    e.register(Signature::build("sort", t, "sort a list").switch("reverse", Some('r'), "largest first"), Runner::Native(sort_by));
    e.register(Signature::build("select", t, "keep only these columns").rest("column", Shape::String, "the columns to keep"), Runner::Native(select));
    e.register(Signature::build("reject", t, "drop these columns").rest("column", Shape::String, "the columns to drop"), Runner::Native(reject));
    e.register(
        Signature::build("get", t, "a column of a table, a field of a record, or an item of a list").required("path", Shape::String, "e.g. name, 0, or info.size"),
        Runner::Native(get),
    );
    e.register(Signature::build("first", t, "the first item, or the first n").optional("n", Shape::Int, "how many"), Runner::Native(first));
    e.register(Signature::build("last", t, "the last item, or the last n").optional("n", Shape::Int, "how many"), Runner::Native(last));
    e.register(Signature::build("skip", t, "everything but the first n items").optional("n", Shape::Int, "how many to skip (1 if not given)"), Runner::Native(skip));
    e.register(Signature::build("length", t, "how many items there are"), Runner::Native(length));
    e.register(Signature::build("reverse", t, "the items in the opposite order"), Runner::Native(reverse));
    e.register(Signature::build("uniq", t, "drop repeated items"), Runner::Native(uniq));
    e.register(
        Signature::build("each", t, "run a closure on every item and collect the results").required("closure", Shape::Closure, "what to do with each item ($in, or its parameter)"),
        Runner::Native(each),
    );
    e.register(Signature::build("group-by", t, "split a table into groups by a column").required("column", Shape::String, "the column to group by"), Runner::Native(group_by));
    e.register(Signature::build("enumerate", t, "number the items: a table of index and item"), Runner::Native(enumerate));
    e.register(
        Signature::build("insert", t, "add a column, from a value or a closure on each row").required("column", Shape::String, "the new column").required("value", Shape::Any, "its value, or a closure that makes it"),
        Runner::Native(insert),
    );
    e.register(
        Signature::build("update", t, "change a column, to a value or with a closure on each row").required("column", Shape::String, "the column").required("value", Shape::Any, "the new value, or a closure that makes it"),
        Runner::Native(update),
    );
    e.register(Signature::build("columns", t, "the column names of a table or record"), Runner::Native(columns));
    e.register(Signature::build("is-empty", t, "whether the input has nothing in it"), Runner::Native(is_empty));
    let m = "maths";
    e.register(Signature::build("math sum", m, "add up the items"), Runner::Native(math_sum));
    e.register(Signature::build("math avg", m, "the average of the items"), Runner::Native(math_avg));
    e.register(Signature::build("math min", m, "the smallest item"), Runner::Native(math_min));
    e.register(Signature::build("math max", m, "the largest item"), Runner::Native(math_max));
}

fn closure_arg(call: &Call, i: usize) -> Result<Rc<Closure>, ShellError> {
    match call.pos(i) {
        Some(Value::Closure(c)) => Ok(c.clone()),
        _ => Err(ShellError::at("expected a closure", call.pos_span(i))),
    }
}

fn filter(ctx: &mut Ctx, call: &Call, input: Value) -> Result<Value, ShellError> {
    let cond = closure_arg(call, 0)?;
    let list = items(input, "filter")?;
    let n = list.len();
    let mut out = Vec::new();
    for (i, item) in list.into_iter().enumerate() {
        let r = ctx.call_closure(&cond, alloc::vec![item.clone()], item.clone()).map_err(|e| on_item(e, i, n))?;
        if r.truthy("the condition").map_err(|e| on_item(e, i, n).or_at(call.pos_span(0)))? {
            out.push(item);
        }
    }
    Ok(Value::List(out))
}

fn sort_by(_: &mut Ctx, call: &Call, input: Value) -> Result<Value, ShellError> {
    let list = items(input, &call.name)?;
    let cols: Vec<&str> = call.positional.iter().filter_map(|(v, _)| v.as_str()).collect();
    // Work out each row's sort key first, so a missing column is an error
    // rather than something the comparison has to cope with.
    let mut keyed: Vec<(Vec<Value>, Value)> = Vec::with_capacity(list.len());
    let n = list.len();
    for (i, item) in list.into_iter().enumerate() {
        let key = if cols.is_empty() {
            alloc::vec![item.clone()]
        } else {
            let mut k = Vec::with_capacity(cols.len());
            for c in &cols {
                k.push(item.member(c).map_err(|e| on_item(e, i, n))?);
            }
            k
        };
        keyed.push((key, item));
    }
    keyed.sort_by(|a, b| {
        for (x, y) in a.0.iter().zip(b.0.iter()) {
            let o = sort_cmp(x, y);
            if o != core::cmp::Ordering::Equal {
                return o;
            }
        }
        core::cmp::Ordering::Equal
    });
    let mut out: Vec<Value> = keyed.into_iter().map(|(_, v)| v).collect();
    if call.has("reverse") {
        out.reverse();
    }
    Ok(Value::List(out))
}

fn pick(r: &Record, cols: &[&str], keep: bool) -> Result<Record, ShellError> {
    if keep {
        let mut out = Record::new();
        for c in cols {
            match r.get(c) {
                Some(v) => out.insert(c, v.clone()),
                None => return Err(no_column(c, r)),
            }
        }
        Ok(out)
    } else {
        for c in cols {
            if r.get(c).is_none() {
                return Err(no_column(c, r));
            }
        }
        Ok(Record { cols: r.cols.iter().filter(|(k, _)| !cols.contains(&k.as_str())).cloned().collect() })
    }
}

fn select_or_reject(call: &Call, input: Value, keep: bool) -> Result<Value, ShellError> {
    let cols: Vec<&str> = call.positional.iter().filter_map(|(v, _)| v.as_str()).collect();
    match input {
        Value::Record(r) => Ok(Value::Record(pick(&r, &cols, keep)?)),
        Value::List(l) => {
            let n = l.len();
            let mut out = Vec::with_capacity(n);
            for (i, row) in l.iter().enumerate() {
                match row {
                    Value::Record(r) => out.push(Value::Record(pick(r, &cols, keep).map_err(|e| on_item(e, i, n))?)),
                    other => return Err(on_item(ShellError::new(alloc::format!("expected a record, found {}", other.a_type())), i, n)),
                }
            }
            Ok(Value::List(out))
        }
        other => Err(ShellError::new(alloc::format!("needs a table or a record, not {}", other.a_type()))),
    }
}

fn select(_: &mut Ctx, call: &Call, input: Value) -> Result<Value, ShellError> {
    select_or_reject(call, input, true)
}

fn reject(_: &mut Ctx, call: &Call, input: Value) -> Result<Value, ShellError> {
    select_or_reject(call, input, false)
}

fn get(_: &mut Ctx, call: &Call, input: Value) -> Result<Value, ShellError> {
    let path = call.str_at(0).unwrap_or("");
    let mut v = input;
    for m in path.split('.') {
        v = v.member(m).map_err(|e| e.or_at(call.pos_span(0)))?;
    }
    Ok(v)
}

fn count_arg(call: &Call, default: i64) -> Result<usize, ShellError> {
    let n = call.int_at(0).unwrap_or(default);
    if n < 0 {
        return Err(ShellError::at("can't be negative", call.pos_span(0)));
    }
    Ok(n as usize)
}

fn first(_: &mut Ctx, call: &Call, input: Value) -> Result<Value, ShellError> {
    let list = items(input, "first")?;
    if call.pos(0).is_none() {
        return list.into_iter().next().ok_or_else(|| ShellError::new("the list is empty"));
    }
    let n = count_arg(call, 1)?;
    Ok(Value::List(list.into_iter().take(n).collect()))
}

fn last(_: &mut Ctx, call: &Call, input: Value) -> Result<Value, ShellError> {
    let mut list = items(input, "last")?;
    if call.pos(0).is_none() {
        return list.pop().ok_or_else(|| ShellError::new("the list is empty"));
    }
    let n = count_arg(call, 1)?;
    let start = list.len().saturating_sub(n);
    Ok(Value::List(list.split_off(start)))
}

fn skip(_: &mut Ctx, call: &Call, input: Value) -> Result<Value, ShellError> {
    let list = items(input, "skip")?;
    let n = count_arg(call, 1)?;
    Ok(Value::List(list.into_iter().skip(n).collect()))
}

fn length(_: &mut Ctx, _: &Call, input: Value) -> Result<Value, ShellError> {
    Ok(Value::Int(match &input {
        Value::List(l) => l.len() as i64,
        Value::Record(r) => r.cols.len() as i64,
        Value::Binary(b) => b.len() as i64,
        Value::Nothing => 0,
        other => return Err(ShellError::new(alloc::format!("needs a list, not {}", other.a_type())).hint("for the length of a string, use `str length`")),
    }))
}

fn reverse(_: &mut Ctx, _: &Call, input: Value) -> Result<Value, ShellError> {
    let mut l = items(input, "reverse")?;
    l.reverse();
    Ok(Value::List(l))
}

fn uniq(_: &mut Ctx, _: &Call, input: Value) -> Result<Value, ShellError> {
    let l = items(input, "uniq")?;
    let mut out: Vec<Value> = Vec::new();
    for v in l {
        if !out.iter().any(|x| equals(x, &v)) {
            out.push(v);
        }
    }
    Ok(Value::List(out))
}

fn each(ctx: &mut Ctx, call: &Call, input: Value) -> Result<Value, ShellError> {
    let f = closure_arg(call, 0)?;
    // A single value is one item.
    let list = match input {
        Value::List(l) => l,
        Value::Nothing => Vec::new(),
        other => return ctx.call_closure(&f, alloc::vec![other.clone()], other),
    };
    let n = list.len();
    let mut out = Vec::with_capacity(n);
    for (i, item) in list.into_iter().enumerate() {
        let r = ctx.call_closure(&f, alloc::vec![item.clone()], item).map_err(|e| on_item(e, i, n))?;
        if !matches!(r, Value::Nothing) {
            out.push(r);
        }
    }
    Ok(Value::List(out))
}

fn group_by(_: &mut Ctx, call: &Call, input: Value) -> Result<Value, ShellError> {
    let col = call.str_at(0).unwrap_or("");
    let list = items(input, "group-by")?;
    let n = list.len();
    let mut groups = Record::new();
    for (i, item) in list.into_iter().enumerate() {
        let key = item.member(col).map_err(|e| on_item(e, i, n))?.to_text();
        match groups.cols.iter_mut().find(|(k, _)| *k == key) {
            Some((_, Value::List(l))) => l.push(item),
            _ => groups.insert(&key, Value::List(alloc::vec![item])),
        }
    }
    Ok(Value::Record(groups))
}

fn enumerate(_: &mut Ctx, _: &Call, input: Value) -> Result<Value, ShellError> {
    let list = items(input, "enumerate")?;
    Ok(Value::List(list.into_iter().enumerate().map(|(i, v)| Value::Record(Record::new().push("index", Value::Int(i as i64)).push("item", v))).collect()))
}

fn set_column(ctx: &mut Ctx, call: &Call, input: Value, inserting: bool) -> Result<Value, ShellError> {
    let col = String::from(call.str_at(0).unwrap_or(""));
    let value = call.pos(1).cloned().unwrap_or(Value::Nothing);
    let one = |ctx: &mut Ctx, mut r: Record| -> Result<Value, ShellError> {
        let exists = r.get(&col).is_some();
        if inserting && exists {
            return Err(ShellError::new(alloc::format!("there's already a column `{col}`")).hint("use `update` to change it"));
        }
        if !inserting && !exists {
            return Err(no_column(&col, &r));
        }
        let v = match &value {
            Value::Closure(c) => {
                let row = Value::Record(r.clone());
                ctx.call_closure(c, alloc::vec![row.clone()], row)?
            }
            v => v.clone(),
        };
        r.insert(&col, v);
        Ok(Value::Record(r))
    };
    match input {
        Value::Record(r) => one(ctx, r),
        Value::List(l) => {
            let n = l.len();
            let mut out = Vec::with_capacity(n);
            for (i, row) in l.into_iter().enumerate() {
                match row {
                    Value::Record(r) => out.push(one(ctx, r).map_err(|e| on_item(e, i, n))?),
                    other => return Err(on_item(ShellError::new(alloc::format!("expected a record, found {}", other.a_type())), i, n)),
                }
            }
            Ok(Value::List(out))
        }
        other => Err(ShellError::new(alloc::format!("needs a table or a record, not {}", other.a_type()))),
    }
}

fn insert(ctx: &mut Ctx, call: &Call, input: Value) -> Result<Value, ShellError> {
    set_column(ctx, call, input, true)
}

fn update(ctx: &mut Ctx, call: &Call, input: Value) -> Result<Value, ShellError> {
    set_column(ctx, call, input, false)
}

fn columns(_: &mut Ctx, _: &Call, input: Value) -> Result<Value, ShellError> {
    let mut cols: Vec<Value> = Vec::new();
    match &input {
        Value::Record(r) => cols.extend(r.cols.iter().map(|(k, _)| Value::str(k))),
        Value::List(l) => {
            for row in l {
                if let Value::Record(r) = row {
                    for (k, _) in &r.cols {
                        if !cols.iter().any(|c| c.as_str() == Some(k)) {
                            cols.push(Value::str(k));
                        }
                    }
                }
            }
        }
        other => return Err(ShellError::new(alloc::format!("needs a table or a record, not {}", other.a_type()))),
    }
    Ok(Value::List(cols))
}

fn is_empty(_: &mut Ctx, _: &Call, input: Value) -> Result<Value, ShellError> {
    Ok(Value::Bool(match &input {
        Value::Nothing => true,
        Value::List(l) => l.is_empty(),
        Value::Record(r) => r.cols.is_empty(),
        Value::String(s) => s.is_empty(),
        Value::Binary(b) => b.is_empty(),
        _ => false,
    }))
}

fn numbers(input: Value, what: &str) -> Result<Vec<Value>, ShellError> {
    let l = items(input, what)?;
    if l.is_empty() {
        return Err(ShellError::new("the list is empty"));
    }
    Ok(l)
}

fn math_sum(_: &mut Ctx, _: &Call, input: Value) -> Result<Value, ShellError> {
    let l = numbers(input, "math sum")?;
    let n = l.len();
    let mut it = l.into_iter();
    let mut acc = it.next().unwrap_or(Value::Int(0));
    for (i, v) in it.enumerate() {
        acc = binary(Op::Add, &acc, &v).map_err(|e| on_item(e, i + 1, n))?;
    }
    Ok(acc)
}

fn math_avg(ctx: &mut Ctx, call: &Call, input: Value) -> Result<Value, ShellError> {
    let n = match &input {
        Value::List(l) => l.len(),
        _ => 0,
    };
    let sum = math_sum(ctx, call, input)?;
    match sum {
        Value::Int(s) => Ok(Value::Float(s as f64 / n as f64)),
        v => binary(Op::Div, &v, &Value::Int(n as i64)),
    }
}

fn extreme(input: Value, what: &str, want: core::cmp::Ordering) -> Result<Value, ShellError> {
    let l = numbers(input, what)?;
    let n = l.len();
    let mut best: Option<Value> = None;
    for (i, v) in l.into_iter().enumerate() {
        if let Some(b) = &best {
            // Same rules as `<`: comparing a size with a string is an error.
            let less = binary(Op::Lt, &v, b).map_err(|e| on_item(e, i, n))?;
            let better = match less {
                Value::Bool(true) => want == core::cmp::Ordering::Less,
                _ => want == core::cmp::Ordering::Greater && !value::equals(&v, b) && sort_cmp(&v, b) == core::cmp::Ordering::Greater,
            };
            if better {
                best = Some(v);
            }
        } else {
            best = Some(v);
        }
    }
    Ok(best.unwrap_or(Value::Nothing))
}

fn math_min(_: &mut Ctx, _: &Call, input: Value) -> Result<Value, ShellError> {
    extreme(input, "math min", core::cmp::Ordering::Less)
}

fn math_max(_: &mut Ctx, _: &Call, input: Value) -> Result<Value, ShellError> {
    extreme(input, "math max", core::cmp::Ordering::Greater)
}
