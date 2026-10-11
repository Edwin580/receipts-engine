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
///
/// Each key column is first encoded as integers whose order is the
/// column's order (missing smallest, reversed for descending). The row
/// number goes last, so every entry is distinct: an unstable sort of the
/// encoded entries gives exactly the stable order, without calling a
/// comparator per comparison.
pub(crate) fn sort_permutation(n: usize, keys: &[(&Column, bool)]) -> Vec<u32> {
    fn sort<T: Ord + Send>(v: &mut [T]) {
        #[cfg(feature = "parallel")]
        rayon::slice::ParallelSliceMut::par_sort_unstable(v);
        #[cfg(not(feature = "parallel"))]
        v.sort_unstable();
    }
    let Some(enc) = keys
        .iter()
        .map(|(c, desc)| sort_keys(n, c, *desc))
        .collect::<Option<Vec<Vec<u64>>>>()
    else {
        return sort_by_comparators(n, keys);
    };
    match &enc[..] {
        [] => (0..n as u32).collect(),
        [a] => {
            let mut v: Vec<(u64, u32)> = (0..n).map(|i| (a[i], i as u32)).collect();
            sort(&mut v);
            v.into_iter().map(|e| e.1).collect()
        }
        [a, b] => {
            let mut v: Vec<(u64, u64, u32)> = (0..n).map(|i| (a[i], b[i], i as u32)).collect();
            sort(&mut v);
            v.into_iter().map(|e| e.2).collect()
        }
        _ => {
            let mut v: Vec<(Vec<u64>, u32)> = (0..n)
                .map(|i| (enc.iter().map(|k| k[i]).collect(), i as u32))
                .collect();
            sort(&mut v);
            v.into_iter().map(|e| e.1).collect()
        }
    }
}

/// One integer per row whose unsigned order is the order `row_cmp` gives,
/// or its reverse for `descending`. Missing values encode as 0, which no
/// present value uses. `None` for the one column this can't encode: an
/// integer or timestamp column with missing values that also holds
/// `i64::MIN`.
fn sort_keys(n: usize, col: &Column, descending: bool) -> Option<Vec<u64>> {
    let present = |i: usize| col.is_valid(i);
    let mut out: Vec<u64> = match &col.data {
        ColumnData::I64(v) | ColumnData::Timestamp(v) => {
            if col.validity.is_some() && (0..n).any(|i| present(i) && v[i] == i64::MIN) {
                return None;
            }
            // Flipping the sign bit orders i64 as u64; only i64::MIN maps to 0.
            (0..n)
                .map(|i| {
                    if present(i) {
                        v[i] as u64 ^ (1 << 63)
                    } else {
                        0
                    }
                })
                .collect()
        }
        ColumnData::F64(v) => (0..n)
            .map(|i| {
                if !present(i) {
                    return 0;
                }
                // Decimal columns hold no NaN or infinity, so no value maps
                // to 0; -0.0 equals 0.0, as in cmp_f64.
                let bits = if v[i] == 0.0 { 0 } else { v[i].to_bits() };
                if bits >> 63 == 1 {
                    !bits
                } else {
                    bits | (1 << 63)
                }
            })
            .collect(),
        ColumnData::Bool(b) => (0..n)
            .map(|i| if present(i) { 1 + b.get(i) as u64 } else { 0 })
            .collect(),
        ColumnData::DictUtf8 { codes, .. } => (0..n)
            .map(|i| if present(i) { 1 + codes[i] as u64 } else { 0 })
            .collect(),
        ColumnData::Geo { .. } => unreachable!("validated: locations aren't ordered"),
    };
    if descending {
        out.iter_mut().for_each(|k| *k = !*k);
    }
    Some(out)
}

/// The general stable sort, for keys `sort_keys` can't encode.
fn sort_by_comparators(n: usize, keys: &[(&Column, bool)]) -> Vec<u32> {
    let cmps: Vec<_> = keys.iter().map(|(c, desc)| (row_cmp(c), *desc)).collect();
    let mut perm: Vec<u32> = (0..n as u32).collect();
    perm.sort_by(|&a, &b| {
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
        AggFunc::Percentile(p) => {
            // Bucket row numbers by group, sort each bucket by value (stable,
            // so ties keep row order), and take the value at the rank.
            let cmp = row_cmp(col);
            let mut start = vec![0usize; g + 1];
            for i in valid_rows() {
                start[ids[i] as usize + 1] += 1;
            }
            for k in 0..g {
                start[k + 1] += start[k];
            }
            let mut fill = start.clone();
            let mut rows = vec![0u32; start[g]];
            for i in valid_rows() {
                let k = ids[i] as usize;
                rows[fill[k]] = i as u32;
                fill[k] += 1;
            }
            let picked: Vec<Option<u32>> = (0..g)
                .map(|k| {
                    let bucket = &mut rows[start[k]..start[k + 1]];
                    bucket.sort_by(|&a, &b| cmp(a as usize, b as usize));
                    percentile_rank(bucket.len(), p).map(|r| bucket[r])
                })
                .collect();
            gather_opt(col, picked.iter().copied(), g)
        }
    })
}

/// Index into `n` sorted values of their nearest-rank `p`th percentile:
/// the smallest value at least `p` percent of them are at or below.
pub fn percentile_rank(n: usize, p: u8) -> Option<usize> {
    (n > 0).then(|| (n * p as usize).div_ceil(100).max(1) - 1)
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

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use receipts_core::Bitmap;

    fn column(data: ColumnData, valid: &[bool]) -> Column {
        let validity = valid
            .contains(&false)
            .then(|| Bitmap::from_bools(valid.iter().copied()));
        Column::new("c", data, validity)
    }

    const INTS: [i64; 6] = [i64::MIN, -1, 0, 1, 7, i64::MAX];
    const FLOATS: [f64; 7] = [-f64::MAX, -1.5, -0.0, 0.0, 1e-300, 2.0, f64::MAX];

    proptest! {
        // The encoded sort must give exactly the comparator sort's order,
        // including missing values, -0.0 == 0.0 and the extremes.
        #[test]
        fn encoded_sort_equals_comparator_sort(
            rows in prop::collection::vec((0usize..6, 0usize..7, 0u32..4, any::<bool>(), any::<bool>()), 0..60),
            nullable in any::<bool>(),
            keys in prop::collection::vec((0usize..4, any::<bool>()), 1..4),
        ) {
            let n = rows.len();
            let valid: Vec<bool> = rows.iter().map(|r| !nullable || r.4).collect();
            let cols = [
                column(ColumnData::I64(rows.iter().map(|r| INTS[r.0]).collect()), &valid),
                column(ColumnData::F64(rows.iter().map(|r| FLOATS[r.1]).collect()), &valid),
                column(
                    ColumnData::DictUtf8 {
                        codes: rows.iter().map(|r| r.2).collect(),
                        dictionary: ["a", "b", "c", "d"].map(String::from).to_vec(),
                    },
                    &valid,
                ),
                column(ColumnData::Bool(Bitmap::from_bools(rows.iter().map(|r| r.3))), &valid),
            ];
            let keys: Vec<(&Column, bool)> = keys.iter().map(|&(c, d)| (&cols[c], d)).collect();
            prop_assert_eq!(sort_permutation(n, &keys), sort_by_comparators(n, &keys));
        }
    }

    #[test]
    fn integers_with_missing_values_and_i64_min_fall_back() {
        let c = column(ColumnData::I64(vec![5, i64::MIN, 0]), &[true, true, false]);
        assert!(sort_keys(3, &c, false).is_none());
        // Missing first, then i64::MIN.
        assert_eq!(sort_permutation(3, &[(&c, false)]), vec![2, 1, 0]);
        assert_eq!(sort_permutation(3, &[(&c, true)]), vec![0, 1, 2]);
    }
}
