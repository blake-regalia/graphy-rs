//! BitmapTriples (doc 02 §3.1): forward-adjacency representation of a
//! sorted, deduplicated triple set for one component ordering XYZ.
//!
//! - `Sy` — the Y ids of every distinct (X, Y) pair, grouped by X.
//! - `Bx` — bitvector over `Sy` marking each X-group's first entry.
//! - `Sz` — the Z ids of every triple, grouped by (X, Y).
//! - `By` — bitvector over `Sz` marking each (X, Y)-group's first entry.
//!
//! S- and P-rooted orderings have a **dense implicit X** (group g ↔ x = g:
//! every dictionary subject/predicate occurs in at least one triple). O-rooted
//! orderings carry an explicit sorted array of the distinct X values, because
//! object columns mix dense dictionary ids with inline `TermId`s (doc 01 §4)
//! and are therefore not dense.
//!
//! Every bound prefix of the ordering resolves to a contiguous `Sz` ordinal
//! range — the property the graph bitmaps and exact counts compose with.
//!
//! Non-SPO orderings in segments with named graphs additionally carry `Pz`
//! (doc 02 §3.2, format v2): the SPO triple ordinal of each triple, parallel
//! to `Sz`, so graph bitmaps built over SPO ordinals serve every ordering
//! without an SPO lookup.

use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};

use graphy_succinct::intvec::PackedIntsBuilder;
use graphy_succinct::serial::{write_u64, write_u64s};
use graphy_succinct::{BitVector, BitVectorBuilder, Cursor, PackedInts};

/// The six component orderings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Order {
    Spo,
    Sop,
    Pos,
    Pso,
    Osp,
    Ops,
}

impl Order {
    pub fn name(self) -> &'static str {
        match self {
            Order::Spo => "spo",
            Order::Sop => "sop",
            Order::Pos => "pos",
            Order::Pso => "pso",
            Order::Osp => "osp",
            Order::Ops => "ops",
        }
    }

    pub fn from_name(s: &str) -> Option<Order> {
        Some(match s {
            "spo" => Order::Spo,
            "sop" => Order::Sop,
            "pos" => Order::Pos,
            "pso" => Order::Pso,
            "osp" => Order::Osp,
            "ops" => Order::Ops,
            _ => return None,
        })
    }

    /// Permute a canonical (s, p, o) triple into this ordering's (x, y, z).
    pub fn to_xyz(self, s: u64, p: u64, o: u64) -> [u64; 3] {
        match self {
            Order::Spo => [s, p, o],
            Order::Sop => [s, o, p],
            Order::Pos => [p, o, s],
            Order::Pso => [p, s, o],
            Order::Osp => [o, s, p],
            Order::Ops => [o, p, s],
        }
    }

    /// Inverse of [`Order::to_xyz`].
    pub fn to_spo(self, x: u64, y: u64, z: u64) -> [u64; 3] {
        match self {
            Order::Spo => [x, y, z],
            Order::Sop => [x, z, y],
            Order::Pos => [z, x, y],
            Order::Pso => [y, x, z],
            Order::Osp => [y, z, x],
            Order::Ops => [z, y, x],
        }
    }

    /// O-rooted orderings carry an explicit X array (see module docs).
    pub fn explicit_x(self) -> bool {
        matches!(self, Order::Osp | Order::Ops)
    }
}

/// Streaming builder; feed strictly increasing (x, y, z) triples.
#[derive(Debug)]
pub(crate) struct BtCore<S> {
    explicit_x: Option<Vec<u64>>,
    bx: BitVectorBuilder,
    sy: S,
    by: BitVectorBuilder,
    sz: S,
    pz: Option<S>,
    last: Option<[u64; 3]>,
    groups: u64,
}

/// In-memory builder — the finished [`Bt`] serves queries (Phase B SPO,
/// the merger, unit tests).
pub(crate) type BtBuilder = BtCore<PackedIntsBuilder>;

/// Sequence sink shared by the in-memory and spilling builders.
pub(crate) trait SeqSink {
    fn push_val(&mut self, v: u64);
}

impl SeqSink for PackedIntsBuilder {
    fn push_val(&mut self, v: u64) {
        self.push(v);
    }
}

impl BtBuilder {
    pub fn new(explicit_x: bool, y_width: u32, z_width: u32) -> BtBuilder {
        BtCore {
            explicit_x: explicit_x.then(Vec::new),
            bx: BitVectorBuilder::new(),
            sy: PackedIntsBuilder::new(y_width),
            by: BitVectorBuilder::new(),
            sz: PackedIntsBuilder::new(z_width),
            pz: None,
            last: None,
            groups: 0,
        }
    }

    /// Also record an SPO-ordinal payload (`Pz`); every push must then pass
    /// `Some(ordinal)`. Production Pz builds go through the spilling
    /// builder (SPO itself never carries Pz), so this remains for the
    /// byte-identity tests.
    #[cfg(test)]
    pub fn with_spo_payload(mut self, ordinal_width: u32) -> BtBuilder {
        self.pz = Some(PackedIntsBuilder::new(ordinal_width));
        self
    }

    pub fn finish(self) -> Bt {
        let x_values = self.explicit_x.map(|xs| PackedInts::from_slice(&xs));
        Bt {
            n_x: self.groups,
            x_values,
            bx: self.bx.build(),
            sy: self.sy.build(),
            by: self.by.build(),
            sz: self.sz.build(),
            pz: self.pz.map(PackedIntsBuilder::build),
        }
    }
}

impl<S: SeqSink> BtCore<S> {
    pub fn push(&mut self, x: u64, y: u64, z: u64, spo_ordinal: Option<u64>) -> Result<(), String> {
        debug_assert_eq!(
            self.pz.is_some(),
            spo_ordinal.is_some(),
            "SPO payload pushes must match the builder configuration"
        );
        let t = [x, y, z];
        if let Some(last) = self.last {
            if t <= last {
                return Err(format!(
                    "triples not strictly increasing: {last:?} then {t:?}"
                ));
            }
        }
        let new_x = self.last.is_none_or(|l| l[0] != x);
        let new_y = new_x || self.last.is_none_or(|l| l[1] != y);
        if new_x {
            match &mut self.explicit_x {
                Some(xs) => xs.push(x),
                None => {
                    // Implicit dense X: group index must equal the value.
                    // Terms that occur only inside triple terms hold section
                    // ordinals without ever heading a quad, so a gap is
                    // possible — fall back to an explicit X array (the format
                    // flag covers both encodings).
                    if x != self.groups {
                        let mut xs: Vec<u64> = (0..self.groups).collect();
                        xs.push(x);
                        self.explicit_x = Some(xs);
                    }
                }
            }
            self.groups += 1;
        }
        if new_y {
            self.sy.push_val(y);
            self.bx.push(new_x);
        }
        self.sz.push_val(z);
        self.by.push(new_y);
        if let (Some(pz), Some(ord)) = (&mut self.pz, spo_ordinal) {
            pz.push_val(ord);
        }
        self.last = Some(t);
        Ok(())
    }
}

/// [`PackedIntsBuilder`] that spills filled words to a scratch file
/// (inc C: Phase C's serialize-only orderings hold ~8 MiB per sequence
/// instead of `n × width` bits). Same packing math, so the serialized
/// bytes are identical. I/O errors defer to `finish` (pushes stay
/// infallible, like the in-memory builder).
pub(crate) struct SpillingIntsBuilder {
    width: u32,
    len: usize,
    /// Words already written to the file (bit offsets in `data` are
    /// relative to `flushed × 64`).
    flushed: u64,
    data: Vec<u64>,
    w: BufWriter<File>,
    path: PathBuf,
    io_error: Option<io::Error>,
}

/// Resident-word ceiling per sequence (8 MiB).
const SPILL_WORDS: usize = 1 << 20;

impl SpillingIntsBuilder {
    fn new(width: u32, path: PathBuf) -> io::Result<SpillingIntsBuilder> {
        assert!(width <= 64);
        let w = BufWriter::new(File::create(&path)?);
        Ok(SpillingIntsBuilder {
            width,
            len: 0,
            flushed: 0,
            data: Vec::new(),
            w,
            path,
            io_error: None,
        })
    }

    fn push(&mut self, v: u64) {
        debug_assert!(
            self.width == 64 || v < 1 << self.width,
            "value {v} exceeds width {}",
            self.width
        );
        if self.width == 0 {
            self.len += 1;
            return;
        }
        let bit = self.len * self.width as usize - self.flushed as usize * 64;
        let word = bit / 64;
        let off = (bit % 64) as u32;
        if word == self.data.len() {
            self.data.push(0);
        }
        self.data[word] |= v << off;
        if off + self.width > 64 {
            self.data.push(v >> (64 - off));
        }
        self.len += 1;
        if self.data.len() >= SPILL_WORDS && self.io_error.is_none() {
            // The last word may still receive bits; flush the rest.
            let n = self.data.len() - 1;
            for word in &self.data[..n] {
                if let Err(e) = self.w.write_all(&word.to_le_bytes()) {
                    self.io_error = Some(e);
                    return;
                }
            }
            self.flushed += n as u64;
            let tail = self.data[n];
            self.data.clear();
            self.data.push(tail);
        }
    }

    fn finish(mut self) -> io::Result<SpilledInts> {
        if let Some(e) = self.io_error {
            return Err(e);
        }
        self.w.flush()?;
        drop(self.w);
        debug_assert_eq!(
            self.flushed as usize + self.data.len(),
            (self.len * self.width as usize).div_ceil(64),
            "word accounting"
        );
        Ok(SpilledInts {
            width: self.width,
            len: self.len,
            flushed: self.flushed,
            tail: self.data,
            path: self.path,
        })
    }
}

impl SeqSink for SpillingIntsBuilder {
    fn push_val(&mut self, v: u64) {
        self.push(v);
    }
}

/// A finished spilled sequence: serializes byte-identically to
/// [`PackedInts::serialize_into`]. Removes its scratch file on drop.
pub(crate) struct SpilledInts {
    width: u32,
    len: usize,
    flushed: u64,
    tail: Vec<u64>,
    path: PathBuf,
}

impl SpilledInts {
    fn serialize_into<W: Write>(&self, w: &mut W) -> io::Result<()> {
        write_u64(w, u64::from(self.width))?;
        write_u64(w, self.len as u64)?;
        let mut f = File::open(&self.path)?;
        let copied = io::copy(&mut f, w)?;
        if copied != self.flushed * 8 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("spilled sequence {}: short file", self.path.display()),
            ));
        }
        write_u64s(w, &self.tail)
    }
}

impl Drop for SpilledInts {
    fn drop(&mut self) {
        std::fs::remove_file(&self.path).ok();
    }
}

/// Serialize-only spilling builder for Phase C's secondary orderings
/// (built, written to its component, dropped — never queried).
pub(crate) type BtSpillBuilder = BtCore<SpillingIntsBuilder>;

impl BtSpillBuilder {
    /// `scratch/bt-{tag}-*.words` back the three sequences.
    pub fn new_spilling(
        scratch: &Path,
        tag: &str,
        explicit_x: bool,
        y_width: u32,
        z_width: u32,
    ) -> io::Result<BtSpillBuilder> {
        Ok(BtCore {
            explicit_x: explicit_x.then(Vec::new),
            bx: BitVectorBuilder::new(),
            sy: SpillingIntsBuilder::new(y_width, scratch.join(format!("bt-{tag}-sy.words")))?,
            by: BitVectorBuilder::new(),
            sz: SpillingIntsBuilder::new(z_width, scratch.join(format!("bt-{tag}-sz.words")))?,
            pz: None,
            last: None,
            groups: 0,
        })
    }

    /// See [`BtBuilder::with_spo_payload`].
    pub fn with_spo_payload(
        mut self,
        scratch: &Path,
        tag: &str,
        ordinal_width: u32,
    ) -> io::Result<BtSpillBuilder> {
        self.pz = Some(SpillingIntsBuilder::new(
            ordinal_width,
            scratch.join(format!("bt-{tag}-pz.words")),
        )?);
        Ok(self)
    }

    pub fn finish(self) -> io::Result<SpilledBt> {
        let x_values = self.explicit_x.map(|xs| PackedInts::from_slice(&xs));
        Ok(SpilledBt {
            n_x: self.groups,
            x_values,
            bx: self.bx.build(),
            sy: self.sy.finish()?,
            by: self.by.build(),
            sz: self.sz.finish()?,
            pz: self.pz.map(SpillingIntsBuilder::finish).transpose()?,
        })
    }
}

/// A finished spilled ordering: serializes byte-identically to
/// [`Bt::serialize_into`], then its scratch files vanish with it.
pub(crate) struct SpilledBt {
    n_x: u64,
    x_values: Option<PackedInts>,
    bx: BitVector,
    sy: SpilledInts,
    by: BitVector,
    sz: SpilledInts,
    pz: Option<SpilledInts>,
}

impl SpilledBt {
    pub fn serialize_into<W: Write>(&self, w: &mut W) -> io::Result<()> {
        let flags = u64::from(self.x_values.is_some()) | u64::from(self.pz.is_some()) << 1;
        write_u64(w, flags)?;
        write_u64(w, self.n_x)?;
        if let Some(xs) = &self.x_values {
            xs.serialize_into(w)?;
        }
        self.bx.serialize_into(w)?;
        self.sy.serialize_into(w)?;
        self.by.serialize_into(w)?;
        self.sz.serialize_into(w)?;
        if let Some(pz) = &self.pz {
            pz.serialize_into(w)?;
        }
        Ok(())
    }
}

/// An immutable BitmapTriples ordering.
#[derive(Debug)]
pub struct Bt {
    n_x: u64,
    /// Sorted distinct X values (O-rooted orderings only).
    x_values: Option<PackedInts>,
    bx: BitVector,
    sy: PackedInts,
    by: BitVector,
    sz: PackedInts,
    /// SPO triple ordinal per triple (non-SPO orderings with graphs).
    pz: Option<PackedInts>,
}

impl Bt {
    pub fn n_triples(&self) -> u64 {
        self.sz.len() as u64
    }

    /// Distinct X values (kept for the M3 read path; used by tests today).
    #[allow(dead_code)]
    pub fn n_x(&self) -> u64 {
        self.n_x
    }

    /// Group index of X value `x`, if present.
    pub fn x_group(&self, x: u64) -> Option<u64> {
        match &self.x_values {
            None => (x < self.n_x).then_some(x),
            Some(xs) => {
                // Binary search over the sorted distinct values.
                let (mut lo, mut hi) = (0usize, xs.len());
                while lo < hi {
                    let mid = (lo + hi) / 2;
                    if xs.get(mid) < x {
                        lo = mid + 1;
                    } else {
                        hi = mid;
                    }
                }
                (lo < xs.len() && xs.get(lo) == x).then_some(lo as u64)
            }
        }
    }

    /// X value of group `g`.
    pub fn x_value(&self, g: u64) -> u64 {
        match &self.x_values {
            None => g,
            Some(xs) => xs.get(g as usize),
        }
    }

    /// `Sy` index range of group `g`.
    pub fn y_range(&self, g: u64) -> (u64, u64) {
        let start = self.bx.select1(g).expect("group in range") as u64;
        let end = self
            .bx
            .select1(g + 1)
            .map_or(self.sy.len() as u64, |e| e as u64);
        (start, end)
    }

    /// Y id at `Sy` index `yi`.
    pub fn y_at(&self, yi: u64) -> u64 {
        self.sy.get(yi as usize)
    }

    /// Number of distinct (X, Y) pairs (`Sy` length).
    pub fn n_y(&self) -> u64 {
        self.sy.len() as u64
    }

    /// X group index containing `Sy` position `yi`.
    pub fn group_of_y(&self, yi: u64) -> u64 {
        self.bx.rank1(yi as usize + 1) - 1
    }

    /// Find `y` within group `g` by binary search; returns its `Sy` index.
    pub fn find_y(&self, g: u64, y: u64) -> Option<u64> {
        let (start, end) = self.y_range(g);
        let (mut lo, mut hi) = (start, end);
        while lo < hi {
            let mid = (lo + hi) / 2;
            if self.sy.get(mid as usize) < y {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        (lo < end && self.sy.get(lo as usize) == y).then_some(lo)
    }

    /// `Sz` ordinal range of `Sy` position `yi`.
    pub fn z_range(&self, yi: u64) -> (u64, u64) {
        let start = self.by.select1(yi).expect("y position in range") as u64;
        let end = self
            .by
            .select1(yi + 1)
            .map_or(self.sz.len() as u64, |e| e as u64);
        (start, end)
    }

    /// `Sz` ordinal range of the whole X group `g` (contiguous by layout).
    pub fn z_range_of_group(&self, g: u64) -> (u64, u64) {
        let (ya, yb) = self.y_range(g);
        let start = self.by.select1(ya).expect("y position in range") as u64;
        let end = self
            .by
            .select1(yb)
            .map_or(self.sz.len() as u64, |e| e as u64);
        (start, end)
    }

    /// Z id at ordinal `i` (M3 read path).
    #[allow(dead_code)]
    pub fn z_at(&self, i: u64) -> u64 {
        self.sz.get(i as usize)
    }

    /// SPO triple ordinal of ordinal `i`, when the `Pz` payload is present.
    pub fn spo_at(&self, i: u64) -> Option<u64> {
        self.pz.as_ref().map(|pz| pz.get(i as usize))
    }

    /// Whether this ordering carries the `Pz` SPO-ordinal payload.
    pub fn has_spo_payload(&self) -> bool {
        self.pz.is_some()
    }

    /// Find `z` within the z-range of `Sy` position `yi`; returns its ordinal.
    pub fn find_z(&self, yi: u64, z: u64) -> Option<u64> {
        let (start, end) = self.z_range(yi);
        let (mut lo, mut hi) = (start, end);
        while lo < hi {
            let mid = (lo + hi) / 2;
            if self.sz.get(mid as usize) < z {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        (lo < end && self.sz.get(lo as usize) == z).then_some(lo)
    }

    /// The (x, y, z) triple at `Sz` ordinal `i`.
    pub fn triple_at(&self, i: u64) -> [u64; 3] {
        let z = self.sz.get(i as usize);
        let yi = self.by.rank1(i as usize + 1) - 1;
        let y = self.sy.get(yi as usize);
        let g = self.bx.rank1(yi as usize + 1) - 1;
        [self.x_value(g), y, z]
    }

    /// Sequential triples over `Sz` ordinals `lo..hi`. Positions once with
    /// rank, then advances run-to-run with forward bit scans — amortized
    /// O(1) per triple where `triple_at` pays two ranks each.
    pub(crate) fn seq_range(&self, lo: u64, hi: u64) -> BtSeqIter<'_> {
        if lo >= hi {
            return BtSeqIter {
                bt: self,
                i: 0,
                end: 0,
                yi: 0,
                group_end_yi: 0,
                g: 0,
                x: 0,
                y: 0,
                run_end: 0,
            };
        }
        let yi = self.by.rank1(lo as usize + 1) - 1;
        let g = self.bx.rank1(yi as usize + 1) - 1;
        BtSeqIter {
            bt: self,
            i: lo,
            end: hi,
            yi,
            group_end_yi: self.next_group_start(yi + 1),
            g,
            x: self.x_value(g),
            y: self.sy.get(yi as usize),
            run_end: self.next_run_start(lo + 1),
        }
    }

    /// First `Sy` index ≥ `from` that starts an X group (`Bx` scan).
    fn next_group_start(&self, from: u64) -> u64 {
        self.bx
            .next_one(from as usize)
            .map_or(self.sy.len() as u64, |p| p as u64)
    }

    /// First `Sz` ordinal ≥ `from` that starts a (X, Y) run (`By` scan).
    fn next_run_start(&self, from: u64) -> u64 {
        self.by
            .next_one(from as usize)
            .map_or(self.sz.len() as u64, |p| p as u64)
    }

    /// `[flags u64][n_x u64][x_values?][bx][sy][by][sz][pz?]`
    pub fn serialize_into<W: Write>(&self, w: &mut W) -> io::Result<()> {
        let flags = u64::from(self.x_values.is_some()) | u64::from(self.pz.is_some()) << 1;
        write_u64(w, flags)?;
        write_u64(w, self.n_x)?;
        if let Some(xs) = &self.x_values {
            xs.serialize_into(w)?;
        }
        self.bx.serialize_into(w)?;
        self.sy.serialize_into(w)?;
        self.by.serialize_into(w)?;
        self.sz.serialize_into(w)?;
        if let Some(pz) = &self.pz {
            pz.serialize_into(w)?;
        }
        Ok(())
    }

    /// Deserialize from a payload view: word arrays stay zero-copy (mmap
    /// mode) or point into the heap-loaded payload (heap mode).
    pub fn deserialize(c: &mut Cursor) -> io::Result<Bt> {
        let bad = |m: &str| io::Error::new(io::ErrorKind::InvalidData, format!("bt: {m}"));
        let flags = c.read_u64()?;
        let n_x = c.read_u64()?;
        let x_values = if flags & 1 == 1 {
            Some(PackedInts::deserialize_view(c)?)
        } else {
            None
        };
        let bx = BitVector::deserialize_view(c)?;
        let sy = PackedInts::deserialize_view(c)?;
        let by = BitVector::deserialize_view(c)?;
        let sz = PackedInts::deserialize_view(c)?;
        let pz = if flags & 2 == 2 {
            Some(PackedInts::deserialize_view(c)?)
        } else {
            None
        };
        // Structural invariants keeping navigation panic-free.
        if bx.len() != sy.len() || by.len() != sz.len() {
            return Err(bad("bitmap/sequence length mismatch"));
        }
        if bx.count_ones() != n_x || by.count_ones() != sy.len() as u64 {
            return Err(bad("group-start counts do not match"));
        }
        if !sy.is_empty() && (!bx.get(0) || !by.get(0)) {
            return Err(bad("first group-start bit unset"));
        }
        if let Some(xs) = &x_values {
            if xs.len() as u64 != n_x {
                return Err(bad("explicit X count mismatch"));
            }
            for i in 1..xs.len() {
                if xs.get(i - 1) >= xs.get(i) {
                    return Err(bad("explicit X values not strictly increasing"));
                }
            }
        }
        if let Some(pz) = &pz {
            // SPO ordinals index graph structures sized by the triple count,
            // so an out-of-range value would panic downstream, not just miss.
            if pz.len() != sz.len() {
                return Err(bad("Pz length differs from Sz"));
            }
            if pz.iter().any(|v| v >= sz.len() as u64) {
                return Err(bad("Pz ordinal out of range"));
            }
        }
        Ok(Bt {
            n_x,
            x_values,
            bx,
            sy,
            by,
            sz,
            pz,
        })
    }
}

/// See [`Bt::seq_range`].
#[derive(Debug)]
pub(crate) struct BtSeqIter<'a> {
    bt: &'a Bt,
    i: u64,
    end: u64,
    yi: u64,
    /// `Sy` index where the next X group starts.
    group_end_yi: u64,
    g: u64,
    x: u64,
    y: u64,
    /// `Sz` ordinal where the next (X, Y) run starts.
    run_end: u64,
}

impl BtSeqIter<'_> {
    /// Force the iterator to its exhausted state.
    pub(crate) fn stop(&mut self) {
        self.i = self.end;
    }
}

impl Iterator for BtSeqIter<'_> {
    /// (`Sz` ordinal, (x, y, z)).
    type Item = (u64, [u64; 3]);

    fn next(&mut self) -> Option<(u64, [u64; 3])> {
        if self.i >= self.end {
            return None;
        }
        if self.i >= self.run_end {
            // Runs are contiguous and non-empty, so `i` sits exactly on the
            // next run's first ordinal.
            self.yi += 1;
            if self.yi >= self.group_end_yi {
                self.g += 1;
                self.x = self.bt.x_value(self.g);
                self.group_end_yi = self.bt.next_group_start(self.yi + 1);
            }
            self.y = self.bt.sy.get(self.yi as usize);
            self.run_end = self.bt.next_run_start(self.i + 1);
        }
        let i = self.i;
        self.i += 1;
        Some((i, [self.x, self.y, self.bt.sz.get(i as usize)]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Cross the word-flush threshold: a spilled sequence's bytes must
    /// match the in-memory packing exactly, including the straddled word
    /// kept resident at each flush.
    #[test]
    fn spilling_ints_flush_boundary() {
        let dir = std::env::temp_dir().join(format!("graphy-seq-spill-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let width = 17u32; // straddles u64 boundaries
        let n = (SPILL_WORDS * 64 / width as usize) * 2 + 3; // ≥ 2 flushes
        let mut mem = PackedIntsBuilder::new(width);
        let mut spill = SpillingIntsBuilder::new(width, dir.join("seq.words")).unwrap();
        let mut state = 0xB5AD_4ECE_DA1C_E2A9u64;
        for _ in 0..n {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let v = state & ((1 << width) - 1);
            mem.push(v);
            spill.push(v);
        }
        let mut a = Vec::new();
        mem.build().serialize_into(&mut a).unwrap();
        let mut b = Vec::new();
        spill.finish().unwrap().serialize_into(&mut b).unwrap();
        assert_eq!(a, b);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The spilling serialize-only builder must emit the exact bytes the
    /// in-memory path does — across word-flush boundaries, straddling
    /// widths, explicit X, and the Pz payload.
    #[test]
    fn spilled_bt_serializes_byte_identically() {
        let dir = std::env::temp_dir().join(format!("graphy-bt-spill-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // Enough triples to matter, widths that straddle word boundaries.
        let mut triples: Vec<[u64; 3]> = Vec::new();
        let mut state = 0x9E37_79B9_7F4A_7C15u64;
        for x in 0..97u64 {
            for y in 0..23u64 {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                for z in 0..(state % 7 + 1) {
                    triples.push([x * 3, y * 5 + 1, z * 1001 + (state & 0xFF)]);
                }
            }
        }
        triples.sort_unstable();
        triples.dedup();
        let wy = graphy_succinct::intvec::bits_for(triples.iter().map(|t| t[1]).max().unwrap());
        let wz = graphy_succinct::intvec::bits_for(triples.iter().map(|t| t[2]).max().unwrap());
        let wo = graphy_succinct::intvec::bits_for(triples.len() as u64);
        for explicit in [false, true] {
            for with_pz in [false, true] {
                let mut mem = BtBuilder::new(explicit, wy, wz);
                let mut spill =
                    BtSpillBuilder::new_spilling(&dir, "test", explicit, wy, wz).unwrap();
                if with_pz {
                    mem = mem.with_spo_payload(wo);
                    spill = spill.with_spo_payload(&dir, "test", wo).unwrap();
                }
                for (i, &[x, y, z]) in triples.iter().enumerate() {
                    let ord = with_pz.then_some(i as u64);
                    mem.push(x, y, z, ord).unwrap();
                    spill.push(x, y, z, ord).unwrap();
                }
                let mut a = Vec::new();
                mem.finish().serialize_into(&mut a).unwrap();
                let mut b = Vec::new();
                spill.finish().unwrap().serialize_into(&mut b).unwrap();
                assert_eq!(a, b, "explicit={explicit} pz={with_pz}");
            }
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    fn build(triples: &[[u64; 3]], explicit: bool) -> Bt {
        let y_max = triples.iter().map(|t| t[1]).max().unwrap_or(0);
        let z_max = triples.iter().map(|t| t[2]).max().unwrap_or(0);
        let mut b = BtBuilder::new(
            explicit,
            graphy_succinct::intvec::bits_for(y_max),
            graphy_succinct::intvec::bits_for(z_max),
        );
        for &[x, y, z] in triples {
            b.push(x, y, z, None).unwrap();
        }
        b.finish()
    }

    #[test]
    fn navigation_round_trip() {
        // Dense X: 0..3 all present.
        let triples = [
            [0, 0, 5],
            [0, 0, 9],
            [0, 2, 1],
            [1, 1, 1],
            [2, 0, 0],
            [2, 0, 2],
            [2, 3, 7],
        ];
        let bt = build(&triples, false);
        assert_eq!(bt.n_triples(), 7);
        assert_eq!(bt.n_x(), 3);
        for (i, &t) in triples.iter().enumerate() {
            assert_eq!(bt.triple_at(i as u64), t, "ordinal {i}");
        }
        // Bound-x range: x=2 covers ordinals 4..7.
        assert_eq!(bt.z_range_of_group(bt.x_group(2).unwrap()), (4, 7));
        // Bound (x, y): (0, 0) covers 0..2.
        let yi = bt.find_y(0, 0).unwrap();
        assert_eq!(bt.z_range(yi), (0, 2));
        // Fully bound lookups.
        assert_eq!(bt.find_z(yi, 9), Some(1));
        assert_eq!(bt.find_z(yi, 4), None);
        assert_eq!(bt.find_y(1, 0), None);
        // Serialization round trip.
        let mut buf = Vec::new();
        bt.serialize_into(&mut buf).unwrap();
        let back = Bt::deserialize(&mut Cursor::new(graphy_succinct::Bytes::from_vec_aligned(
            buf.clone(),
        )))
        .unwrap();
        for (i, &t) in triples.iter().enumerate() {
            assert_eq!(back.triple_at(i as u64), t);
        }
    }

    #[test]
    fn explicit_x_for_sparse_values() {
        // Object-like X values incl. a fake inline id (huge).
        let big = 0x1 << 60 | 42;
        let triples = [[3, 0, 0], [3, 5, 1], [900, 2, 2], [big, 0, 1]];
        let bt = build(&triples, true);
        assert_eq!(bt.n_x(), 3);
        assert_eq!(bt.x_group(3), Some(0));
        assert_eq!(bt.x_group(900), Some(1));
        assert_eq!(bt.x_group(big), Some(2));
        assert_eq!(bt.x_group(4), None);
        for (i, &t) in triples.iter().enumerate() {
            assert_eq!(bt.triple_at(i as u64), t);
        }
        let mut buf = Vec::new();
        bt.serialize_into(&mut buf).unwrap();
        let back = Bt::deserialize(&mut Cursor::new(graphy_succinct::Bytes::from_vec_aligned(
            buf.clone(),
        )))
        .unwrap();
        assert_eq!(back.x_group(big), Some(2));
    }

    #[test]
    fn dense_gap_falls_back_to_explicit() {
        let mut b = BtBuilder::new(false, 8, 8);
        b.push(0, 0, 0, None).unwrap();
        b.push(2, 0, 0, None).unwrap(); // gap at x=1 → explicit fallback
        let bt = b.finish();
        assert_eq!(bt.n_x(), 2);
        assert_eq!(bt.x_group(0), Some(0));
        assert_eq!(bt.x_group(1), None);
        assert_eq!(bt.x_group(2), Some(1));
        assert_eq!(bt.triple_at(1), [2, 0, 0]);
        // Duplicates are still rejected.
        let mut b = BtBuilder::new(false, 8, 8);
        b.push(0, 0, 1, None).unwrap();
        assert!(b.push(0, 0, 1, None).is_err());
    }

    #[test]
    fn seq_range_agrees_with_triple_at() {
        // Mixed run lengths, group gaps, explicit and implicit X.
        let triples = [
            [0u64, 0, 5],
            [0, 0, 9],
            [0, 2, 1],
            [1, 1, 1],
            [2, 0, 0],
            [2, 0, 2],
            [2, 3, 7],
            [2, 3, 8],
        ];
        for explicit in [false, true] {
            let bt = build(&triples, explicit);
            let n = bt.n_triples();
            for lo in 0..=n {
                for hi in lo..=n {
                    let got: Vec<(u64, [u64; 3])> = bt.seq_range(lo, hi).collect();
                    let expected: Vec<(u64, [u64; 3])> =
                        (lo..hi).map(|i| (i, bt.triple_at(i))).collect();
                    assert_eq!(got, expected, "explicit={explicit} range {lo}..{hi}");
                }
            }
        }
    }

    #[test]
    fn spo_payload_round_trip() {
        // A POS-like ordering of 3 triples whose SPO ordinals are a
        // permutation of 0..3.
        let mut b = BtBuilder::new(false, 8, 8).with_spo_payload(8);
        b.push(0, 1, 2, Some(2)).unwrap();
        b.push(0, 3, 1, Some(0)).unwrap();
        b.push(1, 0, 0, Some(1)).unwrap();
        let bt = b.finish();
        assert!(bt.has_spo_payload());
        assert_eq!(bt.spo_at(0), Some(2));
        assert_eq!(bt.spo_at(2), Some(1));

        let mut buf = Vec::new();
        bt.serialize_into(&mut buf).unwrap();
        let back = Bt::deserialize(&mut Cursor::new(graphy_succinct::Bytes::from_vec_aligned(
            buf.clone(),
        )))
        .unwrap();
        assert_eq!(back.spo_at(1), Some(0));

        // Out-of-range Pz ordinal rejected at deserialize.
        let mut b = BtBuilder::new(false, 8, 8).with_spo_payload(8);
        b.push(0, 0, 0, Some(9)).unwrap();
        let mut buf = Vec::new();
        b.finish().serialize_into(&mut buf).unwrap();
        assert!(
            Bt::deserialize(&mut Cursor::new(graphy_succinct::Bytes::from_vec_aligned(
                buf.clone()
            )))
            .is_err()
        );
    }
}
