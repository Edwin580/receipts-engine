//! `receipts-snapshot`: offline snapshot CLI (M0).
//!
//! ```text
//! receipts-snapshot fetch  --out raw/nyc311           # Socrata -> raw directory
//! receipts-snapshot build  --raw raw/nyc311 --out snapshots/nyc311
//! receipts-snapshot verify snapshots/nyc311/<hash16>
//! receipts-snapshot synth  --out raw/synth --rows 1000000   # benchmarks without the network
//! ```

use anyhow::Result;
use clap::{Parser, Subcommand};
use receipts_snapshot::arrow_io::Compression;
use receipts_snapshot::raw::Scope;
use receipts_snapshot::socrata::{self, FetchOptions, UreqTransport};
use receipts_snapshot::spec::NYC_311;
use receipts_snapshot::{assemble, synth, verify};
use std::path::PathBuf;
use std::time::Instant;

#[derive(Parser)]
#[command(version = receipts_snapshot::TOOL_VERSION, about = "Build content-hashed snapshots of NYC 311 data")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Fetch raw pages from the Socrata API. Reads SOCRATA_APP_TOKEN if set.
    Fetch {
        #[arg(long)]
        out: PathBuf,
        /// First day in scope (inclusive), YYYY-MM-DD.
        #[arg(long, default_value = "2024-01-01")]
        from: String,
        /// First day out of scope (exclusive), YYYY-MM-DD.
        #[arg(long, default_value = "2026-01-01")]
        to: String,
        #[arg(long, default_value_t = 50_000)]
        page_size: u32,
    },
    /// Clean a raw directory into a snapshot under <out>/<hash16>/.
    Build {
        #[arg(long)]
        raw: PathBuf,
        #[arg(long)]
        out: PathBuf,
        /// Arrow buffer compression: lz4 (default) or none. The snapshot
        /// hash doesn't depend on it.
        #[arg(long, default_value = "lz4")]
        compression: String,
    },
    /// Re-derive every hash in a snapshot directory and compare with its manifest.
    Verify { dir: PathBuf },
    /// Write a synthetic 311-like raw directory (for tests and benchmarks).
    Synth {
        #[arg(long)]
        out: PathBuf,
        #[arg(long)]
        rows: usize,
        #[arg(long, default_value_t = 1)]
        seed: u64,
        #[arg(long, default_value = "2024-01-01")]
        from: String,
        #[arg(long, default_value = "2026-01-01")]
        to: String,
        #[arg(long, default_value_t = 50_000)]
        page_size: usize,
    },
}

fn main() -> Result<()> {
    let start = Instant::now();
    match Cli::parse().command {
        Command::Fetch {
            out,
            from,
            to,
            page_size,
        } => {
            let scope = Scope::from_dates(NYC_311.created_field(), &from, &to)?;
            let mut opts = FetchOptions::new(&NYC_311, scope);
            opts.page_size = page_size;
            let token = std::env::var("SOCRATA_APP_TOKEN")
                .ok()
                .filter(|t| !t.is_empty());
            let mut transport = UreqTransport::new(token);
            let rec = socrata::fetch(
                &NYC_311,
                &opts,
                &mut transport,
                &mut std::thread::sleep,
                &out,
            )?;
            println!(
                "fetched {} records in {} pages into {}",
                rec.raw_records,
                rec.pages.len(),
                out.display()
            );
        }
        Command::Build {
            raw,
            out,
            compression,
        } => {
            let compression = match compression.as_str() {
                "lz4" => Compression::Lz4,
                "none" => Compression::None,
                other => anyhow::bail!("unknown compression {other:?}: use lz4 or none"),
            };
            let built = assemble::build_with(&NYC_311, &raw, &out, compression)?;
            let m = &built.manifest;
            println!("snapshot {}", m.snapshot_hash);
            println!(
                "  {} rows, {} rejected, {} cleaning-log entries -> {}",
                m.row_count,
                m.cleaning.rejected_rows,
                m.cleaning.cleaning_log_rows,
                built.dir.display()
            );
        }
        Command::Verify { dir } => {
            let v = verify::verify(&dir)?;
            println!(
                "ok: snapshot {} ({} rows, {} columns, {} chunks re-hashed)",
                v.manifest.snapshot_hash, v.manifest.row_count, v.columns_checked, v.chunks_checked
            );
        }
        Command::Synth {
            out,
            rows,
            seed,
            from,
            to,
            page_size,
        } => {
            let scope = Scope::from_dates(NYC_311.created_field(), &from, &to)?;
            let records = synth::Generator311::new(seed, &scope).take(rows);
            let meta = synth::metadata_for(&NYC_311, 1_768_435_200);
            let rec = synth::write_raw(
                &out,
                &NYC_311,
                &scope,
                &meta,
                synth::stream_pages(records, page_size),
            )?;
            println!(
                "wrote {} synthetic records in {} pages to {}",
                rec.raw_records,
                rec.pages.len(),
                out.display()
            );
        }
    }
    eprintln!("done in {:.2?}", start.elapsed());
    Ok(())
}
