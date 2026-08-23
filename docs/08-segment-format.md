# 08 · Base Segment Format v2

Normative description of the on-disk base segment, built by
`graphy-store::SegmentBuilder`, read by `Segment::open`, and checked by
`graphy verify`. **Current version: v2.** Incompatible changes bump the
per-component `version` and manifest `format_version`; readers reject other
versions.

All integers little-endian. One segment = one directory:

```
<segment>/
├── MANIFEST.json
├── dict/{shared,subjects,predicates,objects,graphs}.pfc    (Kind::Dict)
│   ├── {shared,subjects,predicates,objects,graphs}.hash    (Kind::HashSidecar)
│   └── triple_terms.bin                                    (Kind::TripleTerms)
├── idx/{spo,pos,osp,...}.bt                                (Kind::BitmapTriples)
│   └── foq.wm                              (Kind::Foq; compact profile only)
├── graphs/at.roar  graphs/tg.wm        (absent when triples-only)
└── stats/pred.stats  stats/charsets.bin
```

## 1. Component envelope

Every component file is a 64-byte header + payload:

| bytes | field |
|---|---|
| 0–7 | magic `GRFYCMP1` |
| 8–9 | kind u16 (Dict=1, TripleTerms=2, BitmapTriples=3, GraphsAt=4, GraphsTg=5, PredStats=6, CharSets=7, HashSidecar=8, Foq=9) |
| 10–11 | version u16 = 2 |
| 12–15 | reserved (zero) |
| 16–23 | payload length u64 |
| 24–31 | payload xxh3-64 digest u64 |
| 32–63 | reserved (zero) |

`MANIFEST.json` (written last, atomic-rename) repeats each component's payload
length and digest, plus counts, profile, orderings, and `has_graphs`.
Rebuildable sidecars (§4 `dict/*.hash`) are listed in a separate `sidecars`
map: they are **not checksum-critical** — a reader falls back to PFC binary
search when one is missing or fails validation; `graphy verify` still checks
the digests of any that the manifest lists.

**Alignment rule (zero-copy invariant, new in v2):** the header is 64 bytes,
so payload offset 0 is 64-byte aligned in the file. Every u64-typed field or
array within a payload must begin at an 8-byte-aligned payload offset. All
primitive encodings (§3) are u64-granular except trailing byte data (PFC
entry data), which no word field follows; the one byte-granular interior case
— Roaring bitmap blobs in `graphs/at.roar` — is zero-padded to the next
8-byte boundary (§4).

## 2. ID spaces

Index columns store **dense position-local ids**, not `TermId`s:

- **subject**: `shared` ordinals `0..n_sh`, then `subjects` ordinals
  (`n_sh..n_sh+n_subj`).
- **predicate**: `predicates` ordinals (a separate namespace: the same IRI
  can hold different ids per position).
- **graph column**: `0` = default graph; named graph = `graphs` ordinal + 1.
- **object**: dictionary objects `shared` then `objects` (always `< 2⁶⁰`),
  **or** a raw inline `TermId` (tags 1–6, doc 01 §4), **or** a triple-term
  reference `0x7 << 60 | ordinal`. The tag bits keep the three ranges
  disjoint; plain u64 order over the mix is the canonical object order.

Sections are byte-sorted over concise term encodings. `TermId`s with 1-based
section ordinals exist only at the public API boundary (`TermId::NULL`
aliases `(Shared, 0)`).

The on-disk format remains position-local in v2. At the public
`Snapshot` boundary, however, `term_id(column, position)` returns a
position-independent identity. Predicate and graph values are decoded once
when necessary and aliased to an identical subject/object/predicate term;
`column(term_id, position)` resolves the concise spelling back into the
requested local column. This is an API semantic correction, not a format
change, so the segment version is unchanged and existing v2 stores remain
readable.

## 3. Primitive encodings (`graphy-succinct::serial`)

- **BitVector** — `[len_bits u64][words ⌈len/64⌉×u64]`; trailing bits of the
  last word must be zero; rank/select directories rebuild at open.
- **PackedInts** — `[width u64 ≤64][len u64][data ⌈len·width/64⌉×u64]`,
  clean padding required.
- **PFC** — `[block_size u64][n u64][n_offsets u64][offsets u64×][data_len
  u64][data]`; block = `varint(head_len) head` then per entry
  `varint(lcp) varint(suffix_len) suffix` (LEB128).
- **WaveletMatrix** — `[width u64][len u64][zeros u64×width][BitVector ×
  width]`, most-significant level first.

## 4. Components

- **dict/\*.pfc** — one PFC per section.
- **dict/\*.hash** (v2, rebuildable sidecar; doc 02 RQ3) — open-addressing
  `term → ordinal` table over the matching PFC section:
  `[n_slots u64][n_entries u64][slots n_slots × u64]`. `n_slots` is a power
  of two ≥ 8 with load factor ≤ ¾ (`next_pow2(⌈4n/3⌉ + 1)`, min 8). Slot
  encoding: `0` = empty; else `fp << 56 | (ordinal + 1)` where
  `fp = xxh3_64(term) >> 56` (concise term bytes, the PFC key). Insertion:
  ordinals `0..n` in order, start slot `xxh3_64(term) & (n_slots − 1)`,
  linear probing — deterministic. Lookup probes until an empty slot; each
  fp match is confirmed by byte-comparing the PFC entry (fp collisions are
  possible, table hits are not authoritative). Written for every section,
  including empty ones.
- **dict/triple_terms.bin** — `[n u64]` then n × `[s u64][p u64][o u64]`
  records (column encodings per §2), ordered by (nesting depth, record) —
  nested references always point to lower ordinals.
- **idx/\*.bt** — `[flags u64 (bit0 = explicit X, bit1 = Pz present)]
  [n_x u64][x_values PackedInts?][Bx BitVector][Sy PackedInts][By BitVector]
  [Sz PackedInts][Pz PackedInts?]`.
  `Sy` lists Y ids per distinct (X,Y) grouped by X; `Bx` marks X-group starts
  in `Sy`; `Sz` lists Z per triple; `By` marks (X,Y)-group starts. S/P-rooted
  orderings use implicit dense X (group g ↔ x = g) and *fall back* to
  explicit X if a term occurs only inside triple terms (gap); O-rooted
  orderings are always explicit (object values are not dense).
  **Pz** (v2, doc 02 §3.2 permutation composition): parallel to `Sz`, the
  **SPO triple ordinal** of each triple, so graph bitmaps built over SPO
  ordinals serve every ordering without an SPO lookup. Present on every
  non-SPO ordering iff the segment has `graphs/`; never on SPO (identity)
  or in triples-only segments. `Pz` values are `< n_triples` and, within
  one ordering, a permutation of `0..n_triples`.
- **idx/foq.wm** (compact profile only; doc 02 §5, HDT-FoQ heritage) —
  wavelet accessors giving the compact profile P-rooted (PSO-order) and
  O-rooted (OSP-order) access over the lone SPO ordering:
  `[Wp WaveletMatrix][n_obj u64][Xo PackedInts][Bo BitVector][Po PackedInts]`.
  **Wp** spans SPO's `Sy` (the predicate of every distinct (subject,
  predicate) pair): `select(p, k)` walks a predicate's occurrences in
  subject order, each expanding to its `Sz` run — PSO emission from SPO
  alone. **Xo** lists the distinct object values ascending; **Po** lists all
  SPO triple ordinals sorted by (object, ordinal); **Bo** (over `Po`
  positions, `count_ones = n_obj`) marks each object's first position —
  OSP emission, and O(1) per-object triple counts. Both accessors yield SPO
  triple ordinals directly, so the graph layer composes without `Pz`
  (which compact segments never carry — they have no secondary orderings).
- **graphs/at.roar** — `[n_graphs u64]`, followed by n entries of
  `[byte_len u64][RoaringTreemap standard serialization][zero padding to an
  8-byte boundary]`, indexed by graph column (0 = default). The bitmaps hold
  SPO triple ordinals; `byte_len` excludes padding.
- **graphs/tg.wm** — `[starts BitVector][graphs WaveletMatrix]`: `starts`
  marks, in the (triple-ordinal, graph)-sorted quad list, each triple's first
  quad; the wavelet matrix stores the graph column.
- **stats/pred.stats** — `[n_preds u64]` then per predicate `[triple_count
  u64][distinct_subjects u64][distinct_objects u64]`.
- **stats/charsets.bin** — `[n u64][tail_subjects u64][n_tail_preds u64]`
  then n × `[count u64][n_preds u64][pred ids…]`, most frequent first, then
  n_tail_preds × `[pred id u64][count u64]` sorted by pred id (v2). The
  table holds the first 64Ki distinct signatures (SPO subject order);
  overflow subjects aggregate into `tail_subjects` plus **per-predicate tail
  marginals** — for each predicate, the number of tail subjects whose
  signature contains it — instead of v1's single scalar, preserving
  cardinality-estimation utility on signature-heavy datasets.

## 5. Invariants (checked by open/verify)

Checksums match header and manifest; PFC walks decode within bounds; BT
lengths/one-counts consistent, first group bits set, explicit X strictly
increasing, `Pz` length = `Sz` length with values `< n_triples` (open) and
exactly the SPO ordinal of each triple (deep verify); every ordering's
triples strictly increasing (deep verify); non-SPO orderings carry `Pz` iff
the segment has graphs; hash sidecar slots resolve to the correct PFC entries
and every section term is locatable through its sidecar (deep verify); tg
starts count = triples; graph bitmap cardinalities sum to the quad count;
triples-only segments have quads == triples; charset tail marginal counts
are ≤ `tail_subjects` each; section lengths match manifest counts; compact
segments carry `idx/foq.wm` with `Wp` = SPO's `Sy` symbol-for-symbol, `Xo`
strictly increasing, and `Po` a permutation of `0..n_triples` grouped by the
triples' object values (deep verify). Same input ⇒ byte-identical segment,
sidecars included (deterministic build; tested).

## 6. Current limitations

Memory-mapped open mode validates headers and structure but not payload digests
(checksumming would fault every page in; `graphy verify` covers integrity),
and rank/select directories still rebuild at open (one popcount pass over
bitvector words — persisting directories would need a format bump);
memory locking is not implemented; zero-copy views
require a little-endian host (rejected at compile time); charset table
membership is first-seen (SPO subject order) rather than globally
most-frequent.
