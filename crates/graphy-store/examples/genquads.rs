//! Synthetic N-Quads corpus generator (the M2 benchmark shape, scaled):
//! `genquads <n_quads> [out.nq]`. Used for the 10⁸-scale load validations
//! recorded in BENCHMARKS.md — subjects/objects scale with the corpus so
//! the dictionary grows realistically (~n/5 distinct subjects).

use std::io::{BufWriter, Write};

fn main() {
    let mut args = std::env::args().skip(1);
    let n: u64 = args
        .next()
        .expect("usage: genquads <n_quads> [out.nq]")
        .parse()
        .expect("quad count");
    let out: Box<dyn Write> = match args.next() {
        Some(p) => Box::new(std::fs::File::create(p).expect("create output")),
        None => Box::new(std::io::stdout().lock()),
    };
    let mut w = BufWriter::with_capacity(1 << 20, out);
    // --clustered: subjects arrive in contiguous blocks (like real dumps,
    // which group statements by subject) instead of round-robin — the
    // adversarial-vs-typical axis for bounded-memory interning.
    let clustered = std::env::args().any(|a| a == "--clustered");

    let n_subjects = (n / 10).max(1);
    let n_objects = (n / 20).max(1);
    let n_literals = (n / 40).max(1);
    for i in 0..n {
        let s = if clustered {
            i / 10 % n_subjects
        } else {
            i % n_subjects
        };
        let p = i % 17;
        match i % 4 {
            0 => writeln!(
                w,
                "<http://ex/s{s}> <http://ex/p{p}> <http://ex/s{}>{}",
                (i * 7) % n_subjects,
                graph(i)
            ),
            1 => writeln!(
                w,
                "<http://ex/s{s}> <http://ex/p{p}> <http://ex/o{}>{}",
                i % n_objects,
                graph(i)
            ),
            2 => writeln!(
                w,
                "<http://ex/s{s}> <http://ex/p{p}> \"literal value number {}\"{}",
                i % n_literals,
                graph(i)
            ),
            _ => writeln!(
                w,
                "<http://ex/s{s}> <http://ex/p{p}> \
                 \"{i}\"^^<http://www.w3.org/2001/XMLSchema#integer>{}",
                graph(i)
            ),
        }
        .expect("write");
    }
    w.flush().expect("flush");
}

fn graph(i: u64) -> &'static str {
    match i % 6 {
        0 => " .",
        1 => " <http://ex/g1> .",
        2 => " <http://ex/g2> .",
        3 => " <http://ex/g3> .",
        4 => " <http://ex/g4> .",
        _ => " <http://ex/g5> .",
    }
}
