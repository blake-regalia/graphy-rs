//! Peak-memory profile of the layer1-shaped large-update flow (the
//! 2026-08-07 field problem: one ~300k-triple model commit peaking ~17 KB
//! per inserted triple inside layer1/graphy execution — 5 GB, past the
//! wasm32 4 GB ceiling).
//!
//! Replays the layer1-server branch-update route natively against an
//! ephemeral store, with a counting global allocator attributing live and
//! peak bytes to each phase:
//!
//! 1. synthesize a SysML-ish model update text (INSERT DATA of N triples
//!    into a staging graph, preceded by VALUES-scoped DELETE cascades)
//! 2. parse — graphy-sparql-syntax AST
//! 3. translate — graphy-algebra TranslatedUpdate
//! 4. execute — graphy-engine → Store::apply, WAL capture on
//! 5. diff update — gen_diff_update shape: anti-join unions feeding an
//!    18-quad insert template with row-constant metadata, BINDs via
//!    sha256/concat/iri
//! 6. snapshot — COPY staging → model graph
//!
//!   cargo run --release -p graphy-engine --example upmem -- [n_elements]
//!
//! Default n_elements = 24_672 (≈ 296k triples at 12 triples/element,
//! matching the field repro's scale).

use std::alloc::{GlobalAlloc, Layout, System};
use std::fmt::Write as _;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use graphy_algebra::translate_update;
use graphy_engine::execute_update;
use graphy_sparql_syntax::parse_update;
use graphy_store::{Pattern, QuadBatch, Snapshot, Store, TermPos};

// ------------------------------------------------------------ allocator

struct Counting;

static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

fn note_alloc(n: usize) {
    let live = LIVE.fetch_add(n, Ordering::Relaxed) + n;
    PEAK.fetch_max(live, Ordering::Relaxed);
}

fn note_dealloc(n: usize) {
    LIVE.fetch_sub(n, Ordering::Relaxed);
}

// SAFETY: pure pass-through to `System` with size bookkeeping on the
// side; every call forwards its arguments unchanged, so the contract is
// exactly `System`'s.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: same layout contract as the caller's.
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() {
            note_alloc(layout.size());
        }
        p
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: same (ptr, layout) contract as the caller's.
        unsafe { System.dealloc(ptr, layout) };
        note_dealloc(layout.size());
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: same layout contract as the caller's.
        let p = unsafe { System.alloc_zeroed(layout) };
        if !p.is_null() {
            note_alloc(layout.size());
        }
        p
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: same (ptr, layout, new_size) contract as the caller's.
        let p = unsafe { System.realloc(ptr, layout, new_size) };
        if !p.is_null() {
            note_dealloc(layout.size());
            note_alloc(new_size);
        }
        p
    }
}

#[global_allocator]
static ALLOC: Counting = Counting;

// ------------------------------------------------------------- reporting

fn mb(n: usize) -> f64 {
    n as f64 / (1024.0 * 1024.0)
}

/// Run one phase: reset the peak watermark to the current live level,
/// run, and report live-before → live-after plus the phase's transient
/// peak (both absolute and per inserted triple).
fn phase<T>(label: &str, triples: usize, f: impl FnOnce() -> T) -> T {
    let before = LIVE.load(Ordering::Relaxed);
    PEAK.store(before, Ordering::Relaxed);
    let t0 = Instant::now();
    let out = f();
    let secs = t0.elapsed().as_secs_f64();
    let after = LIVE.load(Ordering::Relaxed);
    let peak = PEAK.load(Ordering::Relaxed);
    println!(
        "{label:<12} live {:>8.1} → {:>8.1} MB   phase peak {:>8.1} MB \
         (+{:>7.1} over entry, {:>6.0} B/triple)   {secs:>6.2}s",
        mb(before),
        mb(after),
        mb(peak),
        mb(peak - before),
        (peak - before) as f64 / triples as f64,
    );
    out
}

// ------------------------------------------------------- data synthesis

/// splitmix64 — stable pseudo-ids without pulling in a rand dep.
fn mix(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9e3779b97f4a7c15);
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94d049bb133111eb);
    x ^ (x >> 31)
}

fn uuidish(i: u64) -> String {
    let a = mix(i);
    let b = mix(i ^ 0xdead_beef);
    format!(
        "{:08x}-{:04x}-{:04x}-{:04x}-{:012x}",
        (a >> 32) as u32,
        (a >> 16) as u16,
        a as u16,
        (b >> 48) as u16,
        b & 0xffff_ffff_ffff
    )
}

const NS: &str = "https://demo.org/sysml/projects/f2b1c9d0/elements/";
const META: &str = "https://www.omg.org/spec/SysML/20240201#";
const CLASSES: [&str; 12] = [
    "PartUsage",
    "PartDefinition",
    "AttributeUsage",
    "PortUsage",
    "ConnectionUsage",
    "ActionUsage",
    "StateUsage",
    "RequirementUsage",
    "ItemUsage",
    "InterfaceUsage",
    "OwningMembership",
    "FeatureMembership",
];

fn element_iri(i: u64) -> String {
    format!("{NS}{}", uuidish(i))
}

/// The i-th element's 12 triples, written as NT-ish lines into `out`.
fn write_element(out: &mut String, i: u64, n: u64) {
    let e = element_iri(i);
    let id = uuidish(i);
    let class = CLASSES[(i % CLASSES.len() as u64) as usize];
    let _ = writeln!(
        out,
        "<{e}> <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <{META}{class}> ."
    );
    let _ = writeln!(out, "<{e}> <{META}elementId> \"{id}\" .");
    let _ = writeln!(
        out,
        "<{e}> <{META}declaredName> \"Element {i} of the synthesized vehicle model\" ."
    );
    let _ = writeln!(
        out,
        "<{e}> <{META}qualifiedName> \"VehicleModel::Subsystem{}::Assembly{}::element{i}\" .",
        i / 1000,
        i / 100
    );
    let _ = writeln!(
        out,
        "<{e}> <{META}isAbstract> \"false\"^^<http://www.w3.org/2001/XMLSchema#boolean> ."
    );
    let _ = writeln!(
        out,
        "<{e}> <{META}textualRepresentation> \"{{\\\"@type\\\": \\\"{class}\\\", \\\"name\\\": \\\"element{i}\\\", \\\"idx\\\": {i}}}\" ."
    );
    for (k, pred) in [
        "owner",
        "owningMembership",
        "feature",
        "member",
        "relatedElement",
        "source",
    ]
    .into_iter()
    .enumerate()
    {
        let target = element_iri(mix(i.wrapping_mul(6).wrapping_add(k as u64)) % n);
        let _ = writeln!(out, "<{e}> <{META}{pred}> <{target}> .");
    }
}

const STAGING: &str = "urn:mms:staging";
const MODEL: &str = "urn:mms:model";

/// The user-update text: VALUES-scoped DELETE cascades (over n/12 ids ≈
/// the field repro's ~25k) followed by the full-model INSERT DATA.
fn gen_user_update(n: u64) -> String {
    let mut values = String::new();
    for i in 0..n {
        let _ = write!(values, "<{}> ", element_iri(i));
    }
    let mut s = String::new();
    // Cascade shape: delete the element's own triples, then dangling
    // memberships referencing it (two ops, both VALUES-scoped).
    let _ = writeln!(
        s,
        "DELETE {{ GRAPH <{STAGING}> {{ ?s ?p ?o . }} }} \
         WHERE {{ VALUES ?s {{ {values} }} GRAPH <{STAGING}> {{ ?s ?p ?o . }} }} ;"
    );
    let _ = writeln!(
        s,
        "DELETE {{ GRAPH <{STAGING}> {{ ?m <{META}member> ?s . }} }} \
         WHERE {{ VALUES ?s {{ {values} }} GRAPH <{STAGING}> {{ ?m <{META}member> ?s . }} }} ;"
    );
    let _ = writeln!(s, "INSERT DATA {{ GRAPH <{STAGING}> {{");
    for i in 0..n {
        write_element(&mut s, i, n);
    }
    s.push_str("} }\n");
    s
}

/// The layer1 `gen_diff_update` shape with the txn/where plumbing bound
/// inline: anti-join unions over src/dst feeding an insert template whose
/// metadata/txn/policy quads are row-constant.
fn gen_diff_update() -> String {
    format!(
        r#"insert {{
    graph <urn:mms:graph:Transactions> {{
        <urn:mms:txn> <urn:mms:p:srcGraph> ?srcGraph ;
            <urn:mms:p:dstGraph> ?dstGraph ;
            <urn:mms:p:insGraph> ?insGraph ;
            <urn:mms:p:delGraph> ?delGraph ;
            <urn:mms:p:createdPolicy> <urn:mms:policy:AutoDiffOwner> .
    }}
    graph <urn:mms:graph:Policies> {{
        <urn:mms:policy:AutoDiffOwner> a <urn:mms:Policy> ;
            <urn:mms:p:subject> <urn:mms:user> ;
            <urn:mms:p:scope> <urn:mms:repo> ;
            <urn:mms:p:role> <urn:mms:Role.AdminDiff> .
    }}
    graph ?insGraph {{ ?ins_s ?ins_p ?ins_o . }}
    graph ?delGraph {{ ?del_s ?del_p ?del_o . }}
    graph <urn:mms:graph:Metadata> {{
        ?diff a <urn:mms:Diff> ;
            <urn:mms:p:id> ?diffId ;
            <urn:mms:p:etag> ?diffId ;
            <urn:mms:p:createdBy> <urn:mms:user> ;
            <urn:mms:p:srcCommit> <urn:mms:commit:src> ;
            <urn:mms:p:dstCommit> <urn:mms:commit:dst> ;
            <urn:mms:p:insGraph> ?insGraph ;
            <urn:mms:p:delGraph> ?delGraph .
    }}
}}
where {{
    bind(sha256(concat(str(<urn:mms:commit:dst>), "\n", str(<urn:mms:commit:src>))) as ?diffId)
    bind(iri(concat(str(<urn:mms:commit:dst>), "/diffs/", ?diffId)) as ?diff)
    bind(iri(concat("urn:mms:graph:Diff.Ins.", ?diffId)) as ?insGraph)
    bind(iri(concat("urn:mms:graph:Diff.Del.", ?diffId)) as ?delGraph)
    {{
        graph <{MODEL}> {{ ?del_s ?del_p ?del_o . }}
        filter not exists {{ graph <{STAGING}> {{ ?del_s ?del_p ?del_o . }} }}
    }} union {{
        graph <{STAGING}> {{ ?ins_s ?ins_p ?ins_o . }}
        filter not exists {{ graph <{MODEL}> {{ ?ins_s ?ins_p ?ins_o . }} }}
    }} union {{}}
}}"#
    )
}

// -------------------------------------------------------------- helpers

fn graph_count(snap: &Snapshot, iri: &str) -> u64 {
    let concise = format!(">{iri}");
    let Some(col) = snap
        .resolve(concise.as_bytes(), TermPos::Graph)
        .and_then(|id| snap.column(id, TermPos::Graph))
    else {
        return 0;
    };
    let pat = Pattern {
        g: Some(col),
        ..Pattern::default()
    };
    let mut scan = snap.scan_best(&pat).expect("scan");
    let mut batch = QuadBatch::new();
    let mut n = 0;
    while scan.next_batch(&mut batch).expect("scan batch") {
        n += batch.len() as u64;
    }
    n
}

fn run_update(store: &Store, text: &str) {
    let parsed = parse_update(text).expect("parse");
    let translated = translate_update(&parsed).expect("translate");
    execute_update(store, &translated).expect("execute");
}

// ------------------------------------------------------------------ main

fn main() {
    let n: u64 = std::env::args()
        .nth(1)
        .and_then(|a| a.parse().ok())
        .unwrap_or(24_672);
    let triples = (n * 12) as usize;
    println!("elements: {n}   triples: {triples}\n");

    let text = phase("gen", triples, || gen_user_update(n));
    println!("             update text: {:.1} MB", mb(text.len()));

    let parsed = phase("parse", triples, || parse_update(&text).expect("parse"));
    let translated = phase("translate", triples, || {
        translate_update(&parsed).expect("translate")
    });

    // Hosts release the parse AST and request text before executing (the
    // translated ops carry everything needed) — mirror that here.
    drop(parsed);
    drop(text);

    let store = Store::ephemeral_persistent(None).expect("ephemeral store");
    phase("execute", triples, || {
        execute_update(&store, &translated).expect("execute")
    });
    let captured = store.drain_wal_capture();
    println!(
        "             wal capture drained: {:.1} MB",
        mb(captured.len())
    );
    drop(captured);
    drop(translated);

    {
        let snap = store.snapshot();
        assert_eq!(graph_count(&snap, STAGING), triples as u64);
        println!(
            "             staging graph: {} quads, delta events {}",
            triples,
            snap.delta_events()
        );
    }

    let diff_text = gen_diff_update();
    phase("diff", triples, || run_update(&store, &diff_text));
    drop(store.drain_wal_capture());

    // The copy target mirrors layer1's `copy graph <dst> to Model.{txn}`.
    let copy_text = format!("COPY GRAPH <{STAGING}> TO GRAPH <{MODEL}.next>");
    phase("copy", triples, || run_update(&store, &copy_text));
    drop(store.drain_wal_capture());

    let snap = store.snapshot();
    // staging + the diff's ins-graph image + the copy each hold `triples`
    // quads; anything less means an anti-join arm silently went empty.
    assert!(
        snap.delta_events() >= 3 * triples as u64,
        "diff/copy did not materialize"
    );
    println!(
        "\nfinal        live {:>8.1} MB   delta events {}   ({:.0} B/triple resident)",
        mb(LIVE.load(Ordering::Relaxed)),
        snap.delta_events(),
        LIVE.load(Ordering::Relaxed) as f64 / triples as f64,
    );
    let model_next = graph_count(&snap, &format!("{MODEL}.next"));
    assert_eq!(model_next, triples as u64, "copy landed");
}
