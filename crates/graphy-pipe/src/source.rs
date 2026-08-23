//! Pipeline sources (docs/09 §2–3): `read` drives the serial sans-io
//! parsers chunk by chunk with real upstream cancellation (a downstream
//! [`Flow::Stop`] halts the read loop, so `head` on a huge file reads
//! O(chunk)); `scan` fans N-Triples/N-Quads bytes across the data-parallel
//! parsers, delivering events in worker-arrival order.

use std::collections::HashSet;
use std::io::{self, Read};
use std::path::Path;

use graphy_turtle::{NQuadsParser, NTriplesParser, Options, ParseError, TriGParser, TurtleParser};

use crate::event::{Event, EventBatch, Flow, Sink};

/// Read-loop chunk size (matches the parallel parsers' feed granularity).
const CHUNK: usize = 256 * 1024;

/// Batch bounds for shipping scan output through the shared downstream lock.
const BATCH_ITEMS: usize = 1024;
const BATCH_BYTES: usize = 128 * 1024;

/// Text formats the pipeline reads/writes (HDT/HDTQ arrive with C6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Nt,
    Nq,
    Ttl,
    Trig,
}

impl Format {
    /// Parse a `-c/--content-type` value: short names and media types.
    pub fn from_name(name: &str) -> Option<Format> {
        Some(match name {
            "nt" | "ntriples" | "n-triples" | "application/n-triples" => Format::Nt,
            "nq" | "nquads" | "n-quads" | "application/n-quads" => Format::Nq,
            "ttl" | "turtle" | "text/turtle" => Format::Ttl,
            "trig" | "application/trig" => Format::Trig,
            _ => return None,
        })
    }

    /// Sniff by file extension (the `load` convention).
    pub fn from_path(path: &Path) -> Option<Format> {
        let ext = path.extension()?.to_str()?.to_ascii_lowercase();
        Format::from_name(match ext.as_str() {
            "nt" | "ntriples" => "nt",
            "nq" | "nquads" => "nq",
            "ttl" | "turtle" => "ttl",
            "trig" => "trig",
            _ => return None,
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            Format::Nt => "nt",
            Format::Nq => "nq",
            Format::Ttl => "ttl",
            Format::Trig => "trig",
        }
    }
}

fn parse_io(e: ParseError) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("line {}, column {}: {}", e.line, e.column, e.message),
    )
}

/// Drive a serial parser over `input`, pushing events into `sink`. Returns
/// the flow verdict — `Stop` means downstream cancelled and the rest of the
/// input was never read. Lenient-mode parse errors go to `on_warn`.
pub fn read_stream(
    input: &mut dyn Read,
    format: Format,
    options: Options,
    sink: &mut dyn Sink,
    on_warn: &mut dyn FnMut(&ParseError),
) -> io::Result<Flow> {
    // The four parser types share their API by macro, not trait — mirror
    // that here. The `prefixes` rule adds the Turtle/TriG prefix-event
    // diffing (a compile-time split: NT/NQ parsers have no prefix map).
    macro_rules! pump {
        (@prefixes $p:ident, $seen:ident, $sink:ident, $flow:ident, $label:lifetime) => {
            // New prefixes first: a declaration textually precedes any use,
            // so it lands in the same or an earlier feed than the first
            // quad that needs it.
            let mut fresh: Vec<(String, String)> = $p
                .prefixes()
                .filter(|(n, i)| !$seen.contains(&((*n).to_owned(), (*i).to_owned())))
                .map(|(n, i)| (n.to_owned(), i.to_owned()))
                .collect();
            fresh.sort();
            for (name, iri) in fresh {
                if $sink.event(Event::Prefix {
                    name: &name,
                    iri: &iri,
                })? == Flow::Stop
                {
                    $flow = Flow::Stop;
                    break $label;
                }
                $seen.insert((name, iri));
            }
        };
        ($parser:expr $(, $prefixes:tt)?) => {{
            let mut p = $parser.map_err(parse_io)?;
            #[allow(unused_mut, unused_variables)]
            let mut seen: HashSet<(String, String)> = HashSet::new();
            let mut buf = vec![0u8; CHUNK];
            let mut flow = Flow::Continue;
            'outer: loop {
                let n = input.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                p.feed(&buf[..n]).map_err(parse_io)?;
                $(pump!(@$prefixes p, seen, sink, flow, 'outer);)?
                for q in p.drain() {
                    if sink.event(Event::Quad(q))? == Flow::Stop {
                        flow = Flow::Stop;
                        break 'outer;
                    }
                }
            }
            if flow == Flow::Continue {
                p.finish().map_err(parse_io)?;
                for q in p.drain() {
                    if sink.event(Event::Quad(q))? == Flow::Stop {
                        flow = Flow::Stop;
                        break;
                    }
                }
            }
            for e in p.errors() {
                on_warn(e);
            }
            Ok(flow)
        }};
    }

    match format {
        Format::Nt => pump!(NTriplesParser::new(options)),
        Format::Nq => pump!(NQuadsParser::new(options)),
        Format::Ttl => pump!(TurtleParser::new(options), prefixes),
        Format::Trig => pump!(TriGParser::new(options), prefixes),
    }
}

/// Data-parallel N-Triples/N-Quads source over in-memory (mmap'd) bytes.
/// Events reach `sink` in worker-arrival order — nondeterministic interleave
/// across segment boundaries, document order within a segment. A downstream
/// `Stop` (or error) stops event delivery; the parse itself runs to
/// completion (the workers own the input split).
pub fn scan_stream(
    data: &[u8],
    format: Format,
    options: Options,
    threads: usize,
    sink: &mut dyn Sink,
) -> io::Result<Flow> {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Mutex;

    let threads = threads.max(1);
    let stop = AtomicBool::new(false);
    let failed: Mutex<Option<io::Error>> = Mutex::new(None);
    let shared: Mutex<&mut dyn Sink> = Mutex::new(sink);
    let batches: Vec<Mutex<EventBatch>> = (0..threads)
        .map(|_| Mutex::new(EventBatch::default()))
        .collect();

    let flush = |batch: &mut EventBatch| {
        if batch.is_empty() {
            return;
        }
        let mut s = shared.lock().expect("sink lock never poisoned");
        for ev in batch.events() {
            match s.event(ev) {
                Ok(Flow::Continue) => {}
                Ok(Flow::Stop) => {
                    stop.store(true, Ordering::Relaxed);
                    break;
                }
                Err(e) => {
                    let mut slot = failed.lock().expect("error slot never poisoned");
                    slot.get_or_insert(e);
                    stop.store(true, Ordering::Relaxed);
                    break;
                }
            }
        }
        batch.clear();
    };

    let on_quad = |seg: usize, q: graphy_turtle::QuadRef<'_>| {
        if stop.load(Ordering::Relaxed) {
            return;
        }
        let mut batch = batches[seg].lock().expect("batch lock never poisoned");
        batch.push(&Event::Quad(q));
        if batch.len() >= BATCH_ITEMS || batch.byte_len() >= BATCH_BYTES {
            flush(&mut batch);
        }
    };

    match format {
        Format::Nq => graphy_turtle::par::nquads(data, &options, threads, on_quad),
        Format::Nt => graphy_turtle::par::ntriples(data, &options, threads, on_quad),
        Format::Ttl | Format::Trig => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "scan parses N-Triples/N-Quads only (statements never span \
                 newlines, so the input splits exactly); use read for Turtle/TriG",
            ))
        }
    }
    .map_err(parse_io)?;

    if !stop.load(Ordering::Relaxed) {
        // Flush stragglers in segment order (best-effort document affinity).
        for b in &batches {
            flush(&mut b.lock().expect("batch lock never poisoned"));
        }
    }
    if let Some(e) = failed.into_inner().expect("error slot never poisoned") {
        return Err(e);
    }
    Ok(if stop.load(Ordering::Relaxed) {
        Flow::Stop
    } else {
        Flow::Continue
    })
}
