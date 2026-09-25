use crate::expr::{Expr, ExprType};
use crate::types::{Field, Schema, ValueType};
use receipts_core::{ColumnType, ContentHash};
use std::fmt;

/// Index of a node in `Plan::nodes`. Inputs always point at earlier nodes,
/// so the node list is a topological order of the DAG.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct NodeId(pub u32);

impl NodeId {
    pub fn index(self) -> usize {
        self.0 as usize
    }
}

/// One step of a plan. Every operator except `Scan` reads exactly one input.
#[derive(Clone, PartialEq, Debug)]
pub enum Op {
    /// All rows of a snapshot, identified by its `snapshot_hash`.
    Scan { snapshot: ContentHash },
    /// Keeps the rows where `predicate` is true. Missing counts as not true.
    Filter { input: NodeId, predicate: Expr },
    /// Appends a computed column. It can't replace an existing one.
    Map {
        input: NodeId,
        name: String,
        expr: Expr,
    },
    /// Keeps the listed columns, in the listed order.
    Project { input: NodeId, columns: Vec<String> },
    /// One output row per distinct combination of `group_by` values (one row
    /// in total when `group_by` is empty), sorted by the group values.
    Aggregate {
        input: NodeId,
        group_by: Vec<String>,
        aggregates: Vec<Aggregate>,
    },
    /// Stable sort; missing values are smallest.
    Sort { input: NodeId, keys: Vec<SortKey> },
    /// The first `count` rows.
    Limit { input: NodeId, count: u64 },
}

impl Op {
    pub fn input(&self) -> Option<NodeId> {
        match self {
            Op::Scan { .. } => None,
            Op::Filter { input, .. }
            | Op::Map { input, .. }
            | Op::Project { input, .. }
            | Op::Aggregate { input, .. }
            | Op::Sort { input, .. }
            | Op::Limit { input, .. } => Some(*input),
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            Op::Scan { .. } => "scan",
            Op::Filter { .. } => "filter",
            Op::Map { .. } => "map",
            Op::Project { .. } => "project",
            Op::Aggregate { .. } => "aggregate",
            Op::Sort { .. } => "sort",
            Op::Limit { .. } => "limit",
        }
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Aggregate {
    /// Output column name.
    pub name: String,
    pub func: AggFunc,
    /// The input column; `None` only for `Count`.
    pub column: Option<String>,
}

impl Aggregate {
    pub fn new(name: &str, func: AggFunc, column: Option<&str>) -> Self {
        Self {
            name: name.into(),
            func,
            column: column.map(Into::into),
        }
    }
}

/// Aggregate functions. All except `Count` ignore missing values; over no
/// values, `CountNonNull` is 0 and the others are missing.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum AggFunc {
    /// Rows in the group.
    Count,
    CountNonNull,
    /// Integer sums are exact and fail on overflow; decimal sums add in row
    /// order.
    Sum,
    Mean,
    /// The middle value, or the mean of the two middle values.
    Median,
    Min,
    Max,
}

impl AggFunc {
    pub const ALL: [Self; 7] = [
        Self::Count,
        Self::CountNonNull,
        Self::Sum,
        Self::Mean,
        Self::Median,
        Self::Min,
        Self::Max,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Self::Count => "count",
            Self::CountNonNull => "count_non_null",
            Self::Sum => "sum",
            Self::Mean => "mean",
            Self::Median => "median",
            Self::Min => "min",
            Self::Max => "max",
        }
    }

    /// Output type for an input column of type `input`, or `None` if the
    /// function doesn't apply to it.
    pub fn output_type(self, input: Option<ColumnType>) -> Option<ColumnType> {
        use ColumnType as C;
        match (self, input) {
            (Self::Count, None) => Some(C::I64),
            (Self::CountNonNull, Some(_)) => Some(C::I64),
            (Self::Sum, Some(C::I64)) => Some(C::I64),
            (Self::Sum, Some(C::F64)) => Some(C::F64),
            (Self::Mean | Self::Median, Some(C::I64 | C::F64)) => Some(C::F64),
            (
                Self::Min | Self::Max,
                Some(t @ (C::I64 | C::F64 | C::Timestamp | C::DictUtf8 | C::Bool)),
            ) => Some(t),
            _ => None,
        }
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct SortKey {
    pub column: String,
    pub descending: bool,
}

impl SortKey {
    pub fn asc(column: &str) -> Self {
        Self {
            column: column.into(),
            descending: false,
        }
    }
    pub fn desc(column: &str) -> Self {
        Self {
            column: column.into(),
            descending: true,
        }
    }
}

/// A logical plan: a DAG of operators, listed in topological order, and the
/// node whose output is the result.
#[derive(Clone, PartialEq, Debug)]
pub struct Plan {
    pub nodes: Vec<Op>,
    pub output: NodeId,
}

impl Plan {
    /// Starts a linear plan from a scan. Each builder call appends a node
    /// reading the previous one and makes it the output.
    pub fn scan(snapshot: ContentHash) -> Self {
        Self {
            nodes: vec![Op::Scan { snapshot }],
            output: NodeId(0),
        }
    }

    fn push(mut self, op: impl FnOnce(NodeId) -> Op) -> Self {
        let op = op(self.output);
        self.nodes.push(op);
        self.output = NodeId(self.nodes.len() as u32 - 1);
        self
    }

    pub fn filter(self, predicate: Expr) -> Self {
        self.push(|input| Op::Filter { input, predicate })
    }
    pub fn map(self, name: &str, expr: Expr) -> Self {
        self.push(|input| Op::Map {
            input,
            name: name.into(),
            expr,
        })
    }
    pub fn project(self, columns: &[&str]) -> Self {
        self.push(|input| Op::Project {
            input,
            columns: columns.iter().map(|c| c.to_string()).collect(),
        })
    }
    pub fn aggregate(self, group_by: &[&str], aggregates: Vec<Aggregate>) -> Self {
        self.push(|input| Op::Aggregate {
            input,
            group_by: group_by.iter().map(|c| c.to_string()).collect(),
            aggregates,
        })
    }
    pub fn sort(self, keys: Vec<SortKey>) -> Self {
        self.push(|input| Op::Sort { input, keys })
    }
    pub fn limit(self, count: u64) -> Self {
        self.push(|input| Op::Limit { input, count })
    }
}

/// What the plan layer needs to know about a snapshot.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct SourceInfo {
    /// Short human name, e.g. "NYC 311 Service Requests".
    pub title: String,
    pub schema: Schema,
}

/// Resolves the snapshots that `Scan` nodes name.
pub trait Catalog {
    fn source(&self, snapshot: &ContentHash) -> Option<SourceInfo>;
}

/// Why a plan is invalid, in plain English, and at which node.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct PlanError {
    pub node: Option<NodeId>,
    pub message: String,
}

impl PlanError {
    pub(crate) fn at(node: usize, message: impl Into<String>) -> Self {
        Self {
            node: Some(NodeId(node as u32)),
            message: message.into(),
        }
    }
    pub(crate) fn plan(message: impl Into<String>) -> Self {
        Self {
            node: None,
            message: message.into(),
        }
    }
}

impl fmt::Display for PlanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.node {
            Some(n) => write!(f, "step {}: {}", n.0, self.message),
            None => f.write_str(&self.message),
        }
    }
}

impl std::error::Error for PlanError {}

/// A plan that passed validation, with every node's output schema and
/// content hash. Only a `ValidPlan` can be executed.
#[derive(Clone, PartialEq, Debug)]
pub struct ValidPlan {
    plan: Plan,
    schemas: Vec<Schema>,
    sources: Vec<Option<SourceInfo>>,
    hashes: Vec<ContentHash>,
}

impl ValidPlan {
    pub fn plan(&self) -> &Plan {
        &self.plan
    }
    pub fn nodes(&self) -> &[Op] {
        &self.plan.nodes
    }
    pub fn output(&self) -> NodeId {
        self.plan.output
    }
    pub fn schema(&self, node: NodeId) -> &Schema {
        &self.schemas[node.index()]
    }
    /// The source a `Scan` node reads; `None` for other nodes.
    pub fn source(&self, node: NodeId) -> Option<&SourceInfo> {
        self.sources[node.index()].as_ref()
    }
    /// Content hash of the sub-plan ending at `node` (see `crate::hash`).
    pub fn node_hash(&self, node: NodeId) -> ContentHash {
        self.hashes[node.index()]
    }
    /// The plan's identity: the hash of its output node.
    pub fn plan_hash(&self) -> ContentHash {
        self.node_hash(self.plan.output)
    }
}

/// Checks structure, names and types, and computes schemas and hashes.
pub fn validate(plan: Plan, catalog: &dyn Catalog) -> Result<ValidPlan, PlanError> {
    let n = plan.nodes.len();
    if n == 0 {
        return Err(PlanError::plan("the plan has no steps"));
    }
    if n > u32::MAX as usize {
        return Err(PlanError::plan("the plan has too many steps"));
    }
    if plan.output.index() >= n {
        return Err(PlanError::plan(format!(
            "the output step {} doesn't exist",
            plan.output.0
        )));
    }
    let mut schemas: Vec<Schema> = Vec::with_capacity(n);
    let mut sources = Vec::with_capacity(n);
    for (i, op) in plan.nodes.iter().enumerate() {
        if let Some(input) = op.input()
            && input.index() >= i
        {
            return Err(PlanError::at(
                i,
                format!("reads step {}, which doesn't come before it", input.0),
            ));
        }
        let input_schema = op.input().map(|id| &schemas[id.index()]);
        let (schema, source) =
            node_schema(op, input_schema, catalog).map_err(|m| PlanError::at(i, m))?;
        schemas.push(schema);
        sources.push(source);
    }
    // Every step must contribute to the output, so that the plan hash (which
    // only covers the output's ancestors) covers the whole plan.
    let mut used = vec![false; n];
    used[plan.output.index()] = true;
    for i in (0..n).rev() {
        if used[i]
            && let Some(input) = plan.nodes[i].input()
        {
            used[input.index()] = true;
        }
    }
    if let Some(unused) = used.iter().position(|u| !u) {
        return Err(PlanError::at(
            unused,
            "this step doesn't lead to the output",
        ));
    }
    let hashes = crate::hash::node_hashes(&plan);
    Ok(ValidPlan {
        plan,
        schemas,
        sources,
        hashes,
    })
}

fn node_schema(
    op: &Op,
    input: Option<&Schema>,
    catalog: &dyn Catalog,
) -> Result<(Schema, Option<SourceInfo>), String> {
    let input = || input.expect("non-scan ops have an input");
    let field_of = |name: &str| {
        input()
            .field(name)
            .ok_or_else(|| format!("there is no column named \"{name}\""))
    };
    let schema = match op {
        Op::Scan { snapshot } => {
            let info = catalog
                .source(snapshot)
                .ok_or_else(|| format!("snapshot {} is not loaded", snapshot.to_hex()))?;
            check_unique(info.schema.fields.iter().map(|f| f.name.as_str()))?;
            return Ok((info.schema.clone(), Some(info)));
        }
        Op::Filter { predicate, .. } => {
            let t = predicate.type_of(input())?;
            if t.ty != Some(ValueType::Bool) {
                return Err(format!(
                    "the filter condition must be true/false, not {}",
                    type_name(t)
                ));
            }
            input().clone()
        }
        Op::Map { name, expr, .. } => {
            if name.is_empty() {
                return Err("the new column needs a name".into());
            }
            if input().field(name).is_some() {
                return Err(format!(
                    "there is already a column named \"{name}\"; choose another name"
                ));
            }
            let t = expr.type_of(input())?;
            let Some(ty) = t.ty else {
                return Err("a column can't be always missing".into());
            };
            let mut s = input().clone();
            s.fields
                .push(Field::new(name.clone(), ty.column_type(), t.nullable));
            s
        }
        Op::Project { columns, .. } => {
            if columns.is_empty() {
                return Err("keep at least one column".into());
            }
            check_unique(columns.iter().map(String::as_str))?;
            let fields = columns
                .iter()
                .map(|c| field_of(c).cloned())
                .collect::<Result<_, _>>()?;
            Schema::new(fields)
        }
        Op::Aggregate {
            group_by,
            aggregates,
            ..
        } => {
            if aggregates.is_empty() && group_by.is_empty() {
                return Err(
                    "group by at least one column or compute at least one aggregate".into(),
                );
            }
            check_unique(
                group_by
                    .iter()
                    .map(String::as_str)
                    .chain(aggregates.iter().map(|a| a.name.as_str())),
            )?;
            let mut fields = Vec::new();
            for g in group_by {
                let f = field_of(g)?;
                if f.ty == ColumnType::Geo {
                    return Err(format!("can't group by the location column \"{g}\""));
                }
                fields.push(f.clone());
            }
            for a in aggregates {
                if a.name.is_empty() {
                    return Err("every aggregate needs a name".into());
                }
                let input_ty = match &a.column {
                    Some(c) => Some(field_of(c)?.ty),
                    None => None,
                };
                let ty =
                    a.func
                        .output_type(input_ty)
                        .ok_or_else(|| match (&a.column, input_ty) {
                            (None, _) => format!("{} needs a column", a.func.name()),
                            (Some(_), _) if a.func == AggFunc::Count => {
                                "count counts rows and takes no column; use count_non_null"
                                    .to_string()
                            }
                            (Some(c), Some(t)) => format!(
                                "can't take the {} of \"{c}\", a {} column",
                                a.func.name(),
                                ValueType::of_column(t).name()
                            ),
                            (Some(_), None) => unreachable!(),
                        })?;
                let nullable = !matches!(a.func, AggFunc::Count | AggFunc::CountNonNull);
                fields.push(Field::new(a.name.clone(), ty, nullable));
            }
            Schema::new(fields)
        }
        Op::Sort { keys, .. } => {
            if keys.is_empty() {
                return Err("sort needs at least one column".into());
            }
            for k in keys {
                if field_of(&k.column)?.ty == ColumnType::Geo {
                    return Err(format!(
                        "can't sort by the location column \"{}\"",
                        k.column
                    ));
                }
            }
            input().clone()
        }
        Op::Limit { .. } => input().clone(),
    };
    Ok((schema, None))
}

fn type_name(t: ExprType) -> &'static str {
    t.ty.map_or("missing", ValueType::name)
}

fn check_unique<'a>(names: impl Iterator<Item = &'a str>) -> Result<(), String> {
    let mut seen = std::collections::HashSet::new();
    for n in names {
        if !seen.insert(n) {
            return Err(format!("the column name \"{n}\" is used twice"));
        }
    }
    Ok(())
}
