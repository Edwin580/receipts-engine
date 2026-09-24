# Cleaning rules

Status: **implemented (M0)**, `rules_version: 1`. Code:
`crates/receipts-snapshot/src/{rules,clean}.rs`. Independent re-implementation
used as a differential oracle: `tools/oracle/snapshot_oracle.py`.

Cleaning is lineage. Every rule has a stable ID. The manifest counts every
rule, and every individual application is written to `cleaning_log.arrow`
(value changes) or `rejects.arrow` (rejected records). Changing any rule's
behaviour bumps `rules_version`, and that changes the `snapshot_hash`.

**Scope of these rules.** They make the data *representable*. They do not make
it *plausible* (ADR 0004). Implausible values stay in the snapshot and are
counted in `known_issues`.

## Actions

- **fail**: the whole build aborts.
- **reject**: the record is not admitted. It goes to `rejects.arrow` with its
  raw JSON. A rejected record gets exactly one rule.
- **null**: the value becomes null. The raw text goes to `cleaning_log.arrow`.
- **normalize**: the value is rewritten losslessly or near-losslessly. It is
  logged unless the rule is systematic (CR-10, CR-11), in which case it is only
  counted.

## Order of evaluation

For each record:

1. CR-02 (key).
2. CR-03/CR-04 (duplicates), across **all** records with a valid key. A copy
   with a bad date can therefore not hide a conflict.
3. CR-05 (created time).
4. CR-12 (scope).

Value rules (CR-06 to CR-09) are applied to every record with a valid key.
Their log entries are kept only for admitted rows. Rules on a missing field
never fire: a missing value is simply null, with no log entry.

## Rules

| ID | Applies to | Condition | Action | Why |
|---|---|---|---|---|
| CR-01 | whole dataset | An expected field is missing from the Socrata metadata, has a type outside the accepted set (schema.md §3), or a record holds an array, object, or boolean where a scalar is expected | **fail** | Schema drift needs a person to look at it, not a patch. Checked at fetch (before paging) and at build. |
| CR-02 | `unique_key` | Missing, or not `[1-9][0-9]*` fitting in `i64` (JSON strings and numbers both accepted) | reject | Without a key, the row can't be cited against the source. Leading zeros are refused so that `"0123"` and `"123"` can't collide. |
| CR-03 | `unique_key` | Several records share a key and are identical in every selected source field (compared as raw JSON text; `:id` ignored) | keep one, **reject** the rest | The kept copy is the one whose raw record has the smallest BLAKE3, so the choice doesn't depend on fetch order. |
| CR-04 | `unique_key` | Several records share a key and differ in any selected field | reject **all** copies | We can't know which copy is true, and picking one would be a hidden judgement. |
| CR-05 | `created_date` | Missing, or not exactly `YYYY-MM-DDTHH:MM:SS` with an optional `.` and 1–6 digits, as a real calendar time | reject | This is the scope and sort key, so the row can't be placed. |
| CR-06 | `closed_date` | Present but fails the CR-05 format | null | A representation problem. Implausible *valid* dates are kept. |
| CR-07 | text columns | Leading or trailing ASCII whitespace | normalize: trim | Otherwise `"BRONX "` and `"BRONX"` would be two categories. Case is **not** changed. |
| CR-08 | text columns | Present but empty after trimming | null | In Socrata output, an empty string means the same as a missing field. Raw text logged. |
| CR-09 | `latitude`, `longitude` | At least one is present, but they don't both parse as finite decimals | null the whole `location` | A point with one coordinate can't be plotted. The raw pair is logged. Out-of-NYC points that *do* parse are kept. |
| CR-10 | `location` | Always applies | normalize: f64 → f32 | Systematic, so only counted. At NYC's latitude the maximum error is about 0.3 m, below the source's geocoding precision. |
| CR-11 | timestamps | Has a non-zero fractional second | Kept at µs precision (lossless) | Counted only. Listed so that every coercion is written down. |
| CR-12 | `created_date` | Valid, but outside the fetch scope | reject | Guards against the API returning more than was asked for. |

## Known issues we count but don't fix (`known_issues`)

| ID | What |
|---|---|
| `KI-closed-before-created` | `closed_date < created_date` |
| `KI-closed-before-2010` | `closed_date` before the dataset starts (placeholder dates such as 1900-01-01) |
| `KI-closed-in-future` | `closed_date` more than a day after the source's `rowsUpdatedAt` |
| `KI-created-midnight` | `created_date` exactly at midnight (date-only precision) |
| `KI-location-outside-nyc` | Outside lat 40.45–40.95, lon −74.30 to −73.65 |
| `KI-borough-unspecified` | `borough = 'Unspecified'` (case-insensitive) |
| `KI-zip-not-5-digits` | `incident_zip` that isn't five digits |

Each one comes with a count and a one-sentence explanation in plain English.
The UI can then offer the matching `Filter` as a one-click, visible pipeline
step.
