//! Query engine (doc 05): SPARQL algebra evaluation against store
//! snapshots. This crate currently ships the **reference evaluator**
//! (row-at-a-time, correctness-first — doc 05 §9's semantic oracle);
//! the vectorized morsel-parallel engine builds on it in later
//! increments and must match it query-for-query.

pub mod eval;
pub mod exec;
mod fresh;
pub mod update;
pub mod value;

pub use eval::{evaluate_ref, set_wall_clock_millis, EngineError, Output};
pub use exec::{evaluate, evaluate_with, ExecOptions};
pub use update::{
    decorrelate_fresh_labels, execute_update, execute_update_with_loader, LoadedTriple,
};
