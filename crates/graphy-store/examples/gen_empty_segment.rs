//! Regenerate the embedded empty segment (docs/11 M12a): the byte image the
//! ephemeral store uses as its base. Run after any segment-format change:
//!
//! ```sh
//! cargo run -p graphy-store --example gen_empty_segment
//! ```
//!
//! The `embedded_empty_segment_matches_builders` test fails when the
//! checked-in image drifts from what the live builders produce.

use graphy_store::{BuilderConfig, SegmentBuilder};

fn main() {
    let out = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/empty_segment");
    if out.exists() {
        std::fs::remove_dir_all(&out).expect("clear old fixture");
    }
    std::fs::create_dir_all(&out).expect("create fixture dir");
    let builder = SegmentBuilder::new(BuilderConfig::new(&out)).expect("builder");
    let manifest = builder.finish().expect("finish empty segment");
    println!("built empty segment: profile={}", manifest.profile);
    let mut rels = Vec::new();
    collect(&out, &out, &mut rels);
    rels.sort();
    for r in &rels {
        println!("  {r}");
    }
}

fn collect(root: &std::path::Path, dir: &std::path::Path, out: &mut Vec<String>) {
    for e in std::fs::read_dir(dir).expect("readable") {
        let p = e.expect("entry").path();
        if p.is_dir() {
            collect(root, &p, out);
        } else {
            out.push(
                p.strip_prefix(root)
                    .expect("under root")
                    .to_string_lossy()
                    .into_owned(),
            );
        }
    }
}
