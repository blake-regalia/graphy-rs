//! `MANIFEST.json` (doc 02 §6): names the segment's profile, counts, and
//! component digests. Written last via write-temp + fsync + atomic rename —
//! a segment without a valid manifest is garbage by definition.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::Write;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::format::StoreError;

pub const MANIFEST_NAME: &str = "MANIFEST.json";

/// Segment format version (docs/08 §1; must match the component headers).
pub const FORMAT_VERSION: u32 = 2;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub format_version: u32,
    pub generation: u64,
    pub profile: String,
    /// Ordering names present under `idx/`.
    pub orderings: Vec<String>,
    /// False for triples-only datasets: `graphs/` absent, quad layer no-ops.
    pub has_graphs: bool,
    pub counts: Counts,
    /// Relative component path → (byte length, xxh3 hex) for verification.
    pub components: BTreeMap<String, Component>,
    /// Rebuildable sidecars (`dict/*.hash`): tracked for `verify`, but not
    /// checksum-critical — readers fall back to PFC search without them.
    #[serde(default)]
    pub sidecars: BTreeMap<String, Component>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Counts {
    pub quads: u64,
    pub triples: u64,
    pub shared: u64,
    pub subjects: u64,
    pub predicates: u64,
    pub objects: u64,
    pub graphs: u64,
    pub triple_terms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Component {
    pub bytes: u64,
    pub xxh3: String,
}

impl Manifest {
    pub fn save(&self, dir: &Path) -> Result<(), StoreError> {
        let tmp = dir.join(format!("{MANIFEST_NAME}.tmp"));
        let path = dir.join(MANIFEST_NAME);
        let run = || -> std::io::Result<()> {
            let mut f = File::create(&tmp)?;
            serde_json::to_writer_pretty(&mut f, self)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
            f.write_all(b"\n")?;
            f.sync_all()?;
            std::fs::rename(&tmp, &path)?;
            // Persist the rename.
            File::open(dir)?.sync_all()?;
            Ok(())
        };
        run().map_err(|e| StoreError::io(&path, e))
    }

    pub fn load(dir: &Path) -> Result<Manifest, StoreError> {
        let path = dir.join(MANIFEST_NAME);
        let bytes = std::fs::read(&path).map_err(|e| StoreError::io(&path, e))?;
        Manifest::from_bytes(&bytes)
    }

    /// Parse from raw manifest bytes (embedded segment images, docs/11).
    pub fn from_bytes(bytes: &[u8]) -> Result<Manifest, StoreError> {
        let m: Manifest =
            serde_json::from_slice(bytes).map_err(|e| StoreError::Manifest(e.to_string()))?;
        if m.format_version != FORMAT_VERSION {
            return Err(StoreError::Manifest(format!(
                "unsupported format version {}",
                m.format_version
            )));
        }
        Ok(m)
    }
}
