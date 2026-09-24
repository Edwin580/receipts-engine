use crate::types::{Literal, Schema, ValueType};

/// A scalar expression, evaluated once per row.
///
/// Null semantics follow SQL: comparisons and arithmetic with a missing value
/// are missing, `And`/`Or`/`Not` use three-valued (Kleene) logic, and
/// `IsNull` is never missing. `docs/engine/plan.md` has the full table.
#[derive(Clone, PartialEq, Debug)]
pub enum Expr {
    Column(String),
    Literal(Literal),
    Compare(CmpOp, Box<Expr>, Box<Expr>),
    And(Vec<Expr>),
    Or(Vec<Expr>),
    Not(Box<Expr>),
    IsNull(Box<Expr>),
    /// True if the value equals one of the (non-null) literals.
    InList(Box<Expr>, Vec<Literal>),
    Arith(ArithOp, Box<Expr>, Box<Expr>),
    /// Truncates a timestamp to the start of its day, ISO week (Monday),
    /// month or year.
    DateTrunc(TimeUnit, Box<Expr>),
    /// Latitude of a location, as a decimal.
    Lat(Box<Expr>),
    /// Longitude of a location, as a decimal.
    Lon(Box<Expr>),
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum CmpOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

impl CmpOp {
    pub const ALL: [Self; 6] = [Self::Eq, Self::Ne, Self::Lt, Self::Le, Self::Gt, Self::Ge];

    pub fn name(self) -> &'static str {
        match self {
            Self::Eq => "eq",
            Self::Ne => "ne",
            Self::Lt => "lt",
            Self::Le => "le",
            Self::Gt => "gt",
            Self::Ge => "ge",
        }
    }

    /// Whether `ordering` (left compared to right) satisfies the operator.
    pub fn holds(self, ordering: std::cmp::Ordering) -> bool {
        use std::cmp::Ordering::*;
        match self {
            Self::Eq => ordering == Equal,
            Self::Ne => ordering != Equal,
            Self::Lt => ordering == Less,
            Self::Le => ordering != Greater,
            Self::Gt => ordering == Greater,
            Self::Ge => ordering != Less,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum ArithOp {
    Add,
    Sub,
    Mul,
    /// Always produces a decimal. Division by zero produces a missing value,
    /// as does any decimal result too large to represent.
    Div,
}

impl ArithOp {
    pub const ALL: [Self; 4] = [Self::Add, Self::Sub, Self::Mul, Self::Div];

    pub fn name(self) -> &'static str {
        match self {
            Self::Add => "add",
            Self::Sub => "sub",
            Self::Mul => "mul",
            Self::Div => "div",
        }
    }

    fn symbol(self) -> &'static str {
        match self {
            Self::Add => "+",
            Self::Sub => "−",
            Self::Mul => "×",
            Self::Div => "÷",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum TimeUnit {
    Day,
    Week,
    Month,
    Year,
}

impl TimeUnit {
    pub const ALL: [Self; 4] = [Self::Day, Self::Week, Self::Month, Self::Year];

    pub fn name(self) -> &'static str {
        match self {
            Self::Day => "day",
            Self::Week => "week",
            Self::Month => "month",
            Self::Year => "year",
        }
    }
}

/// The result type of an expression. `ty` is `None` only for a bare `null`
/// literal, which validation refuses everywhere a type is needed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ExprType {
    pub ty: Option<ValueType>,
    pub nullable: bool,
}

impl ExprType {
    fn of(ty: ValueType, nullable: bool) -> Self {
        Self {
            ty: Some(ty),
            nullable,
        }
    }
}

// Constructors, so tests and callers can write `col("a").eq(lit_str("x"))`.
pub fn col(name: &str) -> Expr {
    Expr::Column(name.into())
}

pub fn lit(value: Literal) -> Expr {
    Expr::Literal(value)
}

pub fn lit_str(s: &str) -> Expr {
    Expr::Literal(Literal::Str(s.into()))
}

pub fn lit_i64(i: i64) -> Expr {
    Expr::Literal(Literal::I64(i))
}

pub fn lit_f64(x: f64) -> Expr {
    Expr::Literal(Literal::F64(x))
}

/// `!e` builds `Expr::Not(e)`.
impl std::ops::Not for Expr {
    type Output = Expr;
    fn not(self) -> Expr {
        Expr::Not(Box::new(self))
    }
}

impl Expr {
    pub fn cmp(self, op: CmpOp, other: Expr) -> Expr {
        Expr::Compare(op, Box::new(self), Box::new(other))
    }
    pub fn eq(self, other: Expr) -> Expr {
        self.cmp(CmpOp::Eq, other)
    }
    pub fn lt(self, other: Expr) -> Expr {
        self.cmp(CmpOp::Lt, other)
    }
    pub fn ge(self, other: Expr) -> Expr {
        self.cmp(CmpOp::Ge, other)
    }
    pub fn arith(self, op: ArithOp, other: Expr) -> Expr {
        Expr::Arith(op, Box::new(self), Box::new(other))
    }
    pub fn is_null(self) -> Expr {
        Expr::IsNull(Box::new(self))
    }
    pub fn in_list(self, values: Vec<Literal>) -> Expr {
        Expr::InList(Box::new(self), values)
    }
    pub fn date_trunc(self, unit: TimeUnit) -> Expr {
        Expr::DateTrunc(unit, Box::new(self))
    }

    /// Infers the expression's type against `schema`, or explains in plain
    /// English why it has none.
    pub fn type_of(&self, schema: &Schema) -> Result<ExprType, String> {
        use ValueType as V;
        Ok(match self {
            Expr::Column(name) => {
                let f = schema
                    .field(name)
                    .ok_or_else(|| format!("there is no column named \"{name}\""))?;
                ExprType::of(V::of_column(f.ty), f.nullable)
            }
            Expr::Literal(l) => {
                if let Literal::F64(x) = l
                    && !x.is_finite()
                {
                    return Err(format!("{x} is not a usable number"));
                }
                ExprType {
                    ty: l.value_type(),
                    nullable: *l == Literal::Null,
                }
            }
            Expr::Compare(op, a, b) => {
                let (ta, tb) = (a.typed(schema)?, b.typed(schema)?);
                if !comparable(ta.0, tb.0) {
                    return Err(format!(
                        "can't compare {} with {} ({})",
                        ta.0.name(),
                        tb.0.name(),
                        op.name()
                    ));
                }
                ExprType::of(V::Bool, ta.1 || tb.1)
            }
            Expr::And(args) | Expr::Or(args) => {
                if args.is_empty() {
                    return Err("and/or needs at least one argument".into());
                }
                let mut nullable = false;
                for a in args {
                    nullable |= a.boolean(schema)?;
                }
                ExprType::of(V::Bool, nullable)
            }
            Expr::Not(a) => ExprType::of(V::Bool, a.boolean(schema)?),
            Expr::IsNull(a) => {
                a.type_of(schema)?;
                ExprType::of(V::Bool, false)
            }
            Expr::InList(a, values) => {
                let (ty, nullable) = a.typed(schema)?;
                if values.is_empty() {
                    return Err("\"in\" needs at least one value".into());
                }
                for v in values {
                    let vt = v
                        .value_type()
                        .ok_or("\"in\" can't list a missing value; use is_null")?;
                    if !comparable(ty, vt) {
                        return Err(format!(
                            "can't look for a {} value in a {} column",
                            vt.name(),
                            ty.name()
                        ));
                    }
                }
                ExprType::of(V::Bool, nullable)
            }
            Expr::Arith(op, a, b) => {
                let ((ta, na), (tb, nb)) = (a.typed(schema)?, b.typed(schema)?);
                let ty = arith_type(*op, ta, tb).ok_or_else(|| {
                    format!("can't {} {} and {}", op.name(), ta.name(), tb.name())
                })?;
                // Decimal results that overflow (or divide by zero) are
                // missing; integer overflow is an error instead.
                ExprType::of(ty, na || nb || ty == V::F64)
            }
            Expr::DateTrunc(_, a) => {
                let (ty, nullable) = a.typed(schema)?;
                if ty != V::Timestamp {
                    return Err(format!("date_trunc needs a timestamp, not {}", ty.name()));
                }
                ExprType::of(V::Timestamp, nullable)
            }
            Expr::Lat(a) | Expr::Lon(a) => {
                let (ty, nullable) = a.typed(schema)?;
                if ty != V::Geo {
                    return Err(format!("lat/lon needs a location, not {}", ty.name()));
                }
                ExprType::of(V::F64, nullable)
            }
        })
    }

    /// Type and nullability, refusing an untyped `null`.
    fn typed(&self, schema: &Schema) -> Result<(ValueType, bool), String> {
        let t = self.type_of(schema)?;
        match t.ty {
            Some(ty) => Ok((ty, t.nullable)),
            None => {
                Err("a bare null has no type here; use is_null to test for missing values".into())
            }
        }
    }

    /// Checks that the expression is boolean and returns its nullability.
    fn boolean(&self, schema: &Schema) -> Result<bool, String> {
        match self.typed(schema)? {
            (ValueType::Bool, n) => Ok(n),
            (ty, _) => Err(format!("expected true/false, got {}", ty.name())),
        }
    }

    /// Every column the expression reads, in first-mention order.
    pub fn columns(&self) -> Vec<&str> {
        let mut out = Vec::new();
        self.visit_columns(&mut out);
        out
    }

    fn visit_columns<'a>(&'a self, out: &mut Vec<&'a str>) {
        match self {
            Expr::Column(c) => {
                if !out.contains(&c.as_str()) {
                    out.push(c)
                }
            }
            Expr::Literal(_) => {}
            Expr::Compare(_, a, b) | Expr::Arith(_, a, b) => {
                a.visit_columns(out);
                b.visit_columns(out);
            }
            Expr::And(args) | Expr::Or(args) => args.iter().for_each(|a| a.visit_columns(out)),
            Expr::Not(a)
            | Expr::IsNull(a)
            | Expr::InList(a, _)
            | Expr::DateTrunc(_, a)
            | Expr::Lat(a)
            | Expr::Lon(a) => a.visit_columns(out),
        }
    }

    /// Whether any division occurs (for the "division by zero" note).
    pub fn has_division(&self) -> bool {
        match self {
            Expr::Arith(ArithOp::Div, _, _) => true,
            Expr::Column(_) | Expr::Literal(_) => false,
            Expr::Compare(_, a, b) | Expr::Arith(_, a, b) => a.has_division() || b.has_division(),
            Expr::And(args) | Expr::Or(args) => args.iter().any(Expr::has_division),
            Expr::Not(a)
            | Expr::IsNull(a)
            | Expr::InList(a, _)
            | Expr::DateTrunc(_, a)
            | Expr::Lat(a)
            | Expr::Lon(a) => a.has_division(),
        }
    }

    /// Plain-English rendering, e.g. `closed_date is before created_date`.
    /// `schema` decides wording that depends on type (before/after for times).
    pub fn render(&self, schema: &Schema) -> String {
        self.render_prec(schema, 0)
    }

    // Precedence: 0 = top, 1 = inside and/or, 2 = operand of arithmetic.
    fn render_prec(&self, schema: &Schema, prec: u8) -> String {
        let paren = |s: String, needed: bool| if needed { format!("({s})") } else { s };
        match self {
            Expr::Column(c) => c.clone(),
            Expr::Literal(l) => l.to_string(),
            Expr::Compare(op, a, b) => {
                let temporal = matches!(
                    a.type_of(schema).ok().and_then(|t| t.ty),
                    Some(ValueType::Timestamp)
                );
                let verb = match (op, temporal) {
                    (CmpOp::Eq, _) => "is",
                    (CmpOp::Ne, _) => "is not",
                    (CmpOp::Lt, true) => "is before",
                    (CmpOp::Le, true) => "is on or before",
                    (CmpOp::Gt, true) => "is after",
                    (CmpOp::Ge, true) => "is on or after",
                    (CmpOp::Lt, false) => "is less than",
                    (CmpOp::Le, false) => "is at most",
                    (CmpOp::Gt, false) => "is greater than",
                    (CmpOp::Ge, false) => "is at least",
                };
                paren(
                    format!(
                        "{} {verb} {}",
                        a.render_prec(schema, 2),
                        b.render_prec(schema, 2)
                    ),
                    prec >= 2,
                )
            }
            Expr::And(args) | Expr::Or(args) => {
                let joiner = if matches!(self, Expr::And(_)) {
                    " and "
                } else {
                    " or "
                };
                if args.len() == 1 {
                    return args[0].render_prec(schema, prec);
                }
                let parts: Vec<_> = args.iter().map(|a| a.render_prec(schema, 1)).collect();
                paren(parts.join(joiner), prec >= 1)
            }
            Expr::Not(a) => match a.as_ref() {
                Expr::IsNull(inner) => format!("{} is present", inner.render_prec(schema, 2)),
                _ => format!("not {}", paren(a.render_prec(schema, 0), true)),
            },
            Expr::IsNull(a) => paren(
                format!("{} is missing", a.render_prec(schema, 2)),
                prec >= 2,
            ),
            Expr::InList(a, values) => {
                let vs: Vec<_> = values.iter().map(Literal::to_string).collect();
                let rendered = if vs.len() == 1 {
                    format!("{} is {}", a.render_prec(schema, 2), vs[0])
                } else {
                    format!("{} is one of {}", a.render_prec(schema, 2), vs.join(", "))
                };
                paren(rendered, prec >= 2)
            }
            Expr::Arith(op, a, b) => paren(
                format!(
                    "{} {} {}",
                    a.render_prec(schema, 2),
                    op.symbol(),
                    b.render_prec(schema, 2)
                ),
                prec >= 2,
            ),
            Expr::DateTrunc(unit, a) => {
                let what = match unit {
                    TimeUnit::Day => "day",
                    TimeUnit::Week => "week (starting Monday)",
                    TimeUnit::Month => "month",
                    TimeUnit::Year => "year",
                };
                format!("the {what} of {}", a.render_prec(schema, 2))
            }
            Expr::Lat(a) => format!("the latitude of {}", a.render_prec(schema, 2)),
            Expr::Lon(a) => format!("the longitude of {}", a.render_prec(schema, 2)),
        }
    }
}

/// Values of these two types can be ordered against each other.
pub fn comparable(a: ValueType, b: ValueType) -> bool {
    use ValueType as V;
    (a.is_numeric() && b.is_numeric()) || (a == b && matches!(a, V::Timestamp | V::Str | V::Bool))
}

/// Result type of `a op b`, or `None` if the operation is not defined.
pub fn arith_type(op: ArithOp, a: ValueType, b: ValueType) -> Option<ValueType> {
    use ArithOp::*;
    use ValueType::*;
    Some(match (op, a, b) {
        (Div, x, y) if x.is_numeric() && y.is_numeric() => F64,
        (Add | Sub | Mul, I64, I64) => I64,
        (Add | Sub | Mul, x, y) if x.is_numeric() && y.is_numeric() => F64,
        // Durations are integer microseconds.
        (Sub, Timestamp, Timestamp) => I64,
        (Add | Sub, Timestamp, I64) => Timestamp,
        (Add, I64, Timestamp) => Timestamp,
        _ => return None,
    })
}
