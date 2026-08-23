//! Fuzz target (b), doc 03 §5: the same document fed whole and split at an
//! arbitrary boundary must yield byte-identical quad streams and the same
//! error outcome.

#![no_main]

use graphy_turtle::{Options, QuadRef, TriGParser, TurtleParser};
use libfuzzer_sys::fuzz_target;

macro_rules! outcome {
    ($ty:ident, $doc:expr, $splits:expr) => {{
        let mut p = $ty::new(Options {
            base: Some("http://fuzz.example/".to_owned()),
            ..Options::default()
        })
        .expect("valid base");
        let mut quads: Vec<Vec<u8>> = Vec::new();
        let mut error: Option<String> = None;
        let mut at = 0;
        'feeds: {
            for &split in $splits {
                match p.feed(&$doc[at..split]) {
                    Ok(()) => quads.extend(p.drain().map(render)),
                    Err(e) => {
                        quads.extend(p.drain().map(render));
                        error = Some(e.to_string());
                        break 'feeds;
                    }
                }
                at = split;
            }
            match p.finish() {
                Ok(()) => quads.extend(p.drain().map(render)),
                Err(e) => {
                    quads.extend(p.drain().map(render));
                    error = Some(e.to_string());
                }
            }
        }
        (quads, error)
    }};
}

fn render(q: QuadRef<'_>) -> Vec<u8> {
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
    v
}

fuzz_target!(|data: &[u8]| {
    if data.len() < 2 {
        return;
    }
    let doc = &data[1..];
    let split = (data[0] as usize * doc.len().max(1) / 256).min(doc.len());
    let whole = outcome!(TurtleParser, doc, &[doc.len()]);
    let parts = outcome!(TurtleParser, doc, &[split, doc.len()]);
    assert_eq!(whole, parts, "turtle chunk-split divergence at {split}");
    let whole = outcome!(TriGParser, doc, &[doc.len()]);
    let parts = outcome!(TriGParser, doc, &[split, doc.len()]);
    assert_eq!(whole, parts, "trig chunk-split divergence at {split}");
});
