//! Data-quality issues that are counted, not fixed (ADR 0004). Each one is
//! something a plan may want to filter out explicitly and visibly.

use crate::manifest::KnownIssue;
use receipts_core::time::{MICROS_PER_DAY, MICROS_PER_SECOND, days_from_civil};
use receipts_core::{Column, ColumnData};

/// A loose bounding box around the five boroughs.
pub const NYC_LAT: (f32, f32) = (40.45, 40.95);
pub const NYC_LON: (f32, f32) = (-74.30, -73.65);

/// `rows_updated_at` is the source's last update (UTC epoch seconds), if known.
pub fn known_issues(columns: &[Column], rows_updated_at: Option<i64>) -> Vec<KnownIssue> {
    let find = |name: &str| columns.iter().find(|c| c.name == name);
    let timestamps = |c: &Column| match &c.data {
        ColumnData::Timestamp(v) => Some(v.clone()),
        _ => None,
    };
    let mut out = Vec::new();
    let mut push = |id: &str, cols: &[&str], count: usize, sentence: String| {
        out.push(KnownIssue {
            id: id.into(),
            columns: cols.iter().map(|s| s.to_string()).collect(),
            count: count as u32,
            sentence,
        });
    };

    if let (Some(created), Some(closed)) = (find("created_date"), find("closed_date")) {
        let (cr, cl) = (
            timestamps(created).unwrap_or_default(),
            timestamps(closed).unwrap_or_default(),
        );
        let valid = |i: usize| closed.is_valid(i);
        let n = (0..cl.len()).filter(|&i| valid(i) && cl[i] < cr[i]).count();
        push(
            "KI-closed-before-created",
            &["created_date", "closed_date"],
            n,
            format!(
                "{} requests have a closed date earlier than their created date.",
                thousands(n)
            ),
        );
        let y2010 = days_from_civil(2010, 1, 1) * MICROS_PER_DAY;
        let n = (0..cl.len()).filter(|&i| valid(i) && cl[i] < y2010).count();
        push(
            "KI-closed-before-2010",
            &["closed_date"],
            n,
            format!(
                "{} requests have a closed date before 2010, when this dataset starts; these are usually placeholder dates.",
                thousands(n)
            ),
        );
        if let Some(updated) = rows_updated_at {
            let limit = updated * MICROS_PER_SECOND + MICROS_PER_DAY;
            let n = (0..cl.len()).filter(|&i| valid(i) && cl[i] > limit).count();
            push(
                "KI-closed-in-future",
                &["closed_date"],
                n,
                format!(
                    "{} requests have a closed date more than a day after the data was last updated.",
                    thousands(n)
                ),
            );
        }
    }
    if let Some(created) = find("created_date").and_then(timestamps) {
        let n = created
            .iter()
            .filter(|t| t.rem_euclid(MICROS_PER_DAY) == 0)
            .count();
        push(
            "KI-created-midnight",
            &["created_date"],
            n,
            format!(
                "{} requests were created at exactly midnight, which usually means only the date was recorded.",
                thousands(n)
            ),
        );
    }
    if let Some(loc) = find("location")
        && let ColumnData::Geo { lat, lon } = &loc.data
    {
        let n = (0..lat.len())
            .filter(|&i| loc.is_valid(i))
            .filter(|&i| {
                !(NYC_LAT.0..=NYC_LAT.1).contains(&lat[i])
                    || !(NYC_LON.0..=NYC_LON.1).contains(&lon[i])
            })
            .count();
        push(
            "KI-location-outside-nyc",
            &["location"],
            n,
            format!(
                "{} requests have a location outside New York City.",
                thousands(n)
            ),
        );
    }
    let count_text = |name: &str, pred: &dyn Fn(&str) -> bool| {
        let c = find(name)?;
        let ColumnData::DictUtf8 { codes, dictionary } = &c.data else {
            return None;
        };
        let hits: Vec<bool> = dictionary.iter().map(|s| pred(s)).collect();
        Some(
            (0..codes.len())
                .filter(|&i| c.is_valid(i) && hits[codes[i] as usize])
                .count(),
        )
    };
    if let Some(n) = count_text("borough", &|s| s.eq_ignore_ascii_case("unspecified")) {
        push(
            "KI-borough-unspecified",
            &["borough"],
            n,
            format!("{} requests have the borough 'Unspecified'.", thousands(n)),
        );
    }
    if let Some(n) = count_text("incident_zip", &|s| {
        !(s.len() == 5 && s.bytes().all(|b| b.is_ascii_digit()))
    }) {
        push(
            "KI-zip-not-5-digits",
            &["incident_zip"],
            n,
            format!(
                "{} requests have a ZIP code that is not five digits.",
                thousands(n)
            ),
        );
    }
    out
}

/// `1234567` -> `"1,234,567"`.
pub fn thousands(n: usize) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thousands_separators() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1000), "1,000");
        assert_eq!(thousands(1234567), "1,234,567");
    }
}
