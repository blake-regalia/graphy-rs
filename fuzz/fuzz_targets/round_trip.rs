//! Fuzz target (c), doc 03 §5: parse → serialize → parse must be a fixpoint:
//! reparsing the canonical N-Quads serialization yields the same quads and
//! the same serialization.

#![no_main]

use graphy_turtle::{NQuadsParser, NQuadsWriter, Options, QuadRef};
use libfuzzer_sys::fuzz_target;

fn parse_and_write(data: &[u8]) -> Option<(Vec<Vec<u8>>, Vec<u8>)> {
    let mut p = NQuadsParser::new(Options::default()).expect("no base");
    let mut w = NQuadsWriter::new(Vec::new());
    let mut quads = Vec::new();
    let mut push = |q: QuadRef<'_>, w: &mut NQuadsWriter<Vec<u8>>| {
        let mut v = Vec::new();
        v.extend_from_slice(q.s);
        v.push(0xFF);
        v.extend_from_slice(q.p);
        v.push(0xFF);
        v.extend_from_slice(q.o);
        v.push(0xFF);
        if let Some(g) = q.g {
            v.extend_from_slice(g);
        }
        quads.push(v);
        w.write_quad(&q).expect("Vec write is infallible");
    };
    p.feed(data).ok()?;
    for q in p.drain() {
        push(q, &mut w);
    }
    p.finish().ok()?;
    for q in p.drain() {
        push(q, &mut w);
    }
    Some((quads, w.into_inner()))
}

fuzz_target!(|data: &[u8]| {
    let Some((quads1, text1)) = parse_and_write(data) else {
        return;
    };
    let (quads2, text2) =
        parse_and_write(&text1).expect("canonical serialization must reparse");
    assert_eq!(quads1, quads2, "round-trip changed the quads");
    assert_eq!(text1, text2, "canonical serialization is not a fixpoint");
});
