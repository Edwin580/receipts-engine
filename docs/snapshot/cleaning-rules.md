# Cleaning rules (M0 proposal)

Status: **proposed, awaiting review.** `rules_version: 1`

Cleaning is lineage. Every rule has a stable ID. For every rule, the manifest
records how many rows it affected, and every individual application is written
to `cleaning_log.arrow` (value changes) or `rejects.arrow` (rejected records).
Changing any rule's behaviour bumps `rules_version`, and that changes the
`snapshot_hash`.

**Scope of these rules.** They make the data *representable*. They do not
make it *plausible* (see `schema.md` §1). Implausible values stay in the
snapshot and are counted in `known_issues`. Plans deal with them visibly.

## Actions

- **reject**: the record is not admitted. It goes to `rejects.arrow` with its
  raw JSON.
- **null**: the value becomes null. The raw text goes to `cleaning_log.arrow`.
- **normalize**: the value is rewritten losslessly or near-losslessly. The raw
  text goes to `cleaning_log.arrow` (except for CR-10, which is systematic;
  see that rule).
- **fail**: the whole snapshot build aborts. This is used for schema drift.

## Rules

| ID | Applies to | Condition | Action | Why |
|---|---|---|---|---|
| CR-01 | whole fetch | An expected Socrata field is missing from the dataset metadata, or has changed type | **fail** | Schema drift has to be looked at by a person, not patched over. |
| CR-02 | `unique_key` | Missing, or not a base-10 integer in `1..=i64::MAX` | reject | Without a key, the row can't be cited against the source. |
| CR-03 | `unique_key` | The same key appears more than once, and all copies are byte-identical | keep one copy, **reject** the rest | Keyset paging should prevent this. If it happens anyway, we record it. |
| CR-04 | `unique_key` | The same key appears more than once, and the copies differ | reject **all** copies | We can't know which copy is true, and picking one would be a hidden judgement. |
| CR-05 | `created_date` | Missing, or doesn't match `YYYY-MM-DDTHH:MM:SS(.fff)?` | reject | This is the scope and sort key, so the row can't be placed. |
| CR-06 | `closed_date` | Present but doesn't match the timestamp format | null | This is a representation problem. Implausible *valid* dates are kept (see below). |
| CR-07 | all `utf8` columns | Has leading or trailing ASCII whitespace | normalize: trim | Otherwise `"BRONX "` and `"BRONX"` would be two categories. Case is **not** changed. |
| CR-08 | all `utf8` columns | Empty after trimming | null | An empty string and a missing field mean the same thing in Socrata output. |
| CR-09 | `latitude`, `longitude` | Either one is missing or doesn't parse as a finite decimal | null the whole `location` | A point with one coordinate can't be plotted. Out-of-NYC coordinates that *do* parse are kept. |
| CR-10 | `location` | Always applies | normalize: f64 → f32 | Systematic, so it is not logged per row. At NYC's latitude the maximum error is about 0.3 m, which is below the precision of the source's geocoding. Documented once in the manifest. |
| CR-11 | timestamps | Has fractional seconds | Kept at µs precision (lossless for the source's ms precision) | No log entry needed. This rule is listed so that every coercion is written down. |

## Known issues we count but don't fix (`known_issues`)

- `closed_date < created_date`.
- `closed_date` far in the past (for example 1900-01-01) or in the future.
- `created_date` exactly at midnight, which suggests date-only precision for
  some agencies.
- `location` outside a loose NYC bounding box (lat 40.45–40.95, lon −74.30 to
  −73.65).
- `borough = 'Unspecified'`, and `incident_zip` values that are not five
  digits.

Each one comes with a count and a one-sentence explanation in plain English.
The UI can then offer the matching `Filter` as a one-click, visible pipeline
step.
