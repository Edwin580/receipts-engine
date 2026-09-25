//! Vectorized expression evaluation. Each expression becomes a column (or a
//! constant); semantics are in `docs/engine/plan.md` §2.

use crate::table::{Table, validity_from};
use receipts_core::time::{MICROS_PER_DAY, civil_from_days, days_from_civil};
use receipts_core::{Bitmap, Column, ColumnData};
use receipts_plan::{ArithOp, CmpOp, Expr, Literal, TimeUnit, ValueType, arith_type};
use std::cmp::Ordering;
use std::sync::Arc;

/// An evaluated expression: a column of the table's length, or a constant.
#[derive(Clone, Debug)]
pub(crate) enum Datum {
    Col(Arc<Column>),
    Scalar(Literal),
}

/// Per-row accessor; `None` is a missing value.
type Get<'a, T> = Box<dyn Fn(usize) -> Option<T> + 'a>;

pub(crate) struct Evaluator<'a> {
    pub table: &'a Table,
}

impl Evaluator<'_> {
    fn n(&self) -> usize {
        self.table.len()
    }

    fn ty(&self, e: &Expr) -> ValueType {
        e.type_of(self.table.schema())
            .ok()
            .and_then(|t| t.ty)
            .expect("validated plans only contain typed expressions")
    }

    pub fn eval(&self, e: &Expr) -> Result<Datum, String> {
        Ok(match e {
            Expr::Column(c) => Datum::Col(
                self.table
                    .column(c)
                    .expect("validated plans only name existing columns")
                    .clone(),
            ),
            Expr::Literal(l) => Datum::Scalar(l.clone()),
            Expr::Compare(op, a, b) => {
                let (ta, tb) = (self.ty(a), self.ty(b));
                let (da, db) = (self.eval(a)?, self.eval(b)?);
                self.compare(*op, &da, ta, &db, tb)
            }
            Expr::And(args) | Expr::Or(args) => {
                let is_and = matches!(e, Expr::And(_));
                let datums = args
                    .iter()
                    .map(|a| self.eval(a))
                    .collect::<Result<Vec<_>, _>>()?;
                let getters: Vec<Get<bool>> = datums.iter().map(get_bool).collect();
                // Kleene logic: the deciding value (false for and, true for
                // or) wins over missing; otherwise missing wins.
                bool_col(self.n(), |i| {
                    let mut missing = false;
                    for g in &getters {
                        match g(i) {
                            Some(v) if v != is_and => return Some(v),
                            Some(_) => {}
                            None => missing = true,
                        }
                    }
                    (!missing).then_some(is_and)
                })
            }
            Expr::Not(a) => {
                let d = self.eval(a)?;
                let g = get_bool(&d);
                bool_col(self.n(), |i| g(i).map(|v| !v))
            }
            Expr::IsNull(a) => {
                let d = self.eval(a)?;
                match &d {
                    Datum::Scalar(l) => Datum::Scalar(Literal::Bool(*l == Literal::Null)),
                    Datum::Col(c) => bool_col(self.n(), |i| Some(!c.is_valid(i))),
                }
            }
            Expr::InList(a, values) => {
                let d = self.eval(a)?;
                if let Datum::Col(c) = &d
                    && let ColumnData::DictUtf8 { codes, dictionary } = &c.data
                {
                    // One lookup per dictionary entry instead of per row.
                    let mask: Vec<bool> = dictionary
                        .iter()
                        .map(|s| {
                            values
                                .iter()
                                .any(|v| matches!(v, Literal::Str(x) if x == s))
                        })
                        .collect();
                    return Ok(bool_col(self.n(), |i| {
                        c.is_valid(i).then(|| mask[codes[i] as usize])
                    }));
                }
                // No listed value is missing, so this is exactly an `or` of
                // equalities under Kleene logic.
                let ta = self.ty(a);
                let parts = values
                    .iter()
                    .map(|v| {
                        let tv = v.value_type().expect("validated: no null in lists");
                        self.compare(CmpOp::Eq, &d, ta, &Datum::Scalar(v.clone()), tv)
                    })
                    .collect::<Vec<_>>();
                let getters: Vec<Get<bool>> = parts.iter().map(get_bool).collect();
                bool_col(self.n(), |i| {
                    let mut out = Some(false);
                    for g in &getters {
                        match g(i) {
                            Some(true) => return Some(true),
                            Some(false) => {}
                            None => out = None,
                        }
                    }
                    out
                })
            }
            Expr::Arith(op, a, b) => {
                let (ta, tb) = (self.ty(a), self.ty(b));
                let (da, db) = (self.eval(a)?, self.eval(b)?);
                self.arith(*op, &da, ta, &db, tb)?
            }
            Expr::DateTrunc(unit, a) => {
                let d = self.eval(a)?;
                let g = get_i64(&d);
                i64_col(self.n(), true, |i| g(i).map(|t| trunc(*unit, t)))
            }
            Expr::Lat(a) | Expr::Lon(a) => {
                let is_lat = matches!(e, Expr::Lat(_));
                let d = self.eval(a)?;
                let Datum::Col(c) = &d else {
                    unreachable!("there are no location literals")
                };
                let ColumnData::Geo { lat, lon } = &c.data else {
                    unreachable!("validated: lat/lon of a location")
                };
                let v = if is_lat { lat } else { lon };
                f64_col(self.n(), |i| c.is_valid(i).then(|| f64::from(v[i])))
            }
        })
    }

    fn compare(&self, op: CmpOp, a: &Datum, ta: ValueType, b: &Datum, tb: ValueType) -> Datum {
        let n = self.n();
        use ValueType::*;
        let ord: Get<Ordering> = match (ta, tb) {
            (Str, Str) => match (a, b) {
                (Datum::Col(c), Datum::Scalar(Literal::Str(s))) => dict_vs_str(c, s),
                (Datum::Scalar(Literal::Str(s)), Datum::Col(c)) => {
                    let f = dict_vs_str(c, s);
                    Box::new(move |i| f(i).map(Ordering::reverse))
                }
                _ => {
                    let (ga, gb) = (get_str(a), get_str(b));
                    Box::new(move |i| Some(ga(i)?.cmp(gb(i)?)))
                }
            },
            (Timestamp, Timestamp) | (I64, I64) => {
                let (ga, gb) = (get_i64(a), get_i64(b));
                Box::new(move |i| Some(ga(i)?.cmp(&gb(i)?)))
            }
            (Bool, Bool) => {
                let (ga, gb) = (get_bool(a), get_bool(b));
                Box::new(move |i| Some(ga(i)?.cmp(&gb(i)?)))
            }
            _ => {
                let (ga, gb) = (get_num(a), get_num(b));
                Box::new(move |i| Some(cmp_num(ga(i)?, gb(i)?)))
            }
        };
        bool_col(n, |i| ord(i).map(|o| op.holds(o)))
    }

    fn arith(
        &self,
        op: ArithOp,
        a: &Datum,
        ta: ValueType,
        b: &Datum,
        tb: ValueType,
    ) -> Result<Datum, String> {
        let n = self.n();
        let out = arith_type(op, ta, tb).expect("validated arithmetic");
        if out == ValueType::F64 {
            let (ga, gb) = (get_f64(a), get_f64(b));
            return Ok(f64_col(n, |i| {
                let (x, y) = (ga(i)?, gb(i)?);
                let r = match op {
                    ArithOp::Add => x + y,
                    ArithOp::Sub => x - y,
                    ArithOp::Mul => x * y,
                    ArithOp::Div if y == 0.0 => return None,
                    ArithOp::Div => x / y,
                };
                // Overflow and undefined results are missing, so decimal
                // columns never hold NaN or infinity.
                r.is_finite().then_some(r)
            }));
        }
        // Integer, duration and timestamp arithmetic is exact or an error.
        let (ga, gb) = (get_i64(a), get_i64(b));
        let mut values = Vec::with_capacity(n);
        let mut valid = Vec::with_capacity(n);
        for i in 0..n {
            match (ga(i), gb(i)) {
                (Some(x), Some(y)) => {
                    let r = match op {
                        ArithOp::Add => x.checked_add(y),
                        ArithOp::Sub => x.checked_sub(y),
                        ArithOp::Mul => x.checked_mul(y),
                        ArithOp::Div => unreachable!("division is always decimal"),
                    };
                    let r = r.ok_or_else(|| {
                        format!("{x} {} {y} overflows a 64-bit integer (row {i})", op.name())
                    })?;
                    values.push(r);
                    valid.push(true);
                }
                _ => {
                    values.push(0);
                    valid.push(false);
                }
            }
        }
        let validity = validity_from(n, |i| valid[i]);
        let data = if out == ValueType::Timestamp {
            ColumnData::Timestamp(values)
        } else {
            ColumnData::I64(values)
        };
        Ok(Datum::Col(Arc::new(Column::new("", data, validity))))
    }
}

/// Turns a datum into a named column of `n` rows (constants are repeated).
pub(crate) fn materialize(d: Datum, n: usize, name: &str) -> Column {
    let col = match d {
        Datum::Col(c) => Arc::unwrap_or_clone(c),
        Datum::Scalar(l) => {
            let data = match l {
                Literal::Bool(b) => ColumnData::Bool(Bitmap::from_bools(std::iter::repeat_n(b, n))),
                Literal::I64(i) => ColumnData::I64(vec![i; n]),
                Literal::F64(x) => ColumnData::F64(vec![x; n]),
                Literal::Timestamp(t) => ColumnData::Timestamp(vec![t; n]),
                Literal::Str(s) => ColumnData::DictUtf8 {
                    codes: vec![0; n],
                    dictionary: vec![s],
                },
                Literal::Null => unreachable!("validated: no untyped null columns"),
            };
            Column::new("", data, None)
        }
    };
    crate::table::renamed(col, name)
}

/// Rows where the predicate is true (missing counts as not true).
pub(crate) fn selection(d: &Datum, n: usize) -> Vec<u32> {
    let g = get_bool(d);
    (0..n)
        .filter(|&i| g(i) == Some(true))
        .map(|i| i as u32)
        .collect()
}

fn bool_col(n: usize, f: impl Fn(usize) -> Option<bool>) -> Datum {
    let values: Vec<Option<bool>> = (0..n).map(f).collect();
    let data = ColumnData::Bool(Bitmap::from_bools(
        values.iter().map(|v| v.unwrap_or(false)),
    ));
    let validity = validity_from(n, |i| values[i].is_some());
    Datum::Col(Arc::new(Column::new("", data, validity)))
}

fn i64_col(n: usize, timestamp: bool, f: impl Fn(usize) -> Option<i64>) -> Datum {
    let values: Vec<Option<i64>> = (0..n).map(f).collect();
    let v: Vec<i64> = values.iter().map(|v| v.unwrap_or(0)).collect();
    let data = if timestamp {
        ColumnData::Timestamp(v)
    } else {
        ColumnData::I64(v)
    };
    let validity = validity_from(n, |i| values[i].is_some());
    Datum::Col(Arc::new(Column::new("", data, validity)))
}

fn f64_col(n: usize, f: impl Fn(usize) -> Option<f64>) -> Datum {
    let values: Vec<Option<f64>> = (0..n).map(f).collect();
    let data = ColumnData::F64(values.iter().map(|v| v.unwrap_or(0.0)).collect());
    let validity = validity_from(n, |i| values[i].is_some());
    Datum::Col(Arc::new(Column::new("", data, validity)))
}

fn get_i64(d: &Datum) -> Get<'_, i64> {
    match d {
        Datum::Scalar(Literal::I64(v) | Literal::Timestamp(v)) => {
            let v = *v;
            Box::new(move |_| Some(v))
        }
        Datum::Col(c) => match &c.data {
            ColumnData::I64(v) | ColumnData::Timestamp(v) => {
                Box::new(move |i| c.is_valid(i).then(|| v[i]))
            }
            _ => unreachable!("validated: integer operand"),
        },
        Datum::Scalar(_) => unreachable!("validated: integer operand"),
    }
}

fn get_f64(d: &Datum) -> Get<'_, f64> {
    let g = get_num(d);
    Box::new(move |i| {
        g(i).map(|n| match n {
            Num::I(x) => x as f64,
            Num::F(x) => x,
        })
    })
}

#[derive(Clone, Copy)]
enum Num {
    I(i64),
    F(f64),
}

fn get_num(d: &Datum) -> Get<'_, Num> {
    match d {
        Datum::Scalar(Literal::I64(v)) => {
            let v = *v;
            Box::new(move |_| Some(Num::I(v)))
        }
        Datum::Scalar(Literal::F64(v)) => {
            let v = *v;
            Box::new(move |_| Some(Num::F(v)))
        }
        Datum::Col(c) => match &c.data {
            ColumnData::I64(v) => Box::new(move |i| c.is_valid(i).then(|| Num::I(v[i]))),
            ColumnData::F64(v) => Box::new(move |i| c.is_valid(i).then(|| Num::F(v[i]))),
            _ => unreachable!("validated: numeric operand"),
        },
        Datum::Scalar(_) => unreachable!("validated: numeric operand"),
    }
}

fn get_bool(d: &Datum) -> Get<'_, bool> {
    match d {
        Datum::Scalar(Literal::Bool(b)) => {
            let b = *b;
            Box::new(move |_| Some(b))
        }
        Datum::Scalar(Literal::Null) => Box::new(|_| None),
        Datum::Col(c) => match &c.data {
            ColumnData::Bool(b) => Box::new(move |i| c.is_valid(i).then(|| b.get(i))),
            _ => unreachable!("validated: boolean operand"),
        },
        Datum::Scalar(_) => unreachable!("validated: boolean operand"),
    }
}

fn get_str(d: &Datum) -> Get<'_, &str> {
    match d {
        Datum::Scalar(Literal::Str(s)) => Box::new(move |_| Some(s.as_str())),
        Datum::Col(c) => match &c.data {
            ColumnData::DictUtf8 { codes, dictionary } => Box::new(move |i| {
                c.is_valid(i)
                    .then(|| dictionary[codes[i] as usize].as_str())
            }),
            _ => unreachable!("validated: text operand"),
        },
        Datum::Scalar(_) => unreachable!("validated: text operand"),
    }
}

/// Compares a dictionary column with a constant string using codes only.
/// Dictionaries are sorted, so a code's position relative to the string's
/// insertion point decides the order.
fn dict_vs_str<'a>(c: &'a Column, s: &str) -> Get<'a, Ordering> {
    let ColumnData::DictUtf8 { codes, dictionary } = &c.data else {
        unreachable!("validated: text column")
    };
    let pos = dictionary.partition_point(|d| d.as_str() < s) as u32;
    let found = dictionary.get(pos as usize).is_some_and(|d| d == s);
    Box::new(move |i| {
        c.is_valid(i).then(|| {
            let code = codes[i];
            if code < pos {
                Ordering::Less
            } else if code == pos && found {
                Ordering::Equal
            } else {
                Ordering::Greater
            }
        })
    })
}

/// Exact comparison, including between integers and decimals.
fn cmp_num(a: Num, b: Num) -> Ordering {
    match (a, b) {
        (Num::I(x), Num::I(y)) => x.cmp(&y),
        (Num::F(x), Num::F(y)) => cmp_f64(x, y),
        (Num::I(x), Num::F(y)) => cmp_i64_f64(x, y),
        (Num::F(x), Num::I(y)) => cmp_i64_f64(y, x).reverse(),
    }
}

/// Decimal columns hold no NaN (see `arith`), so this is a total order in
/// which `-0.0 == 0.0`.
pub(crate) fn cmp_f64(x: f64, y: f64) -> Ordering {
    x.partial_cmp(&y).unwrap_or_else(|| x.total_cmp(&y))
}

fn cmp_i64_f64(x: i64, y: f64) -> Ordering {
    const TWO_63: f64 = 9_223_372_036_854_775_808.0;
    if y >= TWO_63 {
        return Ordering::Less;
    }
    if y < -TWO_63 {
        return Ordering::Greater;
    }
    // y is in [-2^63, 2^63), so its integer part fits in i64 exactly.
    let t = y.trunc();
    match x.cmp(&(t as i64)) {
        Ordering::Equal => cmp_f64(0.0, y - t),
        other => other,
    }
}

/// Start of the day, ISO week (Monday), month or year containing `t`.
pub(crate) fn trunc(unit: TimeUnit, t: i64) -> i64 {
    let days = t.div_euclid(MICROS_PER_DAY);
    let start = match unit {
        TimeUnit::Day => days,
        // 1970-01-01 was a Thursday, so Monday is 3 days before day 0.
        TimeUnit::Week => days - (days + 3).rem_euclid(7),
        TimeUnit::Month => {
            let (y, m, _) = civil_from_days(days);
            days_from_civil(y, m, 1)
        }
        TimeUnit::Year => {
            let (y, _, _) = civil_from_days(days);
            days_from_civil(y, 1, 1)
        }
    };
    start * MICROS_PER_DAY
}

#[cfg(test)]
mod tests {
    use super::*;
    use receipts_core::time::parse_naive_timestamp;

    fn t(s: &str) -> i64 {
        parse_naive_timestamp(s).unwrap()
    }

    #[test]
    fn truncates_calendar_units() {
        let x = t("2024-03-14T15:09:26.535");
        assert_eq!(trunc(TimeUnit::Day, x), t("2024-03-14T00:00:00"));
        assert_eq!(trunc(TimeUnit::Week, x), t("2024-03-11T00:00:00")); // Monday
        assert_eq!(trunc(TimeUnit::Month, x), t("2024-03-01T00:00:00"));
        assert_eq!(trunc(TimeUnit::Year, x), t("2024-01-01T00:00:00"));
        // Before 1970, and a Monday maps to itself.
        assert_eq!(
            trunc(TimeUnit::Week, t("1900-01-01T12:00:00")),
            t("1900-01-01T00:00:00")
        );
        assert_eq!(
            trunc(TimeUnit::Day, t("1969-12-31T23:59:59")),
            t("1969-12-31T00:00:00")
        );
    }

    #[test]
    fn compares_integers_with_decimals_exactly() {
        assert_eq!(cmp_i64_f64(i64::MAX, 2f64.powi(63)), Ordering::Less);
        assert_eq!(cmp_i64_f64(i64::MIN, -(2f64.powi(63))), Ordering::Equal);
        assert_eq!(cmp_i64_f64(3, 3.5), Ordering::Less);
        assert_eq!(cmp_i64_f64(-3, -3.5), Ordering::Greater);
        assert_eq!(cmp_i64_f64(0, -0.0), Ordering::Equal);
        // 2^53 + 1 is not a double; a lossy comparison would say Equal.
        assert_eq!(
            cmp_i64_f64((1 << 53) + 1, 9007199254740992.0),
            Ordering::Greater
        );
    }
}
