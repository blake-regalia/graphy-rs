//! Criterion microbenches for the succinct primitives (M0 exit criterion:
//! documented performance curves). Sizes span small-cache to out-of-cache
//! regimes; queries are pre-generated with a deterministic xorshift and
//! cycled so measurement is pure lookup cost.

use std::hint::black_box;

use criterion::{criterion_group, criterion_main, Criterion};
use graphy_succinct::{BitVector, PackedInts, PfcBuilder};

fn xorshift(state: &mut u64) -> u64 {
    *state ^= *state << 13;
    *state ^= *state >> 7;
    *state ^= *state << 17;
    *state
}

const QUERIES: usize = 1024;

fn bench_bitvec(c: &mut Criterion) {
    let mut group = c.benchmark_group("bitvec");
    for n in [1usize << 20, 1 << 24, 1 << 27] {
        let mut state = 0x9E37_79B9_7F4A_7C15;
        let bv: BitVector = (0..n).map(|_| xorshift(&mut state) & 1 == 1).collect();
        let ones = bv.count_ones();
        let zeros = bv.count_zeros();

        let idx: Vec<usize> = (0..QUERIES)
            .map(|_| (xorshift(&mut state) % n as u64) as usize)
            .collect();
        let mut q = 0;
        group.bench_function(format!("rank1/{n}"), |b| {
            b.iter(|| {
                q = (q + 1) % QUERIES;
                black_box(bv.rank1(black_box(idx[q])))
            })
        });

        let ks1: Vec<u64> = (0..QUERIES).map(|_| xorshift(&mut state) % ones).collect();
        group.bench_function(format!("select1/{n}"), |b| {
            b.iter(|| {
                q = (q + 1) % QUERIES;
                black_box(bv.select1(black_box(ks1[q])))
            })
        });

        let ks0: Vec<u64> = (0..QUERIES).map(|_| xorshift(&mut state) % zeros).collect();
        group.bench_function(format!("select0/{n}"), |b| {
            b.iter(|| {
                q = (q + 1) % QUERIES;
                black_box(bv.select0(black_box(ks0[q])))
            })
        });
    }
    group.finish();
}

fn bench_packed_ints(c: &mut Criterion) {
    let mut group = c.benchmark_group("packed_ints");
    for (n, width) in [(1usize << 20, 11u32), (1 << 24, 37), (1 << 24, 64)] {
        let mut state = 0xDEAD_BEEF_CAFE_F00D;
        let mask = if width == 64 {
            u64::MAX
        } else {
            (1 << width) - 1
        };
        let packed = PackedInts::with_width((0..n).map(|_| xorshift(&mut state) & mask), width);
        let idx: Vec<usize> = (0..QUERIES)
            .map(|_| (xorshift(&mut state) % n as u64) as usize)
            .collect();
        let mut q = 0;
        group.bench_function(format!("get/{n}xw{width}"), |b| {
            b.iter(|| {
                q = (q + 1) % QUERIES;
                black_box(packed.get(black_box(idx[q])))
            })
        });
    }
    group.finish();
}

fn bench_pfc(c: &mut Criterion) {
    let mut group = c.benchmark_group("pfc");
    let n = 1 << 17; // 131k keys
    let mut keys: Vec<Vec<u8>> = (0..n)
        .map(|i| {
            format!(
                "http://ex.example/resource/{:07}/label-{}",
                i * 2654435761u64 % n,
                i
            )
            .into_bytes()
        })
        .collect();
    keys.sort();
    keys.dedup();

    for block_size in [16usize, 32, 128] {
        let mut b = PfcBuilder::new(block_size);
        for k in &keys {
            b.push(k);
        }
        let pfc = b.build();
        let mut state = 42;
        let idx: Vec<usize> = (0..QUERIES)
            .map(|_| (xorshift(&mut state) % keys.len() as u64) as usize)
            .collect();
        let mut q = 0;
        group.bench_function(format!("get/{n}xb{block_size}"), |bch| {
            bch.iter(|| {
                q = (q + 1) % QUERIES;
                black_box(pfc.get(black_box(idx[q])))
            })
        });
        group.bench_function(format!("locate/{n}xb{block_size}"), |bch| {
            bch.iter(|| {
                q = (q + 1) % QUERIES;
                black_box(pfc.locate(black_box(&keys[idx[q]])))
            })
        });
    }
    group.finish();
}

criterion_group!(benches, bench_bitvec, bench_packed_ints, bench_pfc);
criterion_main!(benches);
