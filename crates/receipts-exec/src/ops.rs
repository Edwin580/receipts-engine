//! Sort and aggregate. Both produce an explicit row mapping (a permutation,
//! or a group id per input row), which is what lineage capture (M2) records.

use crate::eval::cmp_f64;
use crate::table::{gather, gather_opt};
use receipts_core::{Column, ColumnData};
use receipts_lineage::StepLineage;
use receipts_plan::AggFunc;
use std::cmp::Ordering;
use std::collections::HashMap;

/// Row comparator for one column: missing values are smallest.
pub(crate) fn row_cmp(col: &Column) -> Box<dyn Fn(usize, usize) -> Ordering + Send + Sync + '_> {
    let values: Box<dyn Fn(usize, usize) -> Ordering + Send + Sync + '_> = match &col.data {
        ColumnData::I64(v) | ColumnData::Timestamp(v) => Box::new(move |a, b| v[a].cmp(&v[b])),
        ColumnData::F64(v) => Box::new(move |a, b| cmp_f64(v[a], v[b])),
        ColumnData::Bool(v) => Box::new(move |a, b| v.get(a).cmp(&v.get(b))),
        // Dictionaries are sorted, so code order is string order.
        ColumnData::DictUtf8 { codes, .. } => Box::new(move |a, b| codes[a].cmp(&codes[b])),
        ColumnData::Geo { .. } => unreachable!("validated: locations aren't ordered"),
    };
    if col.validity.is_none() {
        return values;
    }
    Box::new(move |a, b| match (col.is_valid(a), col.is_valid(b)) {
        (true, true) => values(a, b),
        (va, vb) => va.cmp(&vb),
    })
}

/// Stable sort permutation: `out[i]` is the input row placed at position `i`.
pub(crate) fn sort_permutation(n: usize, keys: &[(&Column, bool)]) -> Vec<u32> {
    let cmps: Vec<_> = keys.iter().map(|(c, desc)| (row_cmp(c), *desc)).collect();
    let mut perm: Vec<u32> = (0..n as u32).collect();
    // Both sorts are stable, so the permutation is the same either way.
    #[cfg(feature = "parallel")]
    use rayon::slice::ParallelSliceMut;
    #[cfg(feature = "parallel")]
    let sort = |p: &mut [u32], f: &(dyn Fn(&u32, &u32) -> Ordering + Sync)| p.par_sort_by(f);
    #[cfg(not(feature = "parallel"))]
    let sort = |p: &mut [u32], f: &(dyn Fn(&u32, &u32) -> Ordering + Sync)| p.sort_by(f);
    sort(&mut perm, &|&a, &b| {
        for (cmp, desc) in &cmps {
            let o = cmp(a as usize, b as usize);
            if o != Ordering::Equal {
                return if *desc { o.reverse() } else { o };
            }
        }
        Ordering::Equal
    });
    perm
}

/// The grouping of an aggregate's input.
pub(crate) struct Groups {
    /// Group of each input row.
    pub ids: Vec<u32>,
    /// Number of groups.
    pub count: usize,
    /// The first input row of each group, used to read its key values.
    pub first_row: Vec<u32>,
}

/// Assigns dense group ids, in first-seen order, to the distinct
/// combinations of the key columns. No keys means one group, even with no
/// rows.
pub(crate) fn group(n: usize, keys: &[&Column]) -> Groups {
    if keys.is_empty() {
        return Groups {
            ids: vec![0; n],
            count: 1,
            first_row: Vec::new(),
        };
    }
    let mut acc = Groups {
        ids: vec![0; n],
        count: 1,
        first_row: vec![0],
    };
    for key in keys {
        let (ids, card) = column_ids(key);
        acc = combine(&acc.ids, acc.count, &ids, card);
    }
    acc
}

/// Per-row id of the value (0 for missing) and an upper bound on the ids.
fn column_ids(col: &Column) -> (Vec<u32>, usize) {
    let n = col.len();
    fn intern<K: std::hash::Hash + Eq>(
        n: usize,
        col: &Column,
        key: impl Fn(usize) -> K,
    ) -> (Vec<u32>, usize) {
        let mut map: HashMap<K, u32> = HashMap::new();
        let ids = (0..n)
            .map(|i| {
                if !col.is_valid(i) {
                    return 0;
                }
                let next = map.len() as u32 + 1;
                *map.entry(key(i)).or_insert(next)
            })
            .collect();
        (ids, map.len() + 1)
    }
    match &col.data {
        ColumnData::DictUtf8 { codes, dictionary } => (
            (0..n)
                .map(|i| if col.is_valid(i) { codes[i] + 1 } else { 0 })
                .collect(),
            dictionary.len() + 1,
        ),
        ColumnData::Bool(b) => (
            (0..n)
                .map(|i| {
                    if col.is_valid(i) {
                        1 + b.get(i) as u32
                    } else {
                        0
                    }
                })
                .collect(),
            3,
        ),
        ColumnData::I64(v) | ColumnData::Timestamp(v) => intern(n, col, |i| v[i]),
        // -0.0 and 0.0 are the same group.
        ColumnData::F64(v) => intern(n, col, |i| (v[i] + 0.0).to_bits()),
        ColumnData::Geo { .. } => unreachable!("validated: no location group keys"),
    }
}

/// Dense ids for the pairs `(a[i], b[i])`, in first-seen order.
fn combine(a: &[u32], card_a: usize, b: &[u32], card_b: usize) -> Groups {
    let n = a.len();
    let mut ids = Vec::with_capacity(n);
    let mut first_row = Vec::new();
    let space = card_a as u64 * card_b as u64;
    if space <= 1 << 22 {
        // Small key space: a flat lookup table beats hashing.
        let mut table = vec![u32::MAX; space as usize];
        for i in 0..n {
            let slot = &mut table[a[i] as usize * card_b + b[i] as usize];
            if *slot == u32::MAX {
                *slot = first_row.len() as u32;
                first_row.push(i as u32);
            }
            ids.push(*slot);
        }
    } else {
        let mut map: HashMap<u64, u32> = HashMap::new();
        for i in 0..n {
            let key = a[i] as u64 * card_b as u64 + b[i] as u64;
            let id = *map.entry(key).or_insert_with(|| {
                first_row.push(i as u32);
                first_row.len() as u32 - 1
            });
            ids.push(id);
        }
    }
    Groups {
        count: first_row.len(),
        ids,
        first_row,
    }
}

/// Order of groups in the output: by key values, ascending, missing first.
pub(crate) fn group_order(groups: &Groups, keys: &[&Column]) -> Vec<u32> {
    let cmps: Vec<_> = keys.iter().map(|c| row_cmp(c)).collect();
    let mut order: Vec<u32> = (0..groups.count as u32).collect();
    order.sort_by(|&x, &y| {
        let (rx, ry) = (
            groups.first_row[x as usize] as usize,
            groups.first_row[y as usize] as usize,
        );
        cmps.iter()
            .map(|c| c(rx, ry))
            .find(|o| *o != Ordering::Equal)
            .unwrap_or(Ordering::Equal)
    });
    order
}

/// One output value per group, indexed by group id.
pub(crate) fn aggregate(
    func: AggFunc,
    input: Option<&Column>,
    groups: &Groups,
) -> Result<Column, String> {
    let g = groups.count;
    let ids = &groups.ids;
    let int_col = |v: Vec<i64>, valid: Option<Vec<bool>>| {
        let validity = valid.and_then(|v| crate::table::validity_from(g, |i| v[i]));
        Column::new("", ColumnData::I64(v), validity)
    };
    let f64_col = |v: Vec<Option<f64>>| {
        let validity = crate::table::validity_from(g, |i| v[i].is_some());
        Column::new(
            "",
            ColumnData::F64(v.iter().map(|x| x.unwrap_or(0.0)).collect()),
            validity,
        )
    };
    if func == AggFunc::Count {
        let mut counts = vec![0i64; g];
        for &id in ids {
            counts[id as usize] += 1;
        }
        return Ok(int_col(counts, None));
    }
    let col = input.expect("validated: only count takes no column");
    let valid_rows = || (0..ids.len()).filter(|&i| col.is_valid(i));
    Ok(match func {
        AggFunc::Count => unreachable!(),
        AggFunc::CountNonNull => {
            let mut counts = vec![0i64; g];
            for i in valid_rows() {
                counts[ids[i] as usize] += 1;
            }
            int_col(counts, None)
        }
        AggFunc::Sum | AggFunc::Mean => {
            let mut counts = vec![0u64; g];
            let out: Vec<Option<f64>> = match &col.data {
                ColumnData::I64(v) => {
                    // i128 can't overflow: at most 2^32 rows of |x| <= 2^63.
                    let mut sums = vec![0i128; g];
                    for i in valid_rows() {
                        sums[ids[i] as usize] += v[i] as i128;
                        counts[ids[i] as usize] += 1;
                    }
                    if func == AggFunc::Sum {
                        let mut values = Vec::with_capacity(g);
                        for s in &sums {
                            values.push(i64::try_from(*s).map_err(|_| {
                                format!("the sum {s} doesn't fit in a 64-bit integer")
                            })?);
                        }
                        let valid = counts.iter().map(|&c| c > 0).collect();
                        return Ok(int_col(values, Some(valid)));
                    }
                    sums.iter()
                        .zip(&counts)
                        .map(|(&s, &c)| (c > 0).then(|| s as f64 / c as f64))
                        .collect()
                }
                ColumnData::F64(v) => {
                    // Row order, so the result doesn't depend on grouping.
                    let mut sums = vec![0.0f64; g];
                    for i in valid_rows() {
                        sums[ids[i] as usize] += v[i];
                        counts[ids[i] as usize] += 1;
                    }
                    sums.iter()
                        .zip(&counts)
                        .map(|(&s, &c)| {
                            let r = if func == AggFunc::Sum {
                                s
                            } else {
                                s / c as f64
                            };
                            (c > 0 && r.is_finite()).then_some(r)
                        })
                        .collect()
                }
                _ => unreachable!("validated: numeric sum/mean"),
            };
            f64_col(out)
        }
        AggFunc::Median => {
            let value = |i: usize| match &col.data {
                ColumnData::I64(v) => v[i] as f64,
                ColumnData::F64(v) => v[i],
                _ => unreachable!("validated: numeric median"),
            };
            // Bucket values by group (counting sort), then sort each bucket.
            let mut start = vec![0usize; g + 1];
            for i in valid_rows() {
                start[ids[i] as usize + 1] += 1;
            }
            for k in 0..g {
                start[k + 1] += start[k];
            }
            let mut fill = start.clone();
            let mut values = vec![0.0f64; start[g]];
            for i in valid_rows() {
                let k = ids[i] as usize;
                values[fill[k]] = value(i);
                fill[k] += 1;
            }
            let out = (0..g)
                .map(|k| {
                    let bucket = &mut values[start[k]..start[k + 1]];
                    bucket.sort_by(|a, b| cmp_f64(*a, *b));
                    median_of_sorted(bucket)
                })
                .collect();
            f64_col(out)
        }
        AggFunc::Min | AggFunc::Max => {
            let cmp = row_cmp(col);
            let want = if func == AggFunc::Min {
                Ordering::Less
            } else {
                Ordering::Greater
            };
            let mut best: Vec<Option<u32>> = vec![None; g];
            for i in valid_rows() {
                let slot = &mut best[ids[i] as usize];
                // Strictly better only, so ties keep the first row.
                if slot.is_none_or(|b| cmp(i, b as usize) == want) {
                    *slot = Some(i as u32);
                }
            }
            gather_opt(col, best.iter().copied(), g)
        }
    })
}

/// The middle value, or the mean of the two middle values.
pub fn median_of_sorted(sorted: &[f64]) -> Option<f64> {
    let n = sorted.len();
    if n == 0 {
        return None;
    }
    if n % 2 == 1 {
        return Some(sorted[n / 2]);
    }
    let (a, b) = (sorted[n / 2 - 1], sorted[n / 2]);
    let m = (a + b) / 2.0;
    Some(if m.is_finite() { m } else { a / 2.0 + b / 2.0 })
}

/// Output columns for the groups, in `order`.
pub(crate) fn key_columns(keys: &[&Column], groups: &Groups, order: &[u32]) -> Vec<Column> {
    if keys.is_empty() {
        return Vec::new();
    }
    let rows: Vec<u32> = order
        .iter()
        .map(|&k| groups.first_row[k as usize])
        .collect();
    keys.iter().map(|c| gather(c, &rows)).collect()
}

/// Lineage of an aggregate: output row `p` (group `order[p]`) combines its
/// member rows, ascending.
pub(crate) fn group_lineage(groups: &Groups, order: &[u32]) -> StepLineage {
    let mut position = vec![0usize; groups.count];
    for (p, &g) in order.iter().enumerate() {
        position[g as usize] = p;
    }
    let mut offsets = vec![0u32; order.len() + 1];
    for &g in &groups.ids {
        offsets[position[g as usize] + 1] += 1;
    }
    for p in 0..order.len() {
        offsets[p + 1] += offsets[p];
    }
    let mut fill: Vec<u32> = offsets[..order.len()].to_vec();
    let mut rows = vec![0u32; groups.ids.len()];
    for (i, &g) in groups.ids.iter().enumerate() {
        let p = position[g as usize];
        rows[fill[p] as usize] = i as u32;
        fill[p] += 1;
    }
    StepLineage::Group { offsets, rows }
}
