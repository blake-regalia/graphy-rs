//! Shared discovery for optional, pinned external oracle checkouts.

use std::path::{Path, PathBuf};

/// Returns a checkout under `GRAPHY_ORACLES_DIR` (or
/// `testdata/oracles`) when present. Ordinary crate consumers do not need
/// the corpora; release/CI jobs can set `GRAPHY_REQUIRE_ORACLES=1` to turn
/// a missing checkout into a hard failure.
pub fn checkout(name: &str) -> Option<PathBuf> {
    let base = std::env::var_os("GRAPHY_ORACLES_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testdata/oracles"));
    let path = base.join(name);
    if path.is_dir() {
        return Some(path.canonicalize().expect("canonical oracle checkout"));
    }
    if std::env::var_os("GRAPHY_REQUIRE_ORACLES").is_some() {
        panic!(
            "required oracle checkout `{name}` is missing at {}; run scripts/fetch-oracles.sh",
            path.display()
        );
    }
    eprintln!(
        "skipping {name} oracle: {} is absent (run scripts/fetch-oracles.sh)",
        path.display()
    );
    None
}
