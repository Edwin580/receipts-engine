//! Applies the cleaning rules to a raw directory and produces the sorted
//! snapshot columns, the cleaning log, and the rejects.
//!
//! Rules are evaluated in this order, and a rejected record gets exactly one
//! rule: CR-02 (key), then CR-03/CR-04 (duplicates, compared across *all*
//! records with a valid key, so a bad copy can't hide a conflict), then CR-05
//! (created time), then CR-12 (scope). Value rules (CR-06 to CR-09) are
//! applied while staging, and their log entries are kept only for admitted rows.

use crate::raw::RawDir;
use crate::rules::Rule;
use crate::spec::{DatasetSpec, Kind};
use anyhow::{Context, Result, bail, ensure};
use receipts_core::hash::ContentHasher;
use receipts_core::time::parse_naive_timestamp;
use receipts_core::{Bitmap, Column, ColumnData, ContentHash};
use serde::de::{Deserialize, Deserializer, MapAccess, Visitor};
use serde_json::value::RawValue;
use std::borrow::Cow;
use std::collections::HashMap;
use std::fmt;

pub const CLEANING_LOG_CONTEXT: &str = "receipts snapshot v1 cleaning-log";
pub const REJECTS_CONTEXT: &str = "receipts snapshot v1 rejects";

/// One value a cleaning rule changed.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct LogEntry {
    pub row_index: u32,
    pub column: u16,
    pub rule: Rule,
    /// The exact source text before the rule applied. For a location, a JSON
    /// array `[latitude, longitude]` of the raw values.
    pub raw_value: Option<String>,
}

/// One source record that was not admitted.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Reject {
    pub raw_unique_key: Option<String>,
    pub rule: Rule,
    pub raw_record: String,
}

#[derive(Debug)]
pub struct Cleaned {
    /// Admitted rows, sorted by (created, key), in spec column order.
    pub columns: Vec<Column>,
    /// Sorted by (row_index, column, rule id, raw_value).
    pub log: Vec<LogEntry>,
    /// Sorted by (raw_unique_key, rule id, raw_record); `None` keys first.
    pub rejects: Vec<Reject>,
    pub raw_records: u64,
}

impl Cleaned {
    pub fn row_count(&self) -> usize {
        self.columns.first().map_or(0, Column::len)
    }
}

pub fn clean(spec: &DatasetSpec, raw: &RawDir) -> Result<Cleaned> {
    let (lo, hi) = raw.fetch.scope.bounds()?;
    ensure!(
        raw.fetch.scope.column == spec.created_field(),
        "fetch scope is on {:?}, but the spec's created field is {:?}",
        raw.fetch.scope.column,
        spec.created_field()
    );
    let mut staging = Staging::new(spec);
    for (p, page) in raw.fetch.pages.iter().enumerate() {
        let body = raw.read_page(p)?;
        let records: Vec<&RawValue> = serde_json::from_slice(&body)
            .with_context(|| format!("{} is not a JSON array", page.file))?;
        ensure!(
            records.len() == page.records as usize,
            "{}: {} records, fetch.json says {}",
            page.file,
            records.len(),
            page.records
        );
        for (i, rec) in records.iter().enumerate() {
            staging
                .stage(spec, (p as u32, i as u32), rec)
                .with_context(|| format!("{} record {}", page.file, i + 1))?;
        }
    }
    staging.finish(spec, raw, lo, hi)
}

#[derive(Debug, Default)]
struct Interner {
    ids: HashMap<String, u32>,
    values: Vec<String>,
}

impl Interner {
    fn intern(&mut self, s: &str) -> u32 {
        if let Some(&id) = self.ids.get(s) {
            return id;
        }
        let id = self.values.len() as u32;
        self.ids.insert(s.to_string(), id);
        self.values.push(s.to_string());
        id
    }
}

#[derive(Debug)]
enum Stage {
    Key,
    Created,
    Ts {
        values: Vec<i64>,
        valid: Bitmap,
    },
    Text {
        ids: Vec<u32>,
        valid: Bitmap,
        interner: Interner,
    },
    Geo {
        lat: Vec<f32>,
        lon: Vec<f32>,
        valid: Bitmap,
    },
}

#[derive(Debug)]
struct PendingLog {
    row: u32,
    column: u16,
    rule: Rule,
    raw: Option<String>,
}

#[derive(Debug)]
struct PendingReject {
    loc: (u32, u32),
    rule: Rule,
    raw_key: Option<String>,
}

/// Rows with a valid key, in fetch order, before duplicates are resolved.
#[derive(Debug)]
struct Staging {
    loc: Vec<(u32, u32)>,
    keys: Vec<i64>,
    created: Vec<Option<i64>>,
    /// Hash of the selected source values: equal ⇔ identical duplicates.
    value_fp: Vec<[u8; 16]>,
    /// Hash of the whole raw record: a deterministic tie-break.
    record_fp: Vec<[u8; 16]>,
    cols: Vec<Stage>,
    log: Vec<PendingLog>,
    rejects: Vec<PendingReject>,
    raw_records: u64,
    scratch: Vec<u8>,
}

impl Staging {
    fn new(spec: &DatasetSpec) -> Self {
        let cols = spec
            .columns
            .iter()
            .map(|c| match c.kind {
                Kind::Key => Stage::Key,
                Kind::CreatedTimestamp => Stage::Created,
                Kind::Timestamp => Stage::Ts {
                    values: Vec::new(),
                    valid: Bitmap::new(),
                },
                Kind::Text => Stage::Text {
                    ids: Vec::new(),
                    valid: Bitmap::new(),
                    interner: Interner::default(),
                },
                Kind::LatLon => Stage::Geo {
                    lat: Vec::new(),
                    lon: Vec::new(),
                    valid: Bitmap::new(),
                },
            })
            .collect();
        Self {
            loc: Vec::new(),
            keys: Vec::new(),
            created: Vec::new(),
            value_fp: Vec::new(),
            record_fp: Vec::new(),
            cols,
            log: Vec::new(),
            rejects: Vec::new(),
            raw_records: 0,
            scratch: Vec::new(),
        }
    }

    fn stage(&mut self, spec: &DatasetSpec, loc: (u32, u32), rec: &RawValue) -> Result<()> {
        self.raw_records += 1;
        let map: Fields = serde_json::from_str(rec.get())
            .context("record is not a JSON object with plain (unescaped) field names")?;
        let key_text = scalar(&map, spec.key_field())?;
        let Some(key) = key_text.as_deref().and_then(parse_key) else {
            self.rejects.push(PendingReject {
                loc,
                rule: Rule::InvalidKey,
                raw_key: key_text.map(Cow::into_owned),
            });
            return Ok(());
        };
        let row = self.keys.len() as u32;
        self.loc.push(loc);
        self.keys.push(key);

        // In-memory fingerprints only (never persisted), so a plain BLAKE3
        // over one length-prefixed buffer is enough.
        let buf = &mut self.scratch;
        buf.clear();
        for field in spec.source_fields() {
            match map.get(field).map(RawValue::get).filter(|t| *t != "null") {
                None => buf.push(0),
                Some(t) => {
                    buf.push(1);
                    buf.extend_from_slice(&(t.len() as u32).to_le_bytes());
                    buf.extend_from_slice(t.as_bytes());
                }
            }
        }
        self.value_fp.push(fingerprint(blake3::hash(buf)));
        self.record_fp
            .push(fingerprint(blake3::hash(rec.get().as_bytes())));

        for (ci, (stage, cs)) in self.cols.iter_mut().zip(spec.columns).enumerate() {
            let mut log = |rule, raw| {
                self.log.push(PendingLog {
                    row,
                    column: ci as u16,
                    rule,
                    raw,
                })
            };
            match stage {
                Stage::Key => {}
                Stage::Created => {
                    let v = scalar(&map, cs.source_fields[0])?;
                    self.created
                        .push(v.as_deref().and_then(parse_naive_timestamp));
                }
                Stage::Ts { values, valid } => match scalar(&map, cs.source_fields[0])? {
                    None => push_null(values, valid, 0),
                    Some(s) => match parse_naive_timestamp(&s) {
                        Some(t) => {
                            values.push(t);
                            valid.push(true);
                        }
                        None => {
                            push_null(values, valid, 0);
                            log(Rule::InvalidClosed, Some(s.into_owned()));
                        }
                    },
                },
                Stage::Text {
                    ids,
                    valid,
                    interner,
                } => match scalar(&map, cs.source_fields[0])? {
                    None => push_null(ids, valid, 0),
                    Some(s) => {
                        let trimmed = s.trim_matches(|c: char| c.is_ascii_whitespace());
                        if trimmed.is_empty() {
                            push_null(ids, valid, 0);
                            log(Rule::EmptyText, Some(s.to_string()));
                        } else {
                            ids.push(interner.intern(trimmed));
                            valid.push(true);
                            if trimmed.len() != s.len() {
                                log(Rule::Trimmed, Some(s.to_string()));
                            }
                        }
                    }
                },
                Stage::Geo { lat, lon, valid } => {
                    let (lat_f, lon_f) = (cs.source_fields[0], cs.source_fields[1]);
                    let (a, b) = (scalar(&map, lat_f)?, scalar(&map, lon_f)?);
                    let parsed = match (&a, &b) {
                        (None, None) => None,
                        (Some(a), Some(b)) => parse_coord(a).zip(parse_coord(b)),
                        _ => None,
                    };
                    match parsed {
                        Some((y, x)) => {
                            lat.push(y as f32);
                            lon.push(x as f32);
                            valid.push(true);
                        }
                        None => {
                            lat.push(0.0);
                            push_null(lon, valid, 0.0);
                            if a.is_some() || b.is_some() {
                                let raw = |f| map.get(f).map_or("null", RawValue::get);
                                let pair = format!("[{},{}]", raw(lat_f), raw(lon_f));
                                log(Rule::InvalidLocation, Some(pair));
                            }
                        }
                    }
                }
            }
        }
        Ok(())
    }

    fn finish(self, spec: &DatasetSpec, raw: &RawDir, lo: i64, hi: i64) -> Result<Cleaned> {
        let n = self.keys.len();
        let mut status: Vec<Option<Rule>> = vec![None; n];

        // CR-03 / CR-04, grouped by key; the copy with the smallest raw-record
        // fingerprint is kept, so the outcome doesn't depend on fetch order.
        let mut by_key: Vec<u32> = (0..n as u32).collect();
        by_key.sort_unstable_by_key(|&r| (self.keys[r as usize], self.record_fp[r as usize]));
        for group in by_key.chunk_by(|&a, &b| self.keys[a as usize] == self.keys[b as usize]) {
            if group.len() < 2 {
                continue;
            }
            let fp = self.value_fp[group[0] as usize];
            if group.iter().all(|&r| self.value_fp[r as usize] == fp) {
                for &r in &group[1..] {
                    status[r as usize] = Some(Rule::DuplicateIdentical);
                }
            } else {
                for &r in group {
                    status[r as usize] = Some(Rule::DuplicateConflicting);
                }
            }
        }
        for (s, created) in status.iter_mut().zip(&self.created) {
            if s.is_none() {
                *s = match *created {
                    None => Some(Rule::InvalidCreated),
                    Some(t) if t < lo || t >= hi => Some(Rule::OutsideScope),
                    Some(_) => None,
                };
            }
        }

        let mut order: Vec<u32> = (0..n as u32)
            .filter(|&r| status[r as usize].is_none())
            .collect();
        ensure!(order.len() <= u32::MAX as usize, "more than u32::MAX rows");
        order.sort_unstable_by_key(|&r| (self.created[r as usize], self.keys[r as usize]));
        let mut final_row = vec![u32::MAX; n];
        for (i, &r) in order.iter().enumerate() {
            final_row[r as usize] = i as u32;
        }

        let columns = self
            .cols
            .into_iter()
            .zip(spec.columns)
            .map(|(stage, cs)| {
                let (data, valid) = match stage {
                    Stage::Key => (ColumnData::I64(gather(&self.keys, &order)), None),
                    Stage::Created => {
                        let v = order.iter().map(|&r| {
                            self.created[r as usize].expect("admitted rows have a created time")
                        });
                        (ColumnData::Timestamp(v.collect()), None)
                    }
                    Stage::Ts { values, valid } => (
                        ColumnData::Timestamp(gather(&values, &order)),
                        Some(gather_bits(&valid, &order)),
                    ),
                    Stage::Geo { lat, lon, valid } => (
                        ColumnData::Geo {
                            lat: gather(&lat, &order),
                            lon: gather(&lon, &order),
                        },
                        Some(gather_bits(&valid, &order)),
                    ),
                    Stage::Text {
                        ids,
                        valid,
                        interner,
                    } => {
                        let valid = gather_bits(&valid, &order);
                        let (codes, dictionary) =
                            encode_dictionary(&ids, &valid, &order, interner.values);
                        (ColumnData::DictUtf8 { codes, dictionary }, Some(valid))
                    }
                };
                let valid = valid.filter(|v| v.count_zeros() > 0);
                Column::new(cs.name, data, valid)
            })
            .collect();

        let mut log: Vec<LogEntry> = self
            .log
            .into_iter()
            .filter(|e| status[e.row as usize].is_none())
            .map(|e| LogEntry {
                row_index: final_row[e.row as usize],
                column: e.column,
                rule: e.rule,
                raw_value: e.raw,
            })
            .collect();
        log.sort_by(|a, b| {
            (a.row_index, a.column, a.rule.id(), &a.raw_value).cmp(&(
                b.row_index,
                b.column,
                b.rule.id(),
                &b.raw_value,
            ))
        });

        let mut pending = self.rejects;
        for (r, s) in status.iter().enumerate() {
            if let Some(rule) = s {
                pending.push(PendingReject {
                    loc: self.loc[r],
                    rule: *rule,
                    raw_key: Some(self.keys[r].to_string()),
                });
            }
        }
        let rejects = resolve_rejects(raw, pending)?;
        Ok(Cleaned {
            columns,
            log,
            rejects,
            raw_records: self.raw_records,
        })
    }
}

/// Reads back the raw text of each rejected record, page by page.
fn resolve_rejects(raw: &RawDir, mut pending: Vec<PendingReject>) -> Result<Vec<Reject>> {
    pending.sort_unstable_by_key(|p| p.loc);
    let mut out = Vec::with_capacity(pending.len());
    for group in pending.chunk_by(|a, b| a.loc.0 == b.loc.0) {
        let body = raw.read_page(group[0].loc.0 as usize)?;
        let records: Vec<&RawValue> = serde_json::from_slice(&body)?;
        for p in group {
            out.push(Reject {
                raw_unique_key: p.raw_key.clone(),
                rule: p.rule,
                raw_record: records[p.loc.1 as usize].get().to_string(),
            });
        }
    }
    sort_rejects(&mut out);
    Ok(out)
}

pub fn sort_rejects(rejects: &mut [Reject]) {
    rejects.sort_by(|a, b| {
        (&a.raw_unique_key, a.rule.id(), &a.raw_record).cmp(&(
            &b.raw_unique_key,
            b.rule.id(),
            &b.raw_record,
        ))
    });
}

/// A record's fields as borrowed slices of the raw JSON. Parsing this way
/// allocates nothing per field, which is most of the build's time at 7M
/// records (see docs/benchmarks/m0.md).
struct Fields<'a>(Vec<(&'a str, &'a RawValue)>);

impl<'a> Fields<'a> {
    /// The last occurrence wins, as in `serde_json::Map`.
    fn get(&self, field: &str) -> Option<&'a RawValue> {
        self.0
            .iter()
            .rev()
            .find(|(k, _)| *k == field)
            .map(|(_, v)| *v)
    }
}

impl<'de: 'a, 'a> Deserialize<'de> for Fields<'a> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V<'a>(std::marker::PhantomData<&'a ()>);
        impl<'de: 'a, 'a> Visitor<'de> for V<'a> {
            type Value = Fields<'a>;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a JSON object")
            }
            fn visit_map<M: MapAccess<'de>>(self, mut m: M) -> Result<Fields<'a>, M::Error> {
                let mut out = Vec::with_capacity(16);
                while let Some(entry) = m.next_entry::<&'de str, &'de RawValue>()? {
                    out.push(entry);
                }
                Ok(Fields(out))
            }
        }
        d.deserialize_map(V(std::marker::PhantomData))
    }
}

/// The text of a scalar field: a string's contents, or a number's source
/// text; `None` if missing or JSON null. Arrays, objects, and booleans mean
/// the source schema changed, so the build stops.
fn scalar<'a>(map: &Fields<'a>, field: &str) -> Result<Option<Cow<'a, str>>> {
    let Some(raw) = map.get(field) else {
        return Ok(None);
    };
    let text = raw.get();
    match text.as_bytes().first() {
        Some(b'n') if text == "null" => Ok(None),
        Some(b'"') if !text.contains('\\') => Ok(Some(Cow::Borrowed(&text[1..text.len() - 1]))),
        Some(b'"') => Ok(Some(Cow::Owned(serde_json::from_str::<String>(text)?))),
        Some(b'-' | b'0'..=b'9') => Ok(Some(Cow::Borrowed(text))),
        _ => bail!("CR-01: field {field:?} has non-scalar value {text}"),
    }
}

/// CR-02: a positive base-10 integer without sign or leading zeros.
fn parse_key(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.is_empty() || b[0] == b'0' || !b.iter().all(u8::is_ascii_digit) {
        return None;
    }
    s.parse().ok()
}

fn parse_coord(s: &str) -> Option<f64> {
    s.parse::<f64>().ok().filter(|v| v.is_finite())
}

fn fingerprint(h: blake3::Hash) -> [u8; 16] {
    h.as_bytes()[..16].try_into().expect("16 of 32 bytes")
}

fn push_null<T>(values: &mut Vec<T>, valid: &mut Bitmap, zero: T) {
    values.push(zero);
    valid.push(false);
}

fn gather<T: Copy>(values: &[T], order: &[u32]) -> Vec<T> {
    order.iter().map(|&r| values[r as usize]).collect()
}

fn gather_bits(bits: &Bitmap, order: &[u32]) -> Bitmap {
    Bitmap::from_bools(order.iter().map(|&r| bits.get(r as usize)))
}

/// Builds the sorted dictionary of the values admitted rows use, and remaps
/// interned ids to codes in that dictionary. Null slots get code 0.
fn encode_dictionary(
    ids: &[u32],
    valid: &Bitmap,
    order: &[u32],
    values: Vec<String>,
) -> (Vec<u32>, Vec<String>) {
    let mut used = vec![false; values.len()];
    for (i, &r) in order.iter().enumerate() {
        if valid.get(i) {
            used[ids[r as usize] as usize] = true;
        }
    }
    let mut kept: Vec<u32> = (0..values.len() as u32)
        .filter(|&id| used[id as usize])
        .collect();
    kept.sort_unstable_by(|&a, &b| {
        values[a as usize]
            .as_bytes()
            .cmp(values[b as usize].as_bytes())
    });
    let mut code_of = vec![0u32; values.len()];
    for (code, &id) in kept.iter().enumerate() {
        code_of[id as usize] = code as u32;
    }
    let codes = order
        .iter()
        .enumerate()
        .map(|(i, &r)| {
            if valid.get(i) {
                code_of[ids[r as usize] as usize]
            } else {
                0
            }
        })
        .collect();
    let mut values: Vec<Option<String>> = values.into_iter().map(Some).collect();
    let dictionary = kept
        .iter()
        .map(|&id| values[id as usize].take().expect("each id kept once"))
        .collect();
    (codes, dictionary)
}

/// `n:u32 ‖ (row_index:u32 ‖ column:u16 ‖ rule_id ‖ raw_value?)*`.
pub fn cleaning_log_hash(log: &[LogEntry]) -> ContentHash {
    let mut h = ContentHasher::new(CLEANING_LOG_CONTEXT);
    h.u32(log.len() as u32);
    for e in log {
        h.u32(e.row_index)
            .u16(e.column)
            .bytes(e.rule.id().as_bytes())
            .opt_bytes(e.raw_value.as_deref().map(str::as_bytes));
    }
    h.finish()
}

/// `n:u32 ‖ (raw_unique_key? ‖ rule_id ‖ raw_record)*`.
pub fn rejects_hash(rejects: &[Reject]) -> ContentHash {
    let mut h = ContentHasher::new(REJECTS_CONTEXT);
    h.u32(rejects.len() as u32);
    for r in rejects {
        h.opt_bytes(r.raw_unique_key.as_deref().map(str::as_bytes))
            .bytes(r.rule.id().as_bytes())
            .bytes(r.raw_record.as_bytes());
    }
    h.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_parsing() {
        assert_eq!(parse_key("123"), Some(123));
        assert_eq!(parse_key("9223372036854775807"), Some(i64::MAX));
        for bad in [
            "",
            "0",
            "0123",
            "-1",
            "+1",
            "1.0",
            "1e3",
            " 1",
            "9223372036854775808",
        ] {
            assert_eq!(parse_key(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn borrowed_field_parsing() {
        let rec = r#"{"a":"plain","b":"esc\"aped\u00e9","c":-40.5e1,"d":null,"e":[1],"a":"last"}"#;
        let f: Fields = serde_json::from_str(rec).unwrap();
        assert_eq!(scalar(&f, "a").unwrap().as_deref(), Some("last"));
        let b = scalar(&f, "b").unwrap().unwrap();
        assert_eq!(b, "esc\"apedé");
        assert!(matches!(b, Cow::Owned(_)));
        assert!(matches!(scalar(&f, "a").unwrap(), Some(Cow::Borrowed(_))));
        assert_eq!(scalar(&f, "c").unwrap().as_deref(), Some("-40.5e1"));
        assert_eq!(scalar(&f, "d").unwrap(), None);
        assert_eq!(scalar(&f, "missing").unwrap(), None);
        assert!(scalar(&f, "e").unwrap_err().to_string().contains("CR-01"));
        assert!(serde_json::from_str::<Fields>("[1]").is_err());
    }

    #[test]
    fn coords() {
        assert_eq!(parse_coord("40.7"), Some(40.7));
        assert_eq!(parse_coord("NaN"), None);
        assert_eq!(parse_coord("inf"), None);
        assert_eq!(parse_coord(""), None);
    }

    #[test]
    fn dictionary_is_sorted_and_only_admitted_values() {
        let values = vec![
            "b".to_string(),
            "a".to_string(),
            "unused".to_string(),
            "c".to_string(),
        ];
        let ids = vec![0, 1, 2, 3, 0];
        // Staging rows 2 (unused) is not admitted; row 3 is admitted but null.
        let order = vec![4, 3, 1, 0];
        let valid = Bitmap::from_bools([true, false, true, true]);
        let (codes, dict) = encode_dictionary(&ids, &valid, &order, values);
        assert_eq!(dict, vec!["a", "b"]);
        assert_eq!(codes, vec![1, 0, 0, 1]);
    }
}
