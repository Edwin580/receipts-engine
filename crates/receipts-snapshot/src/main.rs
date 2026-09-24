//! Offline snapshot CLI (M0). Fetches a dataset from the Socrata API, applies
//! the documented cleaning rules (docs/snapshot/cleaning-rules.md), and writes
//! an Arrow IPC snapshot plus a BLAKE3-hashed manifest (docs/snapshot/schema.md).
//!
//! Not implemented yet: the schema and cleaning rules are up for review first.

fn main() {
    eprintln!("receipts-snapshot: not implemented yet (M0 is awaiting schema review)");
    std::process::exit(2);
}
