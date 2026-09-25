# Snapshot schema

Status: **implemented (M0)** in `crates/receipts-snapshot`. The hashing
primitives live in `crates/receipts-core`.

A snapshot is an immutable, content-addressed copy of one public dataset. It
is produced offline by `receipts-snapshot` and served as static files.
Everything the engine computes is keyed by the snapshot's `snapshot_hash`.

## 1. Guiding rule: snapshots coerce, plans judge (ADR 0004)

The snapshot step only changes the **representation** of the data (text →
typed values, strings → dictionary codes). It makes no **judgements about the
data** ("this closed date looks wrong", "this ZIP is junk"). Judgements belong
in the logical plan as explicit `Filter`/`Map` steps. There they show up in the
Pipeline View with a sentence and a row count, and they can be undone as a
counterfactual.

If a value can't be represented at all, the snapshot records a null or
rejects the row, and **logs it**. Nothing is dropped or changed silently. The
full rule list is in [`cleaning-rules.md`](cleaning-rules.md).

## 2. Pipeline and files (ADR 0006)

```
receipts-snapshot fetch  --out raw/nyc311 [--from 2024-01-01 --to 2026-01-01]
receipts-snapshot build  --raw raw/nyc311 --out snapshots/nyc311
receipts-snapshot verify snapshots/nyc311/<hash16>
```

`fetch` writes a **raw directory**: the API's responses, byte for byte.
`build` turns it into a **snapshot directory**. Cleaning can therefore be
re-run and tested offline, and the raw pages are pinned by hash.

```
raw/<name>/                          (local only, not published; 3.1 GB for the 7.1M real records)
  fetch.json         how it was fetched; per-page size + BLAKE3; written last
  metadata.json      Socrata view metadata at fetch start (schema, rowsUpdatedAt)
  pages/000001.json  raw response bodies, in fetch order

snapshots/<dataset>/<snapshot_hash[0..16]>/     (published)
  manifest.json        everything in §4; small, fetched first
  data.arrow           Arrow IPC file; one record batch per chunk
  cleaning_log.arrow   one row per value a cleaning rule changed (§5)
  rejects.arrow        one row per source record not admitted (§5)
```

- **Chunking.** Each chunk holds exactly 65,536 rows (2^16), except the last.
  So `chunk = row_index >> 16`, with no lookup table. Chunks are the unit of
  hashing, streaming load, and (later) HTTP range requests.
- **Compression.** Each Arrow buffer is an LZ4 frame (Arrow's `LZ4_FRAME`
  body compression) by default; `build --compression none` writes plain
  buffers. Hashes cover logical content, so both give the same
  `snapshot_hash` (ADR 0001, M5 update). `manifest.files[].compression`
  says which was used.
- **Dictionaries.** There is one dictionary per string column for the whole
  file, sorted by UTF-8 byte order, with `u32` codes. It contains only values
  used by admitted rows. Code order equals string order.
- **Schema metadata** on every Arrow file: `receipts.format_version`,
  `receipts.table` (`data` / `cleaning_log` / `rejects`), and
  `receipts.snapshot_hash`. A file can't be mixed into another snapshot
  without `verify` noticing.

## 3. The NYC 311 schema (source_id 1)

Source: *311 Service Requests from 2010 to Present*, Socrata dataset
`erm2-nwe9` on `data.cityofnewyork.us`.
Scope: `created_date >= 2024-01-01T00:00:00 AND created_date < 2026-01-01T00:00:00`.

| # | Column | Type | Nullable | Socrata field(s) | Accepted Socrata type (CR-01) |
|---|---|---|---|---|---|
| 0 | `unique_key` | `i64` | no | `unique_key` | `text` or `number` |
| 1 | `created_date` | `timestamp` | no | `created_date` | `calendar_date` |
| 2 | `closed_date` | `timestamp` | yes | `closed_date` | `calendar_date` |
| 3 | `agency` | `utf8_dict` | yes | `agency` | `text` |
| 4 | `complaint_type` | `utf8_dict` | yes | `complaint_type` | `text` |
| 5 | `descriptor` | `utf8_dict` | yes | `descriptor` | `text` |
| 6 | `location_type` | `utf8_dict` | yes | `location_type` | `text` |
| 7 | `incident_zip` | `utf8_dict` | yes | `incident_zip` | `text` |
| 8 | `borough` | `utf8_dict` | yes | `borough` | `text` |
| 9 | `community_board` | `utf8_dict` | yes | `community_board` | `text` |
| 10 | `status` | `utf8_dict` | yes | `status` | `text` |
| 11 | `channel` | `utf8_dict` | yes | `open_data_channel_type` | `text` |
| 12 | `location` | `geo` | yes | `latitude`, `longitude` | `number` |

The plain-English description of each column is in the spec
(`crates/receipts-snapshot/src/spec.rs`) and is copied into the manifest.
`closed_date` is kept as published, including values before `created_date`
and placeholder dates such as 1900-01-01. `borough = 'Unspecified'` is the
source's own label, so it stays a value, not a null.

**Physical layout** (little-endian throughout):

| Logical type | Arrow type in `data.arrow` | Bytes per row |
|---|---|---|
| `i64` | `Int64` | 8 |
| `f64` | `Float64` | 8 |
| `bool` | `Boolean` | 1 bit |
| `timestamp` | `Timestamp(Microsecond, None)`: naive NYC wall clock (ADR 0003) | 8 |
| `utf8_dict` | `Dictionary(UInt32, Utf8)` | 4 (+ dictionary) |
| `geo` | `Struct{lat: Float32, lon: Float32}`, nulls on the struct | 8 |

Null slots in fixed-width columns are written as zero.

**Measured size:** 69.9 B/row. The real two-year snapshot (7,111,809 rows)
has a 497 MB `data.arrow`, matching the 6.7M-row synthetic benchmark's
per-row size (see `docs/benchmarks/m0.md`).

**Excluded columns:** 27 Socrata fields, each listed with a reason in
`excluded_columns` in the manifest. They are free-text and address fields,
redundant fields, and sparse domain-specific fields.

**Derived columns are deliberately absent.** For example, resolution time is a
`Map` in a plan, so the reader can see it being computed.

## 4. Manifest (`manifest.json`)

[`manifest.example.json`](manifest.example.json) is a real manifest, built
from 100k synthetic records (`synth --rows 100000 --seed 1`).

| Field | Meaning |
|---|---|
| `format_version` | `"receipts-snapshot/1"` |
| `snapshot_hash` | Content hash (§6). Receipts cite this. |
| `manifest_hash` | Hash of this manifest's canonical JSON, with this field removed (§6). |
| `source` | `source_id`, `dataset`, `portal`, `dataset_id`, `source_url`, `terms_url`, `rows_updated_at` |
| `fetch` | `started_at`/`finished_at` and the exact query (`select`, `where`, `order`, `pagination`). Also `pages`, `raw_records`, `raw_hash`, `metadata_hash`, `rows_updated_at_start`/`_end` (they differ if the dataset changed mid-fetch), and the fetching `tool_version`. |
| `build` | `tool_version` of the build (crate version + git commit) |
| `scope` | `sentence` (plain English) and `predicate` (`column`, `gte`, `lt`) |
| `row_count`, `chunk_rows`, `chunk_count` | |
| `sort_key` | `["created_date", "unique_key"]` |
| `schema[]` | `index`, `name`, `type`, `nullable`, `source_fields`, `description`, `null_count`, `dictionary_size`/`dictionary_hash` (dictionary columns), `column_hash`, `chunk_hashes[]` |
| `excluded_columns[]` | `{ field, reason }` |
| `cleaning` | `rules_version`, `rules[]` (`id`, `action`, `count`, what `counts` means, `description`), `rejected_rows`, `cleaning_log_rows`, `cleaning_log_hash`, `rejects_hash` |
| `known_issues[]` | `{ id, columns, count, sentence }`: counted, not fixed. See cleaning-rules.md. |
| `files[]` | `{ path, bytes, compression }` for each data file (for progress bars; covered by `manifest_hash`, not by `snapshot_hash`) |

The manifest contains only integers and strings, so its canonical form is
exact.

## 5. Cleaning log and rejects

Cleaning is itself lineage, so both files are part of `snapshot_hash`.

`cleaning_log.arrow` has one row per (row, column) value a logged rule
changed. It is sorted by (`row_index`, `column`, `rule_id`, `raw_value`).

| Column | Arrow type | Meaning |
|---|---|---|
| `row_index` | `UInt32` | Row in `data.arrow` |
| `column` | `UInt16` | Schema column index |
| `rule_id` | `Utf8` | e.g. `CR-07` |
| `raw_value` | `Utf8`, nullable | Exact source text before the rule. For `location`, the raw JSON pair `[latitude,longitude]`. |

`rejects.arrow` has one row per source record not admitted. It is sorted by
(`raw_unique_key` with nulls first, `rule_id`, `raw_record`).

| Column | Arrow type | Meaning |
|---|---|---|
| `reject_index` | `UInt32` | 0..n |
| `raw_unique_key` | `Utf8`, nullable | Key text as published |
| `rule_id` | `Utf8` | Why it was rejected (exactly one rule per record) |
| `raw_record` | `Utf8` | The record's JSON, exactly as fetched (including `:id`) |

Rejected rows have no `RowId`, because they never enter the engine. The UI can
still say "N records were excluded while building the snapshot, see why".

## 6. Content hashing (ADR 0002)

Every hash is BLAKE3 in **`derive_key` mode**, with one context string per
kind of object. A chunk hash can therefore never collide with a column or
snapshot hash. We hash a canonical *logical* encoding, never file bytes.
Integers are little-endian. `‖` means concatenation. `str(x)` means
`len:u32 ‖ utf8 bytes`. `opt(x)` means `0:u8` for null, or `1:u8 ‖ str(x)`.

| Hash | Context | Input |
|---|---|---|
| chunk | `receipts snapshot v1 chunk` | `type_tag:u8 ‖ rows:u32 ‖ validity ‖ values` |
| dictionary | `receipts snapshot v1 dictionary` | `n:u32 ‖ str(entry)*` |
| column | `receipts snapshot v1 column` | `str(name) ‖ type_tag:u8 ‖ (dictionary hash or 32 zero bytes) ‖ n_chunks:u32 ‖ chunk_hash*` |
| cleaning log | `receipts snapshot v1 cleaning-log` | `n:u32 ‖ (row_index:u32 ‖ column:u16 ‖ str(rule_id) ‖ opt(raw_value))*` |
| rejects | `receipts snapshot v1 rejects` | `n:u32 ‖ (opt(raw_unique_key) ‖ str(rule_id) ‖ str(raw_record))*` |
| **snapshot** | `receipts snapshot v1 snapshot` | `source_id:u16 ‖ row_count:u32 ‖ str(descriptor) ‖ n:u32 ‖ column_hash* ‖ 2:u32 ‖ cleaning_log_hash ‖ rejects_hash` |
| manifest | `receipts snapshot v1 manifest` | canonical JSON of the manifest without `manifest_hash` |
| raw pages | `receipts snapshot v1 raw-pages` | `n:u32 ‖ (len:u64 ‖ BLAKE3(page))*` |
| metadata | `receipts snapshot v1 socrata-metadata` | raw `metadata.json` bytes |

Details:

- **Validity** is `ceil(rows/8)` bytes, least significant bit first, with
  padding bits zero. It is always present: a column with no nulls hashes as
  all-ones.
- **Values** are written with null slots as zero. By type:
  - `i64`/`timestamp`: 8 bytes per row.
  - `f64`: IEEE-754 bits.
  - `bool`: one bit per row, packed like validity.
  - dictionary columns: `u32` codes.
  - `geo`: all latitudes, then all longitudes, as `f32` bits.
- **Type tags** are `i64`=1, `f64`=2, `bool`=3, `timestamp`=4, `utf8_dict`=5,
  `geo`=6. They must never be renumbered.
- **`descriptor`** is the canonical JSON (RFC 8785, integers only) of
  `{rules_version, scope, sort_key, source: {portal, dataset_id}}`.

`snapshot_hash` covers **content only**: data, schema names and types, scope,
source identity, rules version, cleaning log, and rejects. It does not cover
fetch timestamps or tool versions, so two fetches of identical data get the
same hash. `manifest_hash` covers everything. A known-answer test in
`receipts-core` pins the v1 chunk encoding.

## 7. Fetch order, row order, and RowIds (ADR 0007)

1. **Fetch** uses keyset pagination on Socrata's system row id:
   `$order=:id` and `$where=(<scope>) AND :id > '<last :id>'`. `:id` is
   selected into every record.
2. **Clean** (cleaning-rules.md). Duplicates are resolved across all records
   with a valid key, independent of the order they were fetched in.
3. **Sort** the admitted rows by (`created_date`, `unique_key`). Keys are
   unique after cleaning, so the order is total.
4. **Assign** `row_index` = position after the sort, and
   `RowId = [source_id:16 | 0:16 | row_index:32]`.

The snapshot is independent of fetch order and page size. A test shuffles and
re-paginates the records and checks that `snapshot_hash` doesn't change.

## 8. Resolved questions

- **Q1 (rows vs. budgets):** keep two years. The first live fetch
  (2026-09-24) gave **7,111,809 rows**, 6% above the 6.7M estimate and 42%
  above the 5M rows the performance budgets assumed. **Decided
  2026-09-25:** keep the two-year default and restate the budgets for
  ~7.1M rows from browser measurements. The M4 proposal and results are in
  `docs/benchmarks/m4.md`. Every budget is met except a full-table sort in
  the single-threaded fallback. A 2025-only snapshot (3,655,040 rows)
  remains an option if a slower target device needs it.
- **Q2 (app token):** `fetch` sends `X-App-Token` if `SOCRATA_APP_TOKEN` is
  set. The token is never written anywhere.
- **Q3 (keep raw pages):** superseded by the fetch/build split (ADR 0006).
  Raw pages are always kept locally.
