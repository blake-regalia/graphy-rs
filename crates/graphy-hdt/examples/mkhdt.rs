//! Write a small HDT file (writer interop validation harness).
use graphy_core::Term;
use graphy_hdt::HdtWriter;

fn main() {
    let out = std::env::args().nth(1).expect("usage: mkhdt <out.hdt>");
    let iri = |s: &str| Term::iri(s).unwrap().as_concise().to_vec();
    let mut w = HdtWriter::new();
    for i in 0..500u32 {
        w.add_triple(
            &iri(&format!("http://example.org/s{i}")),
            &iri(&format!("http://example.org/p{}", i % 5)),
            &iri(&format!("http://example.org/o{}", i % 50)),
        )
        .unwrap();
    }
    w.add_triple(
        &iri("http://example.org/o1"),
        &iri("http://example.org/p0"),
        Term::literal_lang("héllo", "en", None)
            .unwrap()
            .as_concise(),
    )
    .unwrap();
    w.add_triple(
        &iri("http://example.org/s0"),
        &iri("http://example.org/p9"),
        Term::literal_typed("3.14", "http://www.w3.org/2001/XMLSchema#decimal")
            .unwrap()
            .as_concise(),
    )
    .unwrap();
    w.write_to_path(std::path::Path::new(&out)).unwrap();
    println!("wrote {out}");
}
