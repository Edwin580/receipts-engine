//! LineageStore: backward (output -> inputs) and forward (input -> outputs)
//! indexes, composition through the plan DAG, and the composed-trace LRU.
//! See `docs/engine/lineage.md`.
//!
//! Each executed step records how its output rows derive from its input's
//! rows ([`StepLineage`]). Composing those per-step mappings answers "which
//! source rows produced this number?" (backward) and "which results does
//! this source row feed into?" (forward), without re-running the plan.

mod lru;

use receipts_core::{RowId, SourceId};
use std::sync::{Arc, Mutex, OnceLock};

/// How one step's output rows derive from its input's rows.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum StepLineage {
    /// A scan: output row `i` is row `i` of the snapshot `source`.
    Source { source: SourceId, len: u32 },
    /// A scan of part of a snapshot of `source_len` rows: output row `i` is
    /// snapshot row `rows[i]`. Counterfactuals (M3) scan a snapshot minus
    /// the excluded rows this way, so traces still name original rows.
    SourceSubset {
        source: SourceId,
        source_len: u32,
        rows: Vec<u32>,
    },
    /// Output row `i` is input row `i`, for `i < len` (map, project, and
    /// limit, which keeps a prefix).
    Identity { len: u32 },
    /// Output row `i` is input row `rows[i]` (filter, sort).
    Select { rows: Vec<u32> },
    /// Output row `g` combines input rows `rows[offsets[g]..offsets[g + 1]]`,
    /// ascending (aggregate). An input row belongs to at most one group:
    /// after an exclusion (M3), excluded rows belong to none.
    Group { offsets: Vec<u32>, rows: Vec<u32> },
}

impl StepLineage {
    /// Number of output rows.
    pub fn len(&self) -> usize {
        match self {
            Self::Source { len, .. } | Self::Identity { len } => *len as usize,
            Self::Select { rows } | Self::SourceSubset { rows, .. } => rows.len(),
            Self::Group { offsets, .. } => offsets.len() - 1,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Approximate heap bytes, for memory accounting.
    pub fn heap_bytes(&self) -> usize {
        match self {
            Self::Source { .. } | Self::Identity { .. } => 0,
            Self::Select { rows } | Self::SourceSubset { rows, .. } => rows.len() * 4,
            Self::Group { offsets, rows } => (offsets.len() + rows.len()) * 4,
        }
    }
}

/// The input rows behind a set of output rows, at every step down to the
/// source. `path[0]` is the queried step; the last entry is the scan.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Trace {
    pub path: Vec<TraceStep>,
    pub source: SourceId,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct TraceStep {
    pub node: usize,
    /// Sorted, distinct row indices into that step's output.
    pub rows: Vec<u32>,
}

impl Trace {
    /// Row indices in the snapshot, sorted.
    pub fn source_rows(&self) -> &[u32] {
        &self.path.last().expect("a trace ends at a scan").rows
    }

    pub fn row_ids(&self) -> impl ExactSizeIterator<Item = RowId> + '_ {
        let source = self.source;
        self.source_rows()
            .iter()
            .map(move |&r| RowId::new(source, r))
    }
}

/// Maps an input row to the output rows it appears in; built on first use.
#[derive(Debug)]
enum Inverse {
    /// `out[input_row]`, or `u32::MAX` if the row was dropped.
    Select(Vec<u32>),
    /// Group (output row) of every input row.
    Group(Vec<u32>),
}

/// All step lineages of one execution, with their DAG edges.
#[derive(Debug)]
pub struct LineageStore {
    steps: Vec<StepLineage>,
    inputs: Vec<Option<usize>>,
    input_lens: Vec<usize>,
    inverses: Vec<OnceLock<Inverse>>,
    cache: Mutex<lru::Lru<(usize, u32), Arc<Trace>>>,
}

/// Composed single-row traces kept for repeated clicks on the same number.
pub const TRACE_CACHE_ENTRIES: usize = 256;

impl LineageStore {
    /// `inputs[i]` is the step that step `i` reads (`None` for scans), always
    /// an earlier step.
    ///
    /// # Panics
    /// If the steps and edges are inconsistent: a scan with an input, a
    /// non-scan without one, a forward edge, or a mapping that points past
    /// its input's rows.
    pub fn new(steps: Vec<StepLineage>, inputs: Vec<Option<usize>>) -> Self {
        assert_eq!(steps.len(), inputs.len(), "one input entry per step");
        let mut input_lens = Vec::with_capacity(steps.len());
        for (i, (step, input)) in steps.iter().zip(&inputs).enumerate() {
            let in_len = match (step, input) {
                (StepLineage::Source { len, .. }, None) => *len as usize,
                (StepLineage::SourceSubset { source_len, .. }, None) => *source_len as usize,
                (StepLineage::Source { .. } | StepLineage::SourceSubset { .. }, Some(_)) => {
                    panic!("step {i}: a scan has no input")
                }
                (_, None) => panic!("step {i}: only scans have no input"),
                (_, Some(j)) => {
                    assert!(*j < i, "step {i} reads a later step {j}");
                    steps[*j].len()
                }
            };
            let ok = match step {
                StepLineage::Source { .. } => true,
                StepLineage::Identity { len } => *len as usize <= in_len,
                StepLineage::Select { rows } | StepLineage::SourceSubset { rows, .. } => {
                    rows.iter().all(|&r| (r as usize) < in_len)
                }
                StepLineage::Group { offsets, rows } => {
                    offsets.first() == Some(&0)
                        && offsets.last().is_some_and(|&l| l as usize == rows.len())
                        && offsets.windows(2).all(|w| w[0] <= w[1])
                        && distinct_below(rows, in_len)
                }
            };
            assert!(
                ok,
                "step {i}: lineage doesn't fit its input of {in_len} rows"
            );
            input_lens.push(in_len);
        }
        let n = steps.len();
        Self {
            steps,
            inputs,
            input_lens,
            inverses: (0..n).map(|_| OnceLock::new()).collect(),
            cache: Mutex::new(lru::Lru::new(TRACE_CACHE_ENTRIES)),
        }
    }

    pub fn step(&self, node: usize) -> &StepLineage {
        &self.steps[node]
    }

    /// The step that `node` reads, or `None` for a scan.
    pub fn input(&self, node: usize) -> Option<usize> {
        self.inputs[node]
    }

    /// The snapshot row behind row `row` of `node`, if every step between
    /// them maps rows one to one (no aggregate in between).
    pub fn source_row(&self, node: usize, row: u32) -> Option<u32> {
        let (mut at, mut row) = (node, row);
        loop {
            match &self.steps[at] {
                StepLineage::Source { .. } => return Some(row),
                StepLineage::SourceSubset { rows, .. } => return Some(rows[row as usize]),
                StepLineage::Identity { .. } => {}
                StepLineage::Select { rows } => row = rows[row as usize],
                StepLineage::Group { .. } => return None,
            }
            at = self.inputs[at].expect("non-scan steps have an input");
        }
    }

    pub fn len(&self) -> usize {
        self.steps.len()
    }

    pub fn is_empty(&self) -> bool {
        self.steps.is_empty()
    }

    /// Heap bytes of the per-step mappings (not counting lazily built
    /// inverses or the trace cache).
    pub fn heap_bytes(&self) -> usize {
        self.steps.iter().map(StepLineage::heap_bytes).sum()
    }

    /// The input rows that the given output rows of `node` derive from.
    fn step_back(&self, node: usize, rows: &[u32]) -> Vec<u32> {
        let mut out: Vec<u32> = match &self.steps[node] {
            StepLineage::Source { .. } | StepLineage::SourceSubset { .. } => {
                unreachable!("scans are the end of a trace")
            }
            StepLineage::Identity { .. } => return rows.to_vec(),
            StepLineage::Select { rows: map } => rows.iter().map(|&r| map[r as usize]).collect(),
            StepLineage::Group {
                offsets,
                rows: members,
            } => rows
                .iter()
                .flat_map(|&g| {
                    let g = g as usize;
                    members[offsets[g] as usize..offsets[g + 1] as usize]
                        .iter()
                        .copied()
                })
                .collect(),
        };
        out.sort_unstable();
        out.dedup();
        out
    }

    /// Backward trace of a set of output rows of `node`, down to the source.
    ///
    /// # Panics
    /// If a row is out of range for that step.
    pub fn backward(&self, node: usize, rows: &[u32]) -> Trace {
        let len = self.steps[node].len();
        assert!(
            rows.iter().all(|&r| (r as usize) < len),
            "row out of range for step {node} ({len} rows)"
        );
        let mut current: Vec<u32> = rows.to_vec();
        current.sort_unstable();
        current.dedup();
        let mut path = Vec::new();
        let mut at = node;
        loop {
            let step = TraceStep {
                node: at,
                rows: current,
            };
            match &self.steps[at] {
                StepLineage::Source { source, .. } => {
                    path.push(step);
                    return Trace {
                        path,
                        source: *source,
                    };
                }
                // The scan's entry lists snapshot rows, not positions in
                // the subset.
                StepLineage::SourceSubset { source, rows, .. } => {
                    let mut mapped: Vec<u32> =
                        step.rows.iter().map(|&r| rows[r as usize]).collect();
                    mapped.sort_unstable();
                    path.push(TraceStep {
                        node: at,
                        rows: mapped,
                    });
                    return Trace {
                        path,
                        source: *source,
                    };
                }
                _ => {}
            }
            current = self.step_back(at, &step.rows);
            path.push(step);
            at = self.inputs[at].expect("non-scan steps have an input");
        }
    }

    /// Backward trace of one output row, cached: clicking the same number
    /// twice doesn't recompose the trace.
    pub fn backward_row(&self, node: usize, row: u32) -> Arc<Trace> {
        let key = (node, row);
        if let Some(t) = self.cache.lock().expect("cache lock").get(&key) {
            return t;
        }
        let trace = Arc::new(self.backward(node, &[row]));
        self.cache
            .lock()
            .expect("cache lock")
            .insert(key, trace.clone());
        trace
    }

    fn inverse(&self, node: usize) -> &Inverse {
        self.inverses[node].get_or_init(|| {
            let in_len = self.input_lens[node];
            match &self.steps[node] {
                StepLineage::Select { rows } | StepLineage::SourceSubset { rows, .. } => {
                    let mut inv = vec![u32::MAX; in_len];
                    for (out, &r) in rows.iter().enumerate() {
                        inv[r as usize] = out as u32;
                    }
                    Inverse::Select(inv)
                }
                StepLineage::Group { offsets, rows } => {
                    let mut group_of = vec![u32::MAX; in_len];
                    for g in 0..offsets.len() - 1 {
                        for &r in &rows[offsets[g] as usize..offsets[g + 1] as usize] {
                            group_of[r as usize] = g as u32;
                        }
                    }
                    Inverse::Group(group_of)
                }
                StepLineage::Source { .. } | StepLineage::Identity { .. } => {
                    unreachable!("identity-like steps need no inverse")
                }
            }
        })
    }

    /// The output rows of `node` that the given rows of the scan feeding it
    /// flow into. Rows that a filter or limit dropped reach nothing.
    ///
    /// # Panics
    /// If `node` isn't downstream of a scan `scan` (or is the scan itself,
    /// which returns the rows unchanged).
    pub fn forward(&self, scan: usize, source_rows: &[u32], node: usize) -> Vec<u32> {
        assert!(
            matches!(
                self.steps[scan],
                StepLineage::Source { .. } | StepLineage::SourceSubset { .. }
            ),
            "step {scan} is not a scan"
        );
        // Chain from the scan up to `node`.
        let mut chain = vec![node];
        let mut at = node;
        while at != scan {
            at = self.inputs[at].unwrap_or_else(|| panic!("step {node} doesn't read step {scan}"));
            chain.push(at);
        }
        chain.reverse();
        let mut current: Vec<u32> = source_rows.to_vec();
        current.sort_unstable();
        current.dedup();
        // Rows past the end of the snapshot reach nothing.
        if let StepLineage::Source { len, .. } = self.steps[scan] {
            current.retain(|&r| r < len);
        }
        if matches!(self.steps[scan], StepLineage::SourceSubset { .. }) {
            // Snapshot rows to positions in the subset; excluded rows reach
            // nothing.
            let Inverse::Select(inv) = self.inverse(scan) else {
                unreachable!()
            };
            current = current
                .iter()
                .filter_map(|&r| inv.get(r as usize).copied())
                .filter(|&p| p != u32::MAX)
                .collect();
            current.sort_unstable();
        }
        for &step in &chain[1..] {
            current = match &self.steps[step] {
                StepLineage::Source { .. } | StepLineage::SourceSubset { .. } => unreachable!(),
                StepLineage::Identity { len } => {
                    current.retain(|&r| r < *len);
                    current
                }
                StepLineage::Select { .. } | StepLineage::Group { .. } => {
                    let map = match self.inverse(step) {
                        Inverse::Select(v) | Inverse::Group(v) => v,
                    };
                    let mut next: Vec<u32> = current
                        .iter()
                        .map(|&r| map[r as usize])
                        .filter(|&o| o != u32::MAX)
                        .collect();
                    next.sort_unstable();
                    next.dedup();
                    next
                }
            };
        }
        current
    }
}

/// Every entry is below `bound` and appears once.
fn distinct_below(rows: &[u32], bound: usize) -> bool {
    let mut seen = vec![false; bound];
    rows.iter()
        .all(|&r| (r as usize) < bound && !std::mem::replace(&mut seen[r as usize], true))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// scan(6) -> filter keeps [1,2,4,5] -> sort to [5,1,4,2] -> group:
    /// g0 = {0,2} (rows 5 and 4 of the scan), g1 = {1,3} (rows 1 and 2) -> limit 1.
    fn store() -> LineageStore {
        LineageStore::new(
            vec![
                StepLineage::Source {
                    source: SourceId(1),
                    len: 6,
                },
                StepLineage::Select {
                    rows: vec![1, 2, 4, 5],
                },
                StepLineage::Select {
                    rows: vec![3, 0, 2, 1],
                },
                StepLineage::Group {
                    offsets: vec![0, 2, 4],
                    rows: vec![0, 2, 1, 3],
                },
                StepLineage::Identity { len: 1 },
            ],
            vec![None, Some(0), Some(1), Some(2), Some(3)],
        )
    }

    #[test]
    fn backward_composes_every_step() {
        let s = store();
        let t = s.backward(4, &[0]);
        let rows: Vec<_> = t.path.iter().map(|p| (p.node, p.rows.clone())).collect();
        assert_eq!(
            rows,
            vec![
                (4, vec![0]),
                (3, vec![0]),
                (2, vec![0, 2]),
                (1, vec![2, 3]),
                (0, vec![4, 5]),
            ]
        );
        assert_eq!(
            t.row_ids().collect::<Vec<_>>(),
            vec![RowId::new(SourceId(1), 4), RowId::new(SourceId(1), 5)]
        );
        assert_eq!(s.backward(3, &[1]).source_rows(), &[1, 2]);
        assert_eq!(s.backward(3, &[]).source_rows(), &[] as &[u32]);
    }

    #[test]
    fn forward_inverts_backward() {
        let s = store();
        assert_eq!(s.forward(0, &[5], 3), vec![0]);
        assert_eq!(s.forward(0, &[1, 5], 3), vec![0, 1]);
        assert_eq!(s.forward(0, &[0, 3], 3), Vec::<u32>::new()); // filtered out
        assert_eq!(s.forward(0, &[1], 4), Vec::<u32>::new()); // group 1 cut by limit
        assert_eq!(s.forward(0, &[4], 4), vec![0]);
        assert_eq!(s.forward(0, &[4, 2], 0), vec![2, 4]);
        // Exhaustive: r reaches o forward iff o's backward trace contains r.
        for node in 0..5 {
            for r in 0..6u32 {
                let fwd = s.forward(0, &[r], node);
                let expect: Vec<u32> = (0..s.step(node).len() as u32)
                    .filter(|&o| s.backward(node, &[o]).source_rows().contains(&r))
                    .collect();
                assert_eq!(fwd, expect, "node {node} row {r}");
            }
        }
    }

    #[test]
    fn single_row_traces_are_cached() {
        let s = store();
        let a = s.backward_row(4, 0);
        let b = s.backward_row(4, 0);
        assert!(Arc::ptr_eq(&a, &b));
        assert_eq!(*a, s.backward(4, &[0]));
    }

    #[test]
    fn subsets_and_partial_groups_trace_original_rows() {
        // Snapshot of 5 rows minus rows 1 and 3; one group of the rest
        // except subset position 0 (snapshot row 0), which was excluded
        // further down.
        let s = LineageStore::new(
            vec![
                StepLineage::SourceSubset {
                    source: SourceId(2),
                    source_len: 5,
                    rows: vec![0, 2, 4],
                },
                StepLineage::Group {
                    offsets: vec![0, 2],
                    rows: vec![1, 2],
                },
            ],
            vec![None, Some(0)],
        );
        assert_eq!(s.backward(1, &[0]).source_rows(), &[2, 4]);
        assert_eq!(s.backward(0, &[0, 2]).source_rows(), &[0, 4]);
        assert_eq!(s.forward(0, &[4], 1), vec![0]);
        assert_eq!(s.forward(0, &[0], 1), Vec::<u32>::new()); // in no group
        assert_eq!(s.forward(0, &[1, 3], 1), Vec::<u32>::new()); // not in the subset
        assert_eq!(s.forward(0, &[3, 4], 0), vec![2]);
    }

    #[test]
    #[should_panic(expected = "doesn't fit")]
    fn rejects_duplicate_group_members() {
        LineageStore::new(
            vec![
                StepLineage::Source {
                    source: SourceId(1),
                    len: 3,
                },
                StepLineage::Group {
                    offsets: vec![0, 2],
                    rows: vec![1, 1],
                },
            ],
            vec![None, Some(0)],
        );
    }

    #[test]
    #[should_panic(expected = "doesn't fit")]
    fn rejects_inconsistent_lineage() {
        LineageStore::new(
            vec![
                StepLineage::Source {
                    source: SourceId(1),
                    len: 2,
                },
                StepLineage::Select { rows: vec![2] },
            ],
            vec![None, Some(0)],
        );
    }
}
