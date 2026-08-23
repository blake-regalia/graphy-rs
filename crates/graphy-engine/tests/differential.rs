//! Differential fuzzing: generated queries over a generated store,
//! reference evaluator vs vectorized engine — result multisets must be
//! identical (doc 05 §9: "the highest-value bug-finder for engines").
//! A seeded xorshift generator keeps runs reproducible; the CI-tier
//! count is modest, `GRAPHY_DIFF_N` scales it up for soak runs.

use std::path::PathBuf;

use graphy_algebra::{rewrite, translate_query};
use graphy_engine::exec::evaluate_vec;
use graphy_engine::{evaluate_ref, Output};
use graphy_sparql_syntax::parse_query;
use graphy_store::{BuilderConfig, Profile, SegmentBuilder, Store};

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x.wrapping_mul(0x2545F4914F6CDD1D)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

/// Small dense graph: predictable joins, plenty of shared objects.
fn build_store(dir: &PathBuf) -> Store {
    let mut rng = Rng(0xDECAFBAD);
    let iri = |s: String| format!(">http://f/{s}").into_bytes();
    let int = |i: i64| format!("^>http://www.w3.org/2001/XMLSchema#integer\"{i}").into_bytes();
    let mut cfg = BuilderConfig::new(dir);
    cfg.profile = Profile::Balanced;
    cfg.sort_budget = 1 << 16;
    let mut b = SegmentBuilder::new(cfg).unwrap();
    let mut quads = std::collections::BTreeSet::new();
    for _ in 0..600 {
        let s = iri(format!("n{}", rng.below(40)));
        let p = iri(format!("p{}", rng.below(5)));
        let o = if rng.below(4) == 0 {
            int(rng.below(50) as i64)
        } else {
            iri(format!("n{}", rng.below(40)))
        };
        let g = match rng.below(5) {
            0 => Some(iri(format!("g{}", rng.below(2)))),
            _ => None,
        };
        quads.insert((s, p, o, g));
    }
    for (s, p, o, g) in &quads {
        b.push_quad(s, p, o, g.as_deref()).unwrap();
    }
    b.finish().unwrap();
    Store::open(dir).unwrap()
}

fn gen_pattern(rng: &mut Rng, vars: &[&str]) -> String {
    let term = |rng: &mut Rng, vars: &[&str]| -> String {
        match rng.below(3) {
            0 => format!("?{}", vars[rng.below(vars.len() as u64) as usize]),
            1 => format!("<http://f/n{}>", rng.below(40)),
            _ => format!("?{}", vars[rng.below(vars.len() as u64) as usize]),
        }
    };
    let s = term(rng, vars);
    let p = format!("<http://f/p{}>", rng.below(5));
    let o = if rng.below(6) == 0 {
        format!("{}", rng.below(50))
    } else {
        term(rng, vars)
    };
    format!("{s} {p} {o} .")
}

fn gen_query(rng: &mut Rng) -> (String, Option<(String, u64)>) {
    let vars = ["a", "b", "c", "d"];
    let n_pat = 1 + rng.below(4);
    let mut body = String::new();
    for _ in 0..n_pat {
        body.push_str(&gen_pattern(rng, &vars));
        body.push(' ');
    }
    match rng.below(6) {
        0 => body.push_str("OPTIONAL { ?a <http://f/p0> ?opt } "),
        1 => body.push_str(&format!(
            "FILTER(?{} != <http://f/n{}>) ",
            vars[rng.below(4) as usize],
            rng.below(40)
        )),
        2 => body.push_str("{ ?a <http://f/p1> ?u } UNION { ?a <http://f/p2> ?u } "),
        3 => body.push_str(&format!("BIND(?{} AS ?bnd) ", vars[rng.below(4) as usize])),
        4 => body.push_str("GRAPH ?g { ?a <http://f/p0> ?gg } "),
        _ => {}
    }
    let limit = match rng.below(5) {
        1 => Some(1 + rng.below(20)),
        _ => None,
    };
    let order = if limit.is_none() && rng.below(5) == 0 {
        "ORDER BY ?a"
    } else {
        ""
    };
    let distinct = if rng.below(4) == 0 { "DISTINCT " } else { "" };
    let base = format!("SELECT {distinct}* WHERE {{ {body} }} {order}");
    let with_limit = match limit {
        Some(l) => format!("{base} LIMIT {l}"),
        None => base.clone(),
    };
    (with_limit, limit.map(|l| (base, l)))
}

fn norm(out: Output) -> Vec<Vec<Option<Vec<u8>>>> {
    match out {
        Output::Solutions { mut rows, .. } => {
            rows.sort();
            rows
        }
        Output::Boolean(b) => vec![vec![Some(vec![b as u8])]],
        Output::Triples(mut t) => {
            t.sort();
            t.into_iter()
                .map(|(s, p, o)| vec![Some(s), Some(p), Some(o)])
                .collect()
        }
    }
}

#[test]
fn differential_ref_vs_vectorized() {
    let dir = std::env::temp_dir().join(format!("graphy-diff-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let store = build_store(&dir);
    let snap = store.snapshot();

    let n: u64 = std::env::var("GRAPHY_DIFF_N")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1000);
    let mut rng = Rng(0x5EED_5EED);
    let mut nonempty = 0usize;
    let run = |snap: &graphy_store::Snapshot, src: &str, vec: bool| {
        let parsed = parse_query(src)
            .unwrap_or_else(|e| panic!("generator produced unparseable query `{src}`: {e}"));
        let mut tq = translate_query(&parsed).unwrap_or_else(|e| panic!("translate `{src}`: {e}"));
        tq.root = rewrite(tq.root.clone());
        if vec {
            evaluate_vec(snap, &tq)
        } else {
            evaluate_ref(snap, &tq)
        }
    };
    for i in 0..n {
        let (src, limit_no_order) = gen_query(&mut rng);
        let r = run(&snap, &src, false);
        let v = run(&snap, &src, true);
        match (r, v) {
            (Ok(a), Ok(b)) => {
                let (a, b) = (norm(a), norm(b));
                match &limit_no_order {
                    // LIMIT without ORDER BY: any `limit`-sized subset
                    // of the full multiset is a valid answer — check
                    // size + multiset containment for BOTH engines.
                    Some((full_src, limit)) => {
                        let full = norm(run(&snap, full_src, false).unwrap());
                        let want = (*limit as usize).min(full.len());
                        for (engine, rows) in [("ref", &a), ("vec", &b)] {
                            assert_eq!(rows.len(), want, "iteration {i} [{engine}]: `{src}`");
                            let mut pool = full.clone();
                            for row in rows {
                                let at = pool
                                    .iter()
                                    .position(|r| r == row)
                                    .unwrap_or_else(|| panic!(
                                        "iteration {i} [{engine}]: `{src}` emitted a row outside the full result set"
                                    ));
                                pool.swap_remove(at);
                            }
                        }
                    }
                    // Otherwise: identical multisets (ORDER BY ties may
                    // differ in sequence; norm sorts).
                    None => assert_eq!(a, b, "iteration {i}: engines disagree on `{src}`"),
                }
                if !a.is_empty() {
                    nonempty += 1;
                }
            }
            (Err(a), Err(_b)) => panic!("iteration {i}: both engines errored on `{src}`: {a}"),
            (r, v) => panic!(
                "iteration {i}: engines disagree on `{src}`: ref={:?} vec={:?}",
                r.map(|_| "ok"),
                v.map(|_| "ok"),
            ),
        }
    }
    // The generator must actually exercise the join machinery.
    assert!(
        nonempty * 5 >= n as usize,
        "only {nonempty}/{n} queries returned rows — generator drifted"
    );
    std::fs::remove_dir_all(&dir).ok();
}
