//! Quad storage (docs 02, 07 §7): the immutable **base segment** — PFC
//! dictionary sections, BitmapTriples orderings, HDTQ-AT graph bitmaps,
//! statistics — plus the streaming builder that bulk load and (later) the
//! background merger share, and a heap-mode reader for scans and `verify`.
//!
//! On-disk layout: one directory per segment (doc 02 §3, format spec in
//! `docs/08-segment-format.md`), individually checksummed component files,
//! `MANIFEST.json` written last.

mod bt;
mod builder;
mod delta;
mod dict;
mod dictmerge;
mod foq;
mod format;
mod manifest;
mod scan;
mod scheduler;
mod segment;
mod sidecar;
mod store;
mod wal;

pub use builder::{
    build_from_sorted_dict, BuilderConfig, IngestLane, Profile, QuadSource, SegmentBuilder,
};
pub use format::StoreError;
pub use manifest::Manifest;
pub use scan::{QuadBatch, SegmentScan, BATCH_CAPACITY};
pub use scheduler::{MergeScheduler, SchedulerConfig};
pub use segment::{OpenMode, Order, Pattern, QuadId, Segment, TermPos, EMPTY_SEGMENT};
pub use store::{
    resolve_segment_dir, Durability, MergeConfig, MergeStats, QuadScan, QuadTerms, Snapshot, Store,
    CURRENT_NAME,
};
