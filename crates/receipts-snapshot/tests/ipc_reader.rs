//! The dependency-free IPC reader in `receipts-core` (used by the WASM
//! engine) must decode exactly what the arrow-ipc writer produces.

mod common;

use common::*;
use proptest::prelude::*;
use receipts_core::hash::{cleaning_log_hash, hash_column, rejects_hash};
use receipts_core::ipc::{Values, read_file};
use receipts_snapshot::arrow_io::{self, META_SNAPSHOT_HASH, ReadTable};
use receipts_snapshot::manifest::{CLEANING_LOG_FILE, DATA_FILE, REJECTS_FILE};
use receipts_snapshot::spec::NYC_311;
use receipts_snapshot::{assemble, clean, synth};
use std::fs;

fn build(records: &[serde_json::Value]) -> assemble::Built {
    let raw = scratch("raw");
    synth::write_raw(
        &raw,
        &NYC_311,
        &scope(),
        &metadata(),
        synth::paginate(records, 5000),
    )
    .unwrap();
    assemble::build(&NYC_311, &raw, &scratch("snap")).unwrap()
}

fn check(built: &assemble::Built) {
    let dir = &built.dir;
    let m = &built.manifest;
    let bytes = fs::read(dir.join(DATA_FILE)).unwrap();
    let ours = read_file(&bytes).unwrap();
    let theirs = ReadTable::open(&dir.join(DATA_FILE)).unwrap();
    assert_eq!(
        ours.meta(META_SNAPSHOT_HASH),
        Some(m.snapshot_hash.as_str())
    );
    assert_eq!(ours.batch_lengths, theirs.batch_lengths());
    assert_eq!(ours.fields.len(), m.schema.len());
    for (i, info) in m.schema.iter().enumerate() {
        let col = ours.core_column(i).unwrap();
        assert_eq!(col, theirs.column(i).unwrap(), "column {}", info.name);
        assert_eq!(hash_column(&col).column.to_hex(), info.column_hash);
    }

    let log = read_file(&fs::read(dir.join(CLEANING_LOG_FILE)).unwrap()).unwrap();
    let (_, expected_log) = arrow_io::read_cleaning_log(&dir.join(CLEANING_LOG_FILE)).unwrap();
    let (Values::U32(rows), Values::U16(cols), Values::Utf8(rules), Values::Utf8(raws)) = (
        &log.columns[0].values,
        &log.columns[1].values,
        &log.columns[2].values,
        &log.columns[3].values,
    ) else {
        panic!("cleaning log types")
    };
    let raw_valid = |i: usize| log.columns[3].validity.as_ref().is_none_or(|v| v.get(i));
    let h = cleaning_log_hash((0..rows.len()).map(|i| {
        (
            rows[i],
            cols[i],
            rules[i].as_str(),
            raw_valid(i).then(|| raws[i].as_str()),
        )
    }));
    assert_eq!(h, clean::cleaning_log_hash(&expected_log));
    assert_eq!(h.to_hex(), m.cleaning.cleaning_log_hash);

    let rej = read_file(&fs::read(dir.join(REJECTS_FILE)).unwrap()).unwrap();
    let (Values::Utf8(keys), Values::Utf8(rules), Values::Utf8(records)) = (
        &rej.columns[1].values,
        &rej.columns[2].values,
        &rej.columns[3].values,
    ) else {
        panic!("rejects types")
    };
    let key_valid = |i: usize| rej.columns[1].validity.as_ref().is_none_or(|v| v.get(i));
    let h = rejects_hash((0..keys.len()).map(|i| {
        (
            key_valid(i).then(|| keys[i].as_str()),
            rules[i].as_str(),
            records[i].as_str(),
        )
    }));
    assert_eq!(h.to_hex(), m.cleaning.rejects_hash);
}

#[test]
fn reads_the_fixture_snapshot() {
    // Every cleaning rule fires, so the log and rejects have nulls and text.
    let built = build(&fixture());
    assert!(
        built.manifest.cleaning.cleaning_log_rows > 0 && built.manifest.cleaning.rejected_rows > 0
    );
    check(&built);
}

#[test]
fn reads_a_multi_chunk_snapshot() {
    let built = build(&synth::generate_311(140_000, 5, &scope()));
    assert_eq!(built.manifest.chunk_count, 3);
    check(&built);
}

#[test]
fn compression_changes_bytes_not_content() {
    let records = synth::generate_311(140_000, 8, &scope());
    let raw = scratch("raw");
    synth::write_raw(
        &raw,
        &NYC_311,
        &scope(),
        &metadata(),
        synth::paginate(&records, 50_000),
    )
    .unwrap();
    let plain = assemble::build_with(
        &NYC_311,
        &raw,
        &scratch("snap"),
        arrow_io::Compression::None,
    )
    .unwrap();
    let lz4 =
        assemble::build_with(&NYC_311, &raw, &scratch("snap"), arrow_io::Compression::Lz4).unwrap();
    assert_eq!(plain.manifest.snapshot_hash, lz4.manifest.snapshot_hash);
    assert_ne!(
        plain.manifest.manifest_hash, lz4.manifest.manifest_hash,
        "files[] records the compression"
    );
    let size = |b: &assemble::Built| b.manifest.files[0].bytes;
    assert!(
        size(&lz4) < size(&plain),
        "{} vs {}",
        size(&lz4),
        size(&plain)
    );
    assert_eq!(lz4.manifest.files[0].compression, "lz4_frame");
    let a = read_file(&fs::read(plain.dir.join(DATA_FILE)).unwrap()).unwrap();
    let b = read_file(&fs::read(lz4.dir.join(DATA_FILE)).unwrap()).unwrap();
    assert_eq!(a.columns, b.columns);
    check(&plain);
    check(&lz4);
}

#[test]
fn reads_an_empty_snapshot() {
    check(&build(&[]));
}

/// A record batch claiming an absurd length must be refused, not overflow
/// (found by `corrupt_files_never_panic`).
#[test]
fn absurd_lengths_are_errors() {
    let built = build(&fixture());
    let bytes = fs::read(built.dir.join(DATA_FILE)).unwrap();
    // Every 8-byte little-endian word equal to the fixture's row count is a
    // candidate length field; setting each to i64::MAX must never panic.
    let rows = built.manifest.row_count as i64;
    let mut hits = 0;
    for at in 0..bytes.len().saturating_sub(8) {
        if i64::from_le_bytes(bytes[at..at + 8].try_into().unwrap()) == rows {
            let mut b = bytes.clone();
            b[at..at + 8].copy_from_slice(&i64::MAX.to_le_bytes());
            let _ = read_file(&b);
            b[at..at + 8].copy_from_slice(&(u32::MAX as i64 * 3).to_le_bytes());
            let _ = read_file(&b);
            hits += 1;
        }
    }
    assert!(hits > 0, "found no length fields to corrupt");
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: std::env::var("PROPTEST_CASES").ok().and_then(|s| s.parse().ok()).unwrap_or(300),
        failure_persistence: None,
        ..ProptestConfig::default()
    })]

    /// Corrupted files must be rejected (or read), never crash the reader.
    #[test]
    fn corrupt_files_never_panic(flips in prop::collection::vec((any::<prop::sample::Index>(), any::<u8>()), 1..8), cut in any::<prop::sample::Index>()) {
        use std::sync::OnceLock;
        static FILE: OnceLock<Vec<u8>> = OnceLock::new();
        let file = FILE.get_or_init(|| {
            let built = build(&fixture());
            fs::read(built.dir.join(DATA_FILE)).unwrap()
        });
        let mut bytes = file.clone();
        for (i, b) in &flips {
            let i = i.index(bytes.len());
            bytes[i] ^= b | 1;
        }
        if let Ok(f) = read_file(&bytes) {
            for i in 0..f.fields.len() {
                let _ = f.core_column(i);
            }
        }
        let truncated = &file[..cut.index(file.len())];
        prop_assert!(read_file(truncated).is_err());
    }
}
