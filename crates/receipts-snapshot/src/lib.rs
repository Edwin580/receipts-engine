//! Offline snapshot pipeline (M0): Socrata → raw directory → cleaned,
//! content-hashed Arrow IPC snapshot + manifest. See `docs/snapshot/`.

pub mod arrow_io;
pub mod assemble;
pub mod clean;
pub mod known_issues;
pub mod manifest;
pub mod raw;
pub mod rules;
pub mod socrata;
pub mod spec;
pub mod synth;
pub mod verify;

/// Crate version plus the git commit it was built from, if known.
pub const TOOL_VERSION: &str = concat!(
    "receipts-snapshot ",
    env!("CARGO_PKG_VERSION"),
    " (git ",
    env!("RECEIPTS_GIT_COMMIT"),
    ")"
);
