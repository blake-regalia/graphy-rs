//! Property tests for graphy-succinct against brute-force models (plan M0
//! exit criteria). All ignored under Miri (too slow); unit tests cover the
//! same code paths there.
#![cfg(not(miri))]

use graphy_succinct::{BitVector, ExtSorter, PackedInts, PfcBuilder, WaveletMatrix};
use proptest::prelude::*;

proptest! {
    #[test]
    fn bitvec_rank_select_vs_brute_force(bits in proptest::collection::vec(any::<bool>(), 0..6000)) {
        let bv: BitVector = bits.iter().copied().collect();
        let mut ones = 0u64;
        for (i, &b) in bits.iter().enumerate() {
            prop_assert_eq!(bv.get(i), b);
            prop_assert_eq!(bv.rank1(i), ones);
            prop_assert_eq!(bv.rank0(i), i as u64 - ones);
            if b {
                prop_assert_eq!(bv.select1(ones), Some(i));
            } else {
                prop_assert_eq!(bv.select0(i as u64 - ones), Some(i));
            }
            ones += u64::from(b);
        }
        prop_assert_eq!(bv.count_ones(), ones);
        prop_assert_eq!(bv.select1(ones), None);
        prop_assert_eq!(bv.select0(bits.len() as u64 - ones), None);
    }

    #[test]
    fn packed_ints_round_trip(values in proptest::collection::vec(any::<u64>(), 0..500), width_slack in 0u32..8) {
        // Pack at minimal width plus some slack (capped at 64).
        let min_width = values.iter().map(|v| 64 - v.leading_zeros()).max().unwrap_or(0);
        let width = (min_width + width_slack).min(64);
        let packed = PackedInts::with_width(values.iter().copied(), width);
        prop_assert_eq!(packed.len(), values.len());
        for (i, &v) in values.iter().enumerate() {
            prop_assert_eq!(packed.get(i), v);
        }
    }

    #[test]
    fn pfc_round_trip_and_locate(
        mut keys in proptest::collection::vec(proptest::collection::vec(any::<u8>(), 0..40), 1..300),
        probes in proptest::collection::vec(proptest::collection::vec(any::<u8>(), 0..40), 0..50),
        block_size in 1usize..70,
    ) {
        keys.sort();
        keys.dedup();
        let mut b = PfcBuilder::new(block_size);
        for k in &keys {
            b.push(k);
        }
        let pfc = b.build();
        prop_assert_eq!(pfc.len(), keys.len());
        for (i, k) in keys.iter().enumerate() {
            let got = pfc.get(i);
            prop_assert_eq!(got.as_ref(), Some(k));
            prop_assert_eq!(pfc.locate(k), Some(i), "locate hit {:?}", k);
        }
        prop_assert_eq!(&pfc.iter().collect::<Vec<_>>(), &keys);
        // Misses agree with binary search on the model.
        for p in &probes {
            let expected = keys.binary_search(p).ok();
            prop_assert_eq!(pfc.locate(p), expected, "probe {:?}", p);
        }
    }

    #[test]
    fn wavelet_vs_brute_force(values in proptest::collection::vec(0u64..256, 0..800), probes in proptest::collection::vec((0u64..256, 0usize..800), 0..30)) {
        let wm = WaveletMatrix::new(&values, 8);
        for (i, &v) in values.iter().enumerate() {
            prop_assert_eq!(wm.access(i), v);
        }
        for &(sym, i) in &probes {
            let i = i.min(values.len());
            let expected = values[..i].iter().filter(|&&v| v == sym).count() as u64;
            prop_assert_eq!(wm.rank(sym, i), expected);
        }
        // select is the inverse of rank for every occurrence.
        if let Some(&sym) = values.first() {
            let occurrences: Vec<usize> = values
                .iter()
                .enumerate()
                .filter_map(|(i, &v)| (v == sym).then_some(i))
                .collect();
            for (k, &pos) in occurrences.iter().enumerate() {
                prop_assert_eq!(wm.select(sym, k as u64), Some(pos));
            }
            prop_assert_eq!(wm.select(sym, occurrences.len() as u64), None);
        }
    }

    #[test]
    fn extsort_matches_std_sort(
        values in proptest::collection::vec(any::<u64>(), 0..3000),
        budget_records in 1usize..200,
    ) {
        let dir = std::env::temp_dir().join(format!(
            "graphy-extsort-prop-{}",
            std::process::id()
        ));
        let mut sorter: ExtSorter<u64> = ExtSorter::new(&dir, budget_records * 8).unwrap();
        for &v in &values {
            sorter.push(v).unwrap();
        }
        let got: Vec<u64> = sorter.finish().unwrap().map(Result::unwrap).collect();
        let mut expected = values;
        expected.sort_unstable();
        prop_assert_eq!(got, expected);
    }
}
