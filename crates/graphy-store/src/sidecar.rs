//! `dict/*.hash` term→ordinal sidecars (doc 02 RQ3, docs/08 §4): one
//! open-addressing table per PFC section for O(1) `term → id` resolution.
//!
//! Slot encoding: `0` = empty, else `fp << 56 | (ordinal + 1)` with
//! `fp = xxh3_64(term) >> 56`. Table hits are never authoritative — fp
//! collisions are possible, so every candidate is confirmed against the PFC
//! entry bytes. Sidecars are rebuildable and excluded from the
//! checksum-critical set: readers fall back to PFC binary search when one is
//! missing or malformed.

use std::io::{self, Write};

use graphy_succinct::serial::write_u64;
use graphy_succinct::{Bytes, Cursor, Pfc, Words};
use xxhash_rust::xxh3::xxh3_64;

const FP_SHIFT: u32 = 56;
const ORDINAL_MASK: u64 = (1 << FP_SHIFT) - 1;

/// An open-addressing `xxh3(term) → section ordinal` table. The slot array
/// may be a zero-copy view (mmap open mode).
#[derive(Debug)]
pub(crate) struct HashSidecar {
    slots: Words,
}

impl HashSidecar {
    /// Build from a section's sorted terms (deterministic: ordinals insert
    /// in order with linear probing).
    pub fn build(pfc: &Pfc) -> HashSidecar {
        let n = pfc.len() as u64;
        let n_slots = (n * 4 / 3 + 1).next_power_of_two().max(8);
        let mask = n_slots - 1;
        let mut slots = vec![0u64; n_slots as usize];
        for (ordinal, term) in pfc.iter().enumerate() {
            let h = xxh3_64(&term);
            let mut at = (h & mask) as usize;
            while slots[at] != 0 {
                at = (at + 1) & mask as usize;
            }
            slots[at] = (h >> FP_SHIFT) << FP_SHIFT | (ordinal as u64 + 1);
        }
        HashSidecar {
            slots: Words::from_vec(slots),
        }
    }

    /// Ordinal of `term` in the section, confirmed against the PFC bytes.
    pub fn locate(&self, term: &[u8], pfc: &Pfc) -> Option<usize> {
        let h = xxh3_64(term);
        let mask = self.slots.len() as u64 - 1;
        let fp = h >> FP_SHIFT << FP_SHIFT;
        let mut at = (h & mask) as usize;
        loop {
            let slot = self.slots[at];
            if slot == 0 {
                return None;
            }
            if slot & !ORDINAL_MASK == fp {
                let ordinal = ((slot & ORDINAL_MASK) - 1) as usize;
                if pfc.get(ordinal).as_deref() == Some(term) {
                    return Some(ordinal);
                }
            }
            at = (at + 1) & mask as usize;
        }
    }

    /// `[n_slots u64][n_entries u64][slots …]`
    pub fn serialize_into<W: Write>(&self, w: &mut W) -> io::Result<()> {
        write_u64(w, self.slots.len() as u64)?;
        write_u64(w, self.slots.iter().filter(|&&s| s != 0).count() as u64)?;
        for &s in self.slots.iter() {
            write_u64(w, s)?;
        }
        Ok(())
    }

    /// Deserialize (zero-copy over the payload view) + validate shape
    /// against the section it indexes. Errors here mean "rebuildable sidecar
    /// is malformed" — callers fall back to PFC binary search rather than
    /// failing the open. The validation walk touches every slot, which also
    /// pre-faults the (small) table under mmap.
    pub fn deserialize(payload: Bytes, section_len: usize) -> io::Result<HashSidecar> {
        let bad = |m: &str| io::Error::new(io::ErrorKind::InvalidData, format!("sidecar: {m}"));
        let mut c = Cursor::new(payload);
        let n_slots = c.read_u64()?;
        let n_entries = c.read_u64()?;
        if !n_slots.is_power_of_two() || n_slots < 8 {
            return Err(bad("slot count not a power of two ≥ 8"));
        }
        if n_entries != section_len as u64 || n_entries * 4 > n_slots * 3 {
            return Err(bad("entry count does not match section"));
        }
        let slots = c.take_words(n_slots as usize)?;
        if !c.is_empty() {
            return Err(bad("trailing bytes"));
        }
        let mut occupied = 0u64;
        for &s in slots.iter() {
            if s != 0 {
                occupied += 1;
                if s & ORDINAL_MASK == 0 || (s & ORDINAL_MASK) > n_entries {
                    return Err(bad("slot ordinal out of range"));
                }
            }
        }
        if occupied != n_entries {
            return Err(bad("occupied slots do not match entry count"));
        }
        Ok(HashSidecar { slots })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use graphy_succinct::PfcBuilder;

    fn section(terms: &[&[u8]]) -> Pfc {
        let mut b = PfcBuilder::new(4);
        for t in terms {
            b.push(t);
        }
        b.build()
    }

    #[test]
    fn build_locate_round_trip() {
        let terms: Vec<Vec<u8>> = (0..300)
            .map(|i| format!(">http://x/{i:03}").into_bytes())
            .collect();
        let mut sorted: Vec<&[u8]> = terms.iter().map(Vec::as_slice).collect();
        sorted.sort_unstable();
        let pfc = section(&sorted);
        let sc = HashSidecar::build(&pfc);
        for (i, t) in sorted.iter().enumerate() {
            assert_eq!(sc.locate(t, &pfc), Some(i), "term {i}");
            assert_eq!(sc.locate(t, &pfc), pfc.locate(t));
        }
        assert_eq!(sc.locate(b">http://x/miss", &pfc), None);

        let mut buf = Vec::new();
        sc.serialize_into(&mut buf).unwrap();
        let view = |b: &Vec<u8>| Bytes::from_vec_aligned(b.clone());
        let back = HashSidecar::deserialize(view(&buf), pfc.len()).unwrap();
        assert_eq!(back.locate(sorted[7], &pfc), Some(7));
        // Wrong section length rejected.
        assert!(HashSidecar::deserialize(view(&buf), pfc.len() + 1).is_err());
    }

    #[test]
    fn empty_section() {
        let pfc = section(&[]);
        let sc = HashSidecar::build(&pfc);
        assert_eq!(sc.locate(b"x", &pfc), None);
        let mut buf = Vec::new();
        sc.serialize_into(&mut buf).unwrap();
        HashSidecar::deserialize(Bytes::from_vec_aligned(buf), 0).unwrap();
    }
}
