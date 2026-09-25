//! Synthetic raw directories, for tests and benchmarks while the real API is
//! unavailable. Records look like Socrata 311 output (all values as strings,
//! nulls omitted) and include every anomaly the cleaning rules handle, at low rates.

use crate::raw::{
    self, FETCH_FILE, FetchRecord, METADATA_CONTEXT, METADATA_FILE, PAGES_DIR, PageRecord,
    Pagination, RawHasher, Scope,
};
use crate::socrata::{ROW_ID_FIELD, select_fields};
use crate::spec::DatasetSpec;
use anyhow::{Result, ensure};
use receipts_core::hash::ContentHasher;
use receipts_core::time::{MICROS_PER_SECOND, format_naive_timestamp};
use serde_json::{Map, Value, json};
use std::fs;
use std::path::Path;

/// Socrata metadata that satisfies CR-01 for `spec`.
pub fn metadata_for(spec: &DatasetSpec, rows_updated_at: i64) -> Value {
    let columns: Vec<Value> = spec
        .expected_types()
        .map(|(field, accepted)| json!({ "fieldName": field, "dataTypeName": accepted[0] }))
        .collect();
    json!({ "id": spec.dataset_id, "rowsUpdatedAt": rows_updated_at, "columns": columns })
}

/// Writes a complete raw directory from pre-built page bodies.
pub fn write_raw(
    dir: &Path,
    spec: &DatasetSpec,
    scope: &Scope,
    metadata: &Value,
    pages: impl IntoIterator<Item = impl AsRef<[u8]>>,
) -> Result<FetchRecord> {
    ensure!(
        !dir.exists() || fs::read_dir(dir)?.next().is_none(),
        "{} is not empty",
        dir.display()
    );
    fs::create_dir_all(dir.join(PAGES_DIR))?;
    let metadata = serde_json::to_vec(metadata)?;
    fs::write(dir.join(METADATA_FILE), &metadata)?;
    let mut hasher = RawHasher::new();
    let mut records = Vec::new();
    let mut raw_records = 0u64;
    for (i, body) in pages.into_iter().enumerate() {
        let body = body.as_ref();
        let n = serde_json::from_slice::<Vec<&serde_json::value::RawValue>>(body)?.len();
        let file = raw::page_file_name(i);
        fs::write(dir.join(&file), body)?;
        hasher.page(body);
        raw_records += n as u64;
        records.push(PageRecord {
            file,
            url: format!("synthetic://page/{}", i + 1),
            records: n as u32,
            bytes: body.len() as u64,
            blake3: blake3::hash(body).to_hex().to_string(),
        });
    }
    let record = FetchRecord {
        tool_version: crate::TOOL_VERSION.into(),
        portal: spec.portal.into(),
        dataset_id: spec.dataset_id.into(),
        endpoint: "synthetic".into(),
        select: select_fields(spec),
        scope: scope.clone(),
        where_: scope.soql(),
        order: ROW_ID_FIELD.into(),
        pagination: Pagination {
            kind: "synthetic".into(),
            key: ROW_ID_FIELD.into(),
            page_size: 0,
        },
        started_at: "1970-01-01T00:00:00Z".into(),
        finished_at: "1970-01-01T00:00:00Z".into(),
        rows_updated_at_start: None,
        rows_updated_at_end: None,
        pages: records,
        raw_records,
        raw_hash: hasher.finish().to_hex(),
        metadata_hash: ContentHasher::new(METADATA_CONTEXT)
            .raw(&metadata)
            .finish()
            .to_hex(),
    };
    fs::write(dir.join(FETCH_FILE), serde_json::to_vec_pretty(&record)?)?;
    Ok(record)
}

/// Splits records into JSON-array page bodies.
pub fn paginate(records: &[Value], page_size: usize) -> Vec<Vec<u8>> {
    stream_pages(records.iter().cloned(), page_size).collect()
}

/// SplitMix64: small, deterministic, good enough for synthetic data.
#[derive(Debug, Clone)]
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self(seed)
    }

    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform in `[0, 1)`.
    pub fn f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

    pub fn below(&mut self, n: u64) -> u64 {
        self.next_u64() % n
    }

    pub fn chance(&mut self, p: f64) -> bool {
        self.f64() < p
    }
}

/// A Zipf-like categorical distribution over `n` generated labels.
#[derive(Debug)]
struct Categorical {
    labels: Vec<String>,
    cumulative: Vec<f64>,
}

impl Categorical {
    fn new(labels: Vec<String>, exponent: f64) -> Self {
        let mut total = 0.0;
        let cumulative = (0..labels.len())
            .map(|k| {
                total += 1.0 / ((k + 1) as f64).powf(exponent);
                total
            })
            .collect::<Vec<_>>();
        let cumulative = cumulative.iter().map(|c| c / total).collect();
        Self { labels, cumulative }
    }

    fn sample_index(&self, rng: &mut Rng) -> usize {
        let u = rng.f64();
        self.cumulative
            .partition_point(|&c| c < u)
            .min(self.labels.len() - 1)
    }

    fn sample(&self, rng: &mut Rng) -> &str {
        &self.labels[self.sample_index(rng)]
    }
}

const BOROUGHS: [&str; 5] = ["BROOKLYN", "QUEENS", "MANHATTAN", "BRONX", "STATEN ISLAND"];

fn labels(values: &[&str]) -> Vec<String> {
    values.iter().map(|s| s.to_string()).collect()
}

/// An endless, deterministic stream of 311-like records within a scope.
/// Streaming keeps memory flat when generating millions of records.
#[derive(Debug)]
pub struct Generator311 {
    rng: Rng,
    lo: i64,
    span_secs: u64,
    complaints: Categorical,
    agencies: Categorical,
    location_types: Categorical,
    zips: Categorical,
    statuses: Categorical,
    channels: Categorical,
    next_key: u64,
    index: u64,
    /// Recent records, the source of duplicate-key anomalies.
    recent: Vec<Map<String, Value>>,
}

impl Generator311 {
    pub fn new(seed: u64, scope: &Scope) -> Self {
        let (lo, hi) = scope.bounds().expect("valid scope");
        let head = [
            "Illegal Parking",
            "Noise - Residential",
            "HEAT/HOT WATER",
            "Blocked Driveway",
            "Noise - Street/Sidewalk",
            "Request Large Bulky Item Collection",
            "UNSANITARY CONDITION",
            "Street Condition",
            "Water System",
            "Abandoned Vehicle",
        ];
        let complaints = labels(&head)
            .into_iter()
            .chain((head.len()..250).map(|i| format!("Complaint Type {i:03}")))
            .collect();
        let zips = (10001..10300)
            .step_by(3)
            .chain((10451..10475).step_by(2))
            .chain((11201..11697).step_by(5))
            .map(|z: u32| z.to_string())
            .collect();
        Self {
            rng: Rng::new(seed),
            lo,
            span_secs: ((hi - lo) / MICROS_PER_SECOND) as u64,
            complaints: Categorical::new(complaints, 1.1),
            agencies: Categorical::new(
                labels(&[
                    "NYPD", "HPD", "DSNY", "DOT", "DEP", "DOB", "DPR", "DOHMH", "DHS", "TLC",
                    "DCWP", "EDC", "DOE", "DOITT", "DFTA",
                ]),
                1.3,
            ),
            location_types: Categorical::new(
                (0..150).map(|i| format!("Location Type {i:03}")).collect(),
                1.2,
            ),
            zips: Categorical::new(zips, 0.6),
            statuses: Categorical::new(
                labels(&["Closed", "In Progress", "Open", "Pending", "Assigned"]),
                2.5,
            ),
            channels: Categorical::new(
                labels(&["PHONE", "ONLINE", "MOBILE", "UNKNOWN", "OTHER"]),
                1.0,
            ),
            next_key: 59_000_000,
            index: 0,
            recent: Vec::new(),
        }
    }

    fn fresh(&mut self, id: String) -> Map<String, Value> {
        let rng = &mut self.rng;
        self.next_key += 1 + rng.below(3);
        let mut r = Map::new();
        r.insert(ROW_ID_FIELD.into(), json!(id));
        if !rng.chance(0.000_1) {
            r.insert("unique_key".into(), json!(self.next_key.to_string()));
        }
        let mut created = self.lo + (rng.below(self.span_secs) as i64) * MICROS_PER_SECOND;
        if rng.chance(0.02) {
            created -= created.rem_euclid(86_400 * MICROS_PER_SECOND);
        }
        let ts = |t: i64| format_naive_timestamp(t)[..23].to_string();
        if rng.chance(0.000_05) {
            r.insert("created_date".into(), json!("not a date"));
        } else {
            r.insert("created_date".into(), json!(ts(created)));
        }
        if rng.chance(0.85) {
            let closed = if rng.chance(0.003) {
                created - (rng.below(86_400) as i64 + 1) * MICROS_PER_SECOND
            } else if rng.chance(0.0005) {
                receipts_core::time::parse_naive_timestamp("1900-01-01T00:00:00").expect("valid")
            } else {
                created + (-(1.0 - rng.f64()).ln() * 5.0 * 86_400.0) as i64 * MICROS_PER_SECOND
            };
            r.insert("closed_date".into(), json!(ts(closed)));
        }
        let complaint_i = self.complaints.sample_index(rng);
        r.insert("agency".into(), json!(self.agencies.sample(rng)));
        r.insert(
            "complaint_type".into(),
            json!(self.complaints.labels[complaint_i]),
        );
        let mut descriptor = format!("Descriptor {complaint_i:03}-{}", rng.below(6));
        if rng.chance(0.001) {
            descriptor.push(' ');
        }
        r.insert("descriptor".into(), json!(descriptor));
        if rng.chance(0.9) {
            let lt = if rng.chance(0.0005) {
                String::new()
            } else {
                self.location_types.sample(rng).to_string()
            };
            r.insert("location_type".into(), json!(lt));
        }
        if rng.chance(0.98) {
            let z = if rng.chance(0.001) {
                "N/A".to_string()
            } else {
                self.zips.sample(rng).to_string()
            };
            r.insert("incident_zip".into(), json!(z));
        }
        let borough = if rng.chance(0.005) {
            "Unspecified"
        } else {
            BOROUGHS[rng.below(5) as usize]
        };
        r.insert("borough".into(), json!(borough));
        let board = if borough == "Unspecified" {
            "0 Unspecified".to_string()
        } else {
            format!("{:02} {borough}", 1 + rng.below(18))
        };
        r.insert("community_board".into(), json!(board));
        r.insert("status".into(), json!(self.statuses.sample(rng)));
        r.insert(
            "open_data_channel_type".into(),
            json!(self.channels.sample(rng)),
        );
        if rng.chance(0.97) {
            let (lat, lon) = if rng.chance(0.0002) {
                (0.0, 0.0)
            } else {
                (40.50 + rng.f64() * 0.41, -74.25 + rng.f64() * 0.55)
            };
            r.insert("latitude".into(), json!(format!("{lat:.15}")));
            if !rng.chance(0.0005) {
                r.insert("longitude".into(), json!(format!("{lon:.15}")));
            }
        }
        r
    }
}

impl Iterator for Generator311 {
    type Item = Value;

    fn next(&mut self) -> Option<Value> {
        let id = format!("row-{:012x}", self.index);
        self.index += 1;
        // Duplicate-key anomalies: a copy of a recent record under a new :id,
        // sometimes with a conflicting value.
        let record = if self.recent.len() > 10 && self.rng.chance(0.000_07) {
            let pick = self.rng.below(self.recent.len() as u64) as usize;
            let mut copy = self.recent[pick].clone();
            copy.insert(ROW_ID_FIELD.into(), json!(id));
            if self.rng.chance(0.3) {
                copy.insert("status".into(), json!("Conflicting Status"));
            }
            copy
        } else {
            let r = self.fresh(id);
            if self.recent.len() < 4096 {
                self.recent.push(r.clone());
            } else {
                let slot = self.rng.below(4096) as usize;
                self.recent[slot] = r.clone();
            }
            r
        };
        Some(Value::Object(record))
    }
}

pub fn generate_311(rows: usize, seed: u64, scope: &Scope) -> Vec<Value> {
    Generator311::new(seed, scope).take(rows).collect()
}

/// Streams records into JSON-array page bodies of `page_size` records.
/// Always yields at least one page (`[]` for no records), like the API.
pub fn stream_pages(
    records: impl Iterator<Item = Value>,
    page_size: usize,
) -> impl Iterator<Item = Vec<u8>> {
    let mut records = records.peekable();
    let mut first = true;
    std::iter::from_fn(move || {
        if records.peek().is_none() && !first {
            return None;
        }
        first = false;
        let page: Vec<Value> = records.by_ref().take(page_size.max(1)).collect();
        Some(serde_json::to_vec(&page).expect("values serialize"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generator_is_deterministic() {
        let scope = Scope::from_dates("created_date", "2024-01-01", "2026-01-01").unwrap();
        assert_eq!(generate_311(500, 7, &scope), generate_311(500, 7, &scope));
        assert_ne!(generate_311(500, 7, &scope), generate_311(500, 8, &scope));
    }
}
