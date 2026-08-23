//! graphy-pipe — the CLI's internal pipeline (docs/09): `/`-chained
//! commands exchange quads **in memory as concise-bytes events** instead of
//! re-serializing through shell pipes. Push-based: sources drive events
//! through a chain of operators into a terminal sink; [`Flow::Stop`]
//! propagates back to the source's read loop, so `head` on a huge file
//! reads O(chunk) rather than the file.
//!
//! The unary prefix of a pipeline replicates once per input (a "leg"); legs
//! join at `concat` (sequential, order-preserving) or `merge` (concurrent,
//! scoped threads, arrival order); the shared tail runs once. Store-free by
//! design — store/HDT endpoints wire up in `graphy-cli` (MC C6).

mod event;
mod ops;
mod plan;
mod sink;
mod source;

pub use event::{Event, EventBatch, Flow, OwnedQuad, Sink};
pub use ops::{chain, Head, Op, Skip, Tail, Tree, Unit};
pub use plan::{run, Input, Junction, OpSpec, PipelineSpec, SourceSpec, TerminalSpec};
pub use sink::{CountSink, DistinctBy, DistinctSink, NqSink, Out, PrettySink};
pub use source::{read_stream, scan_stream, Format};
