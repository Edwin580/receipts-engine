# Snapshot schema (M0 proposal)

Status: **proposed, awaiting review.** Nothing here is implemented yet except
`RowId` and `ColumnType` in `receipts-core`.

A snapshot is an immutable, content-addressed copy of one public dataset. It
is produced offline by `receipts-snapshot` and served as static files.
Everything the engine computes is keyed by the snapshot's `snapshot_hash`.

## 1. Guiding rule: snapshots coerce, plans judge

The snapshot step only changes the **representation** of the data (text →
typed values, strings → dictionary codes). It makes no **judgements about the
data** ("this closed date looks wrong", "this ZIP is junk"). Judgements belong
in the logical plan as explicit `Filter`/`Map` steps. There they show up in the
Pipeline View with a sentence and a row count, and they can be undone as a
counterfactual.

If a value can't be represented at all (for example, an unparseable
timestamp), the snapshot records a null or rejects the row, and **logs it**.
Nothing is dropped or changed silently. The full rule list is in
[`cleaning-rules.md`](cleaning-rules.md).

## 2. Files

```
snapshots/<dataset>/<snapshot_hash[0..16]>/
  manifest.json        # everything in §4; small, fetched first
  data.arrow           # Arrow IPC *file* format; one record batch per chunk
  cleaning_log.arrow   # one row per value a cleaning rule touched (§5)
  rejects.arrow        # one row per source record that was rejected (§5)
```

- **Chunking.** Each chunk holds exactly 65,536 rows (2^16), except the last.
  So `chunk = row_index >> 16`, with no lookup table. Chunks are the unit of
  hashing, streaming load, and (later) HTTP range requests. The Arrow IPC
  footer gives each chunk's byte offset.
- **Compression.** None in M0. Arrow body compression (LZ4/ZSTD) would need a
  decompressor in WASM. Whether we add one or rely on HTTP `Content-Encoding`
  is decided in M4, using the load-time numbers (see ADR 0001).
- **Dictionaries.** There is one dictionary per string column for the whole
  file, written once before the first batch. It is sorted by UTF-8 byte order,
  so code order equals string order. Sort, min, and max on a dictionary column
  can therefore compare `u32` codes directly.

## 3. The NYC 311 schema (source_id 1)

Source: *311 Service Requests from 2010 to Present*, Socrata dataset
`erm2-nwe9` on `data.cityofnewyork.us`.
Scope: `created_date >= 2024-01-01T00:00:00 AND created_date < 2026-01-01T00:00:00`
(two calendar years; see open question Q1).

| # | Column | Type | Nullable | Socrata field | Notes |
|---|---|---|---|---|---|
| 0 | `unique_key` | `i64` | no | `unique_key` (text) | The source's primary key. Parsed as a decimal. |
| 1 | `created_date` | `timestamp` | no | `created_date` | Naive NYC wall clock, µs (ADR 0003). |
| 2 | `closed_date` | `timestamp` | yes | `closed_date` | Kept as published, including values before `created_date` and sentinels such as 1900-01-01 (see §1). |
| 3 | `agency` | `utf8` (dict) | yes | `agency` | Acronym, e.g. `NYPD`, `HPD`. |
| 4 | `complaint_type` | `utf8` (dict) | yes | `complaint_type` | ~200–300 distinct values. |
| 5 | `descriptor` | `utf8` (dict) | yes | `descriptor` | Subtype of the complaint; ~1–2k distinct values. |
| 6 | `location_type` | `utf8` (dict) | yes | `location_type` | |
| 7 | `incident_zip` | `utf8` (dict) | yes | `incident_zip` | Stored as a string: a ZIP is a category, not a number. |
| 8 | `borough` | `utf8` (dict) | yes | `borough` | `Unspecified` is kept as a real value from the source, not turned into null. |
| 9 | `community_board` | `utf8` (dict) | yes | `community_board` | e.g. `12 MANHATTAN`, `0 Unspecified`. |
| 10 | `status` | `utf8` (dict) | yes | `status` | |
| 11 | `channel` | `utf8` (dict) | yes | `open_data_channel_type` | `PHONE`, `ONLINE`, `MOBILE`, ... |
| 12 | `location` | `geo` | yes | `latitude`, `longitude` | Two `f32` arrays with one shared validity bitmap. |

**Physical layout per type** (little-endian throughout):

| Logical type | Arrow type in `data.arrow` | Values buffer |
|---|---|---|
| `i64` | `Int64` | 8 B/row |
| `f64` | `Float64` | 8 B/row |
| `bool` | `Boolean` | 1 bit/row |
| `timestamp` | `Timestamp(Microsecond, None)` | 8 B/row |
| `utf8` (dict) | `Dictionary(UInt32, Utf8)` | 4 B/row + dictionary |
| `geo` | `Struct{lat: Float32, lon: Float32}` | 8 B/row |

For nullable fixed-width columns, null slots are **zeroed**. Arrow leaves them
undefined, but we need them fixed so that the hashes are deterministic.

**Estimated size:** about 61 B/row, plus validity bitmaps and small
dictionaries. That is roughly 300 MB for 5M rows and roughly 420 MB for 7M
rows, before any compression.

**Excluded columns** (listed in the manifest with a reason): free-text fields
(`resolution_description`, addresses, street names, landmark). These are large,
high-cardinality, and gain nothing from dictionary encoding. Also excluded:
redundant or derived fields (`agency_name`, `park_borough`,
`x/y_coordinate_state_plane`, `location`, `city`) and sparse domain-specific
fields (taxi, bridge/highway, vehicle, facility). Any of these can be added
later as a new snapshot version.

**Derived columns are deliberately absent.** For example, resolution time
(`closed_date − created_date`) is a `Map` in a plan, so the reader can see it
being computed.

## 4. Manifest (`manifest.json`)

See [`manifest.example.json`](manifest.example.json) for a complete example.
Its fields:

- `format_version`: `"receipts-snapshot/1"`.
- `snapshot_hash`: the content hash of the snapshot (§6). Receipts cite this.
- `manifest_hash`: BLAKE3 of this file's canonical JSON with the
  `manifest_hash` field removed. It covers the fetch metadata.
- `source`: `source_id`, `dataset` name, `portal`, `dataset_id`, `source_url`,
  `license`/terms URL, and the Socrata `rowsUpdatedAt` seen at fetch time.
- `fetch`: `started_at` and `finished_at` (UTC, RFC 3339), the exact query
  (`$select`, `$where`, `$order`, page size), `pages`, `raw_records`,
  `raw_hash`, and `tool_version` (crate version + git commit).
  - `raw_hash` is BLAKE3 over the raw response bodies, in page order, each
    prefixed by its length.
- `scope`: a human-readable scope sentence plus the machine predicate.
- `row_count`, `chunk_rows` (65536), `chunk_count`.
- `sort_key`: `["created_date", "unique_key"]` (§7).
- `schema[]`: for each column: `name`, `type`, `nullable`, `source_fields[]`,
  `description` (plain English, shown in the UI), `null_count`, and for
  dictionary columns `dictionary_size`. Also `column_hash` and
  `chunk_hashes[]` (hex).
- `excluded_columns[]`: `{ field, reason }`.
- `cleaning`: `rules_version`, and per rule `{ id, affected_rows, action }`,
  plus `cleaning_log_hash`, `rejects_hash`, and `rejected_rows`.
- `known_issues[]`: plain-English notes the UI can surface. Examples:
  "About N% of `created_date` values are exactly midnight, which suggests
  date-only precision." "N rows have `closed_date` before `created_date`."
  These are counted, not fixed.

## 5. Cleaning log and rejects

Cleaning is itself lineage, so both files are part of the snapshot hash.

`cleaning_log.arrow`: one row per (row, column) value that a rule changed.

| Column | Type | Meaning |
|---|---|---|
| `row_index` | `u32` | Row in `data.arrow` |
| `column` | `u16` | Schema column index |
| `rule_id` | dict utf8 | e.g. `CR-06` |
| `raw_value` | utf8, nullable | The exact source text before the rule applied |

`rejects.arrow`: one row per source record that was not admitted.

| Column | Type | Meaning |
|---|---|---|
| `reject_index` | `u32` | Position in this file (sorted by `raw_unique_key`, then `rule_id`) |
| `raw_unique_key` | utf8 | Source key text, as published |
| `rule_id` | dict utf8 | Why the record was rejected |
| `raw_record` | utf8 | The record's raw JSON, as fetched |

Rejected rows have no `RowId`, because they never enter the engine. The Claim
View can still say "N records were excluded while building the snapshot, see
why", and the UI links to them.

## 6. Content hashing (BLAKE3, ADR 0002)

We hash a **canonical logical encoding** that we define, not the Arrow IPC
bytes. IPC bytes depend on writer version, padding, and metadata ordering. All
integers are little-endian. `‖` means concatenation. Each hash starts with a
domain tag, so a hash of one kind can never collide with a hash of another
kind.

```
chunk_hash(c, k)   = BLAKE3("receipts/chunk/v1" ‖ type_tag:u8 ‖ rows:u32
                            ‖ validity bits (LSB-first, zero-padded to a byte;
                              all-ones if the column has no nulls)
                            ‖ values (null slots zeroed; geo = all lats then all lons;
                              dict = u32 codes; bool = LSB-first bits))
dict_hash(c)       = BLAKE3("receipts/dict/v1" ‖ n:u32 ‖ for each entry: len:u32 ‖ bytes)
column_hash(c)     = BLAKE3("receipts/column/v1" ‖ name_len:u32 ‖ name ‖ type_tag:u8
                            ‖ dict_hash(c) or 32 zero bytes ‖ chunk_hash(c, 0) ‖ ... )
snapshot_hash      = BLAKE3("receipts/snapshot/v1" ‖ source_id:u16 ‖ row_count:u32
                            ‖ canonical_json(scope, sort_key, cleaning.rules_version)
                            ‖ column_hash(0) ‖ ... ‖ cleaning_log_hash ‖ rejects_hash)
```

`snapshot_hash` covers **content only**. Fetch timestamps and tool versions
are left out. Two fetches that produce identical data therefore get the same
hash, and a receipt stays valid across a re-fetch when nothing changed.
`manifest_hash` covers everything.

Canonical JSON follows RFC 8785 (JCS) for the few JSON fragments we hash. The
same canonicalizer will be reused for plan hashing in M1.

## 7. Row order and RowId assignment

1. Fetch with keyset pagination: `$order=unique_key`, and
   `$where=... AND unique_key > '<last key>'`. This avoids the duplicates and
   gaps that offset paging produces while the dataset is being updated.
2. Apply the cleaning rules, logging every change, and move rejects aside.
3. Sort the admitted rows by (`created_date`, `unique_key`). Row order is then
   independent of fetch order and API quirks, and it is useful: a time-range
   `Filter` touches a contiguous run of chunks.
4. Assign `row_index` = position after the sort.
   `RowId = [source_id:16 | 0:16 | row_index:32]`.

## 8. Open questions for review

- **Q1: row count vs. budgets.** 311 has recently run at a bit over 3M
  requests a year, so two years is probably about 6.5–7M rows. The
  performance budgets assume 5M. I couldn't confirm the count, because
  `data.cityofnewyork.us` is blocked from this container's network. Options:
  (a) keep two years and benchmark on both the full snapshot and a 5M-row
  prefix; (b) narrow the scope to about 18 months. I recommend (a).
- **Q2: app token.** Socrata throttles anonymous clients. The CLI would read
  an optional `SOCRATA_APP_TOKEN` from the environment. The token is never
  written to the manifest.
- **Q3: keep raw pages?** `--keep-raw` would write the fetched pages as
  compressed NDJSON, for local re-cleaning without a re-fetch. They wouldn't
  be published; `raw_hash` pins them either way.
