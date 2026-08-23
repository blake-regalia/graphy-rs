//! Fuzz target (doc 04 §5): arbitrary bytes through parse_query and
//! parse_update must not panic, hang, or overflow the stack (the depth
//! guard is the defense) — errors are the only acceptable failure.

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(src) = std::str::from_utf8(data) {
        let _ = graphy_sparql_syntax::parse_query(src);
        let _ = graphy_sparql_syntax::parse_update(src);
    }
});
