//! Plan cache (doc 05 §5.6 / §8): canonical-algebra-keyed, **snapshot-
//! scoped** — the key includes the snapshot's storage identity,
//! generation, and epoch, so a cached plan can never leak stale constant
//! resolutions (graph columns, provably-empty prunes) across stores,
//! commits, or an ephemeral compaction that reissues overlay columns at
//! the same epoch. Correctness by construction, per the §8 caching
//! philosophy; cross-snapshot reuse under cardinality-band guards is a
//! tracked refinement.

use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::{Mutex, OnceLock};

use graphy_algebra::TranslatedQuery;
use graphy_store::Snapshot;

use crate::eval::{Evaluator, Scope};
use crate::exec::plan::{plan, PScope, Phys, PlanError};

/// Bounded process-wide cache (cleared wholesale at capacity — plans
/// re-derive cheaply from exact counts).
const CAPACITY: usize = 256;

static CACHE: OnceLock<Mutex<HashMap<u64, Phys>>> = OnceLock::new();

fn key(snap: &Snapshot, q: &TranslatedQuery) -> u64 {
    let mut h = DefaultHasher::new();
    snap.storage_identity().hash(&mut h);
    snap.generation().hash(&mut h);
    snap.epoch().hash(&mut h);
    // The algebra Debug form is a deterministic structural rendering;
    // the dataset clauses participate in planning (graph visibility).
    format!("{:?}|{:?}", q.root, q.dataset).hash(&mut h);
    h.finish()
}

/// Plan through the cache.
pub(crate) fn plan_cached(
    ev: &Evaluator<'_>,
    snap: &Snapshot,
    q: &TranslatedQuery,
) -> Result<Phys, PlanError> {
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let k = key(snap, q);
    if let Some(hit) = cache.lock().unwrap().get(&k) {
        return Ok(hit.clone());
    }
    let phys = plan(ev, &q.root, PScope::Fixed(Scope::Default))?;
    let mut map = cache.lock().unwrap();
    if map.len() >= CAPACITY {
        map.clear();
    }
    map.insert(k, phys.clone());
    Ok(phys)
}
