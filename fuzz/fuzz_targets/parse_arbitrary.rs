//! Fuzz target (a), doc 03 §5: arbitrary bytes through every parser in both
//! strict and lenient mode must not panic, hang, or UB — errors are the only
//! acceptable failure.

#![no_main]

use graphy_turtle::{NQuadsParser, NTriplesParser, Options, TriGParser, TurtleParser};
use libfuzzer_sys::fuzz_target;

macro_rules! drive {
    ($parser:expr, $data:expr) => {{
        if let Ok(mut p) = $parser {
            if p.feed($data).is_ok() {
                let _ = p.drain().count();
                if p.finish().is_ok() {
                    let _ = p.drain().count();
                }
            }
            let _ = p.errors().len();
        }
    }};
}

fuzz_target!(|data: &[u8]| {
    for lenient in [false, true] {
        let opts = Options {
            base: Some("http://fuzz.example/dir/doc".to_owned()),
            spec12: true,
            lenient,
            ..Options::default()
        };
        drive!(TurtleParser::new(opts.clone()), data);
        drive!(TriGParser::new(opts.clone()), data);
        drive!(NTriplesParser::new(opts.clone()), data);
        drive!(NQuadsParser::new(opts), data);
    }
});
