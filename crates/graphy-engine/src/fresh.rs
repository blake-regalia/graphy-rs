//! Process-session identity shared by every engine blank-node minting domain.

use std::sync::OnceLock;

static SESSION: OnceLock<u128> = OnceLock::new();

pub(crate) fn session() -> u128 {
    *SESSION.get_or_init(|| {
        let mut bytes = [0; 16];
        getrandom::fill(&mut bytes)
            .expect("secure randomness is required for blank-node freshness");
        u128::from_le_bytes(bytes)
    })
}
