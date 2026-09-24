//! Logical plan DAG: typed operators, validation, canonical JSON, plan hashing,
//! and the `describe()` sentences for each operator. See `docs/engine/plan.md`.

mod describe;
mod expr;
pub mod hash;
pub mod json;
mod plan;
mod types;

pub use describe::{describe, describe_node};
pub use expr::{
    ArithOp, CmpOp, Expr, ExprType, TimeUnit, arith_type, col, comparable, lit, lit_f64, lit_i64,
    lit_str,
};
pub use plan::{
    AggFunc, Aggregate, Catalog, NodeId, Op, Plan, PlanError, SortKey, SourceInfo, ValidPlan,
    validate,
};
pub use types::{Field, Literal, Schema, ValueType};
