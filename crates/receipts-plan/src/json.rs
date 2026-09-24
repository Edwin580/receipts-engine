//! The plan's JSON form (`docs/engine/plan.md` §3). Parsing is strict:
//! unknown keys, missing keys and wrong types are errors, so a plan that
//! parses means exactly what it says.

use crate::expr::{ArithOp, CmpOp, Expr, TimeUnit};
use crate::plan::{AggFunc, Aggregate, NodeId, Op, Plan, PlanError, SortKey};
use crate::types::Literal;
use receipts_core::ContentHash;
use receipts_core::time::{format_naive_timestamp, parse_naive_timestamp};
use serde_json::{Map, Value, json};

pub const FORMAT: &str = "receipts-plan/1";

pub fn to_json(plan: &Plan) -> Value {
    let nodes: Vec<Value> = plan
        .nodes
        .iter()
        .map(|op| op_to_json(op, op.input().map(|i| json!(i.0))))
        .collect();
    json!({ "format": FORMAT, "nodes": nodes, "output": plan.output.0 })
}

/// `input` is what goes under the `"input"` key: the node index in a plan
/// document, or the input's hash when hashing (see `crate::hash`).
pub(crate) fn op_to_json(op: &Op, input: Option<Value>) -> Value {
    let mut m = Map::new();
    m.insert("op".into(), json!(op.name()));
    if let Some(input) = input {
        m.insert("input".into(), input);
    }
    match op {
        Op::Scan { snapshot } => {
            m.insert("snapshot".into(), json!(snapshot.to_hex()));
        }
        Op::Filter { predicate, .. } => {
            m.insert("predicate".into(), expr_to_json(predicate));
        }
        Op::Map { name, expr, .. } => {
            m.insert("name".into(), json!(name));
            m.insert("expr".into(), expr_to_json(expr));
        }
        Op::Project { columns, .. } => {
            m.insert("columns".into(), json!(columns));
        }
        Op::Aggregate {
            group_by,
            aggregates,
            ..
        } => {
            m.insert("group_by".into(), json!(group_by));
            let aggs: Vec<Value> = aggregates
                .iter()
                .map(|a| {
                    let mut o = Map::new();
                    o.insert("name".into(), json!(a.name));
                    o.insert("fn".into(), json!(a.func.name()));
                    if let Some(c) = &a.column {
                        o.insert("column".into(), json!(c));
                    }
                    Value::Object(o)
                })
                .collect();
            m.insert("aggregates".into(), Value::Array(aggs));
        }
        Op::Sort { keys, .. } => {
            let keys: Vec<Value> = keys
                .iter()
                .map(|k| {
                    json!({ "column": k.column, "order": if k.descending { "desc" } else { "asc" } })
                })
                .collect();
            m.insert("keys".into(), Value::Array(keys));
        }
        Op::Limit { count, .. } => {
            // A string: counts above 2^53 can't be canonical JSON numbers.
            m.insert("count".into(), json!(count.to_string()));
        }
    }
    Value::Object(m)
}

pub fn expr_to_json(e: &Expr) -> Value {
    let call = |name: &str, args: &[&Expr]| json!({ "call": name, "args": args.iter().map(|a| expr_to_json(a)).collect::<Vec<_>>() });
    match e {
        Expr::Column(c) => json!({ "column": c }),
        Expr::Literal(l) => json!({ "literal": literal_to_json(l) }),
        Expr::Compare(op, a, b) => call(op.name(), &[a, b]),
        Expr::Arith(op, a, b) => call(op.name(), &[a, b]),
        Expr::And(args) | Expr::Or(args) => {
            let refs: Vec<&Expr> = args.iter().collect();
            call(
                if matches!(e, Expr::And(_)) {
                    "and"
                } else {
                    "or"
                },
                &refs,
            )
        }
        Expr::Not(a) => call("not", &[a]),
        Expr::IsNull(a) => call("is_null", &[a]),
        Expr::Lat(a) => call("lat", &[a]),
        Expr::Lon(a) => call("lon", &[a]),
        Expr::InList(a, values) => {
            let mut v = call("in", &[a]);
            v["values"] = values.iter().map(literal_to_json).collect();
            v
        }
        Expr::DateTrunc(unit, a) => {
            let mut v = call("date_trunc", &[a]);
            v["unit"] = json!(unit.name());
            v
        }
    }
}

/// Literals carry their type, so `1` (integer) and `1.0` (decimal) stay
/// distinct even though canonical JSON writes both as `1`. Integers are
/// strings because canonical JSON numbers stop at 2^53.
pub fn literal_to_json(l: &Literal) -> Value {
    match l {
        Literal::Null => Value::Null,
        Literal::Bool(b) => json!({ "bool": b }),
        Literal::I64(i) => json!({ "i64": i.to_string() }),
        Literal::F64(x) => json!({ "f64": x }),
        Literal::Str(s) => json!({ "str": s }),
        Literal::Timestamp(t) => json!({ "timestamp": format_naive_timestamp(*t) }),
    }
}

pub fn from_json(v: &Value) -> Result<Plan, PlanError> {
    let top = Obj::new(v, &["format", "nodes", "output"]).map_err(PlanError::plan)?;
    let format = top.str("format").map_err(PlanError::plan)?;
    if format != FORMAT {
        return Err(PlanError::plan(format!(
            "unsupported plan format \"{format}\""
        )));
    }
    let nodes = top
        .array("nodes")
        .map_err(PlanError::plan)?
        .iter()
        .enumerate()
        .map(|(i, n)| op_from_json(n).map_err(|m| PlanError::at(i, m)))
        .collect::<Result<Vec<_>, _>>()?;
    let output = top.u32("output").map_err(PlanError::plan)?;
    Ok(Plan {
        nodes,
        output: NodeId(output),
    })
}

fn op_from_json(v: &Value) -> Result<Op, String> {
    let op = v
        .get("op")
        .and_then(Value::as_str)
        .ok_or("each step needs an \"op\"")?;
    let keys: &[&str] = match op {
        "scan" => &["op", "snapshot"],
        "filter" => &["op", "input", "predicate"],
        "map" => &["op", "input", "name", "expr"],
        "project" => &["op", "input", "columns"],
        "aggregate" => &["op", "input", "group_by", "aggregates"],
        "sort" => &["op", "input", "keys"],
        "limit" => &["op", "input", "count"],
        other => return Err(format!("unknown op \"{other}\"")),
    };
    let o = Obj::new(v, keys)?;
    let input = || o.u32("input").map(NodeId);
    Ok(match op {
        "scan" => {
            let hex = o.str("snapshot")?;
            let snapshot = ContentHash::from_hex(hex)
                .ok_or_else(|| format!("\"{hex}\" is not a 64-digit hex snapshot hash"))?;
            Op::Scan { snapshot }
        }
        "filter" => Op::Filter {
            input: input()?,
            predicate: expr_from_json(o.get("predicate")?)?,
        },
        "map" => Op::Map {
            input: input()?,
            name: o.str("name")?.to_string(),
            expr: expr_from_json(o.get("expr")?)?,
        },
        "project" => Op::Project {
            input: input()?,
            columns: o.strings("columns")?,
        },
        "aggregate" => Op::Aggregate {
            input: input()?,
            group_by: o.strings("group_by")?,
            aggregates: o
                .array("aggregates")?
                .iter()
                .map(|a| {
                    let has_column = a.get("column").is_some();
                    let keys: &[&str] = if has_column {
                        &["name", "fn", "column"]
                    } else {
                        &["name", "fn"]
                    };
                    let a = Obj::new(a, keys)?;
                    let name = a.str("fn")?;
                    let func = AggFunc::ALL
                        .into_iter()
                        .find(|f| f.name() == name)
                        .ok_or_else(|| format!("unknown aggregate \"{name}\""))?;
                    Ok(Aggregate {
                        name: a.str("name")?.to_string(),
                        func,
                        column: if has_column {
                            Some(a.str("column")?.to_string())
                        } else {
                            None
                        },
                    })
                })
                .collect::<Result<_, String>>()?,
        },
        "sort" => Op::Sort {
            input: input()?,
            keys: o
                .array("keys")?
                .iter()
                .map(|k| {
                    let k = Obj::new(k, &["column", "order"])?;
                    let descending = match k.str("order")? {
                        "asc" => false,
                        "desc" => true,
                        other => {
                            return Err(format!("sort order must be asc or desc, not \"{other}\""));
                        }
                    };
                    Ok(SortKey {
                        column: k.str("column")?.to_string(),
                        descending,
                    })
                })
                .collect::<Result<_, String>>()?,
        },
        "limit" => {
            let s = o.str("count")?;
            Op::Limit {
                input: input()?,
                count: parse_decimal(s).ok_or_else(|| format!("\"{s}\" is not a row count"))?,
            }
        }
        _ => unreachable!(),
    })
}

pub fn expr_from_json(v: &Value) -> Result<Expr, String> {
    if v.get("column").is_some() {
        let o = Obj::new(v, &["column"])?;
        return Ok(Expr::Column(o.str("column")?.to_string()));
    }
    if v.get("literal").is_some() {
        let o = Obj::new(v, &["literal"])?;
        return Ok(Expr::Literal(literal_from_json(o.get("literal")?)?));
    }
    let name = v
        .get("call")
        .and_then(Value::as_str)
        .ok_or("an expression needs \"column\", \"literal\" or \"call\"")?;
    let keys: &[&str] = match name {
        "in" => &["call", "args", "values"],
        "date_trunc" => &["call", "args", "unit"],
        _ => &["call", "args"],
    };
    let o = Obj::new(v, keys)?;
    let args = o
        .array("args")?
        .iter()
        .map(expr_from_json)
        .collect::<Result<Vec<_>, _>>()?;
    let arity = |n: usize| {
        if args.len() == n {
            Ok(())
        } else {
            Err(format!("{name} takes {n} argument(s), got {}", args.len()))
        }
    };
    let mut it = args.clone().into_iter().map(Box::new);
    let mut next = || it.next().expect("arity checked");
    if let Some(op) = CmpOp::ALL.into_iter().find(|o| o.name() == name) {
        arity(2)?;
        return Ok(Expr::Compare(op, next(), next()));
    }
    if let Some(op) = ArithOp::ALL.into_iter().find(|o| o.name() == name) {
        arity(2)?;
        return Ok(Expr::Arith(op, next(), next()));
    }
    Ok(match name {
        "and" => Expr::And(args),
        "or" => Expr::Or(args),
        "not" => {
            arity(1)?;
            Expr::Not(next())
        }
        "is_null" => {
            arity(1)?;
            Expr::IsNull(next())
        }
        "lat" => {
            arity(1)?;
            Expr::Lat(next())
        }
        "lon" => {
            arity(1)?;
            Expr::Lon(next())
        }
        "in" => {
            arity(1)?;
            let values = o
                .array("values")?
                .iter()
                .map(literal_from_json)
                .collect::<Result<_, _>>()?;
            Expr::InList(next(), values)
        }
        "date_trunc" => {
            arity(1)?;
            let unit = o.str("unit")?;
            let unit = TimeUnit::ALL
                .into_iter()
                .find(|u| u.name() == unit)
                .ok_or_else(|| format!("unknown time unit \"{unit}\""))?;
            Expr::DateTrunc(unit, next())
        }
        other => return Err(format!("unknown function \"{other}\"")),
    })
}

pub fn literal_from_json(v: &Value) -> Result<Literal, String> {
    if v.is_null() {
        return Ok(Literal::Null);
    }
    let map = v
        .as_object()
        .ok_or("a literal must be null or a one-key object")?;
    if map.len() != 1 {
        return Err("a literal must have exactly one type key".into());
    }
    let (ty, value) = map.iter().next().expect("len is 1");
    let as_str = || {
        value
            .as_str()
            .ok_or(format!("a {ty} literal must be a string"))
    };
    Ok(match ty.as_str() {
        "bool" => Literal::Bool(
            value
                .as_bool()
                .ok_or("a bool literal must be true or false")?,
        ),
        "i64" => {
            let s = as_str()?;
            Literal::I64(parse_i64(s).ok_or_else(|| format!("\"{s}\" is not an integer"))?)
        }
        "f64" => {
            let x = value.as_f64().ok_or("an f64 literal must be a number")?;
            Literal::F64(x)
        }
        "str" => Literal::Str(as_str()?.to_string()),
        "timestamp" => {
            let s = as_str()?;
            Literal::Timestamp(
                parse_naive_timestamp(s)
                    .ok_or_else(|| format!("\"{s}\" is not a YYYY-MM-DDTHH:MM:SS timestamp"))?,
            )
        }
        other => return Err(format!("unknown literal type \"{other}\"")),
    })
}

/// Canonical decimal only: no sign, no leading zeros (so each value has
/// exactly one spelling, and one hash).
fn parse_decimal(s: &str) -> Option<u64> {
    let canonical =
        s == "0" || (!s.is_empty() && !s.starts_with('0') && s.bytes().all(|b| b.is_ascii_digit()));
    if canonical { s.parse().ok() } else { None }
}

fn parse_i64(s: &str) -> Option<i64> {
    match s.strip_prefix('-') {
        Some("0") => None,
        Some(rest) => {
            parse_decimal(rest)?;
            s.parse().ok()
        }
        None => {
            parse_decimal(s)?;
            s.parse().ok()
        }
    }
}

/// A JSON object whose keys must be exactly `allowed` (all required).
struct Obj<'a> {
    map: &'a Map<String, Value>,
}

impl<'a> Obj<'a> {
    fn new(v: &'a Value, allowed: &[&str]) -> Result<Self, String> {
        let map = v.as_object().ok_or("expected an object")?;
        if let Some(k) = map.keys().find(|k| !allowed.contains(&k.as_str())) {
            return Err(format!("unexpected key \"{k}\""));
        }
        if let Some(k) = allowed.iter().find(|k| !map.contains_key(**k)) {
            return Err(format!("missing key \"{k}\""));
        }
        Ok(Self { map })
    }
    fn get(&self, key: &str) -> Result<&'a Value, String> {
        self.map.get(key).ok_or(format!("missing key \"{key}\""))
    }
    fn str(&self, key: &str) -> Result<&'a str, String> {
        self.get(key)?
            .as_str()
            .ok_or(format!("\"{key}\" must be a string"))
    }
    fn u32(&self, key: &str) -> Result<u32, String> {
        self.get(key)?
            .as_u64()
            .and_then(|n| u32::try_from(n).ok())
            .ok_or(format!("\"{key}\" must be a step number"))
    }
    fn array(&self, key: &str) -> Result<&'a Vec<Value>, String> {
        self.get(key)?
            .as_array()
            .ok_or(format!("\"{key}\" must be a list"))
    }
    fn strings(&self, key: &str) -> Result<Vec<String>, String> {
        self.array(key)?
            .iter()
            .map(|s| {
                s.as_str()
                    .map(String::from)
                    .ok_or(format!("\"{key}\" must be a list of strings"))
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn integer_spellings_are_canonical() {
        assert_eq!(parse_i64("0"), Some(0));
        assert_eq!(parse_i64("-12"), Some(-12));
        assert_eq!(parse_i64("-9223372036854775808"), Some(i64::MIN));
        for bad in ["", "-", "-0", "007", "+1", "1.0", "9223372036854775808"] {
            assert_eq!(parse_i64(bad), None, "{bad}");
        }
        assert_eq!(parse_decimal("18446744073709551615"), Some(u64::MAX));
        assert_eq!(parse_decimal("01"), None);
    }
}
