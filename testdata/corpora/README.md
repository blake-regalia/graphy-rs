# Benchmark corpora

Everything under this directory except this README is **gitignored** —
datasets are fetched/generated locally. This file records exactly how to
reconstruct each corpus and how to run its workload against graphy.

Run workloads with the amortized runner (store open + parse shared across
the whole workload, min-of-N wall times):

```sh
cargo run --release -p graphy-engine --example workload -- \
    <store-dir> <query-path>... [--lines] [--repeat N] [--threads N] [--timeout SECS]
```

## Available suites

### WatDiv (Waterloo SPARQL Diversity Test Suite)

- Source: https://dsg.uwaterloo.ca/watdiv/ (cite Aluç et al., ISWC 2014)
- Fetched: `watdiv.10M.tar.bz2` (10,916,457 triples, NT),
  `stress-workloads.tar.gz` (fully instantiated query workloads:
  `watdiv-stress-100/` = 17 query classes × 100 queries each, one query
  per line), `watdiv_v06.tar` (generator source,
  md5 `9eac247dfdec044d7fa0141ea3ad361f` — needs Boost +
  `/usr/share/dict/words`; NOT required, the pre-generated data +
  workloads cover the dashboard). 100M/1B datasets available at the same
  URL pattern (`watdiv.100M.tar.bz2`, `watdiv.1000M.tar.bz2`).
- Basic-testing templates (`watdiv/testsuite/*.txt`) contain `%v%`
  placeholders and need the generator to instantiate — the stress
  workloads don't.

```sh
graphy load stores/watdiv10m watdiv/watdiv.10M.nt --threads 0
cargo run --release -p graphy-engine --example workload -- \
  stores/watdiv10m watdiv/watdiv-stress-100 --lines --repeat 3 --timeout 60
```

### BSBM (Berlin SPARQL Benchmark), tools 0.2

- Source: https://sourceforge.net/projects/bsbmtools/ (GPL)
- `bsbm/bsbmtools-0.2/`: Java data generator + protocol test driver.
- Dataset: `./generate -fc -pc 2785 -s nt -fn dataset_1m` → 1,000,312
  triples (also writes `td_data/` used by the test driver to
  parameterize query templates).
- The BSBM workload is *protocol-level* (parameterized templates via the
  test driver against a SPARQL endpoint) — run it against `graphy serve`:

```sh
graphy load stores/bsbm1m bsbm/bsbmtools-0.2/dataset_1m.nt --threads 0
graphy serve stores/bsbm1m --bind 127.0.0.1:7879 &
cd bsbm/bsbmtools-0.2 && ./testdriver http://127.0.0.1:7879/sparql
```

### SP²Bench v1.01

- Original Freiburg site is dead; source + all 17 queries mirrored in
  https://github.com/earthquakesan/ISWC-2016-SQBenchmarks (`sp2bench/`).
- Build on macOS (the Makefile is 32-bit-Linux-era):
  `make CPP="c++ -Wall -O2 -Dfopen64=fopen"` in `sp2b_v1_01/src`.
  Keep `-O2` (the README warns `-O3` changes generated documents).
- Generator needs `familynames.txt`/`givennames.txt`/`titlewords.txt` in
  the working directory: `sp2b_gen -t 1000000 sp2b_1m.n3` (output is
  prefixed Turtle — rename to `.ttl` for graphy's extension sniffing).
- Queries: `sp2b_v1_01/queries/q{1..12c}.sparql` (q4/q5a are the
  intentionally heavy ones — run with `--timeout`).

```sh
graphy load stores/sp2b1m sp2bench/data/sp2b_1m.ttl
cargo run --release -p graphy-engine --example workload -- \
  stores/sp2b1m sp2bench/sp2b_v1_01/queries --repeat 3 --timeout 120
```

## Additional corpora

Real-world corpora and logs for the parser corpus, differential-fuzz
seeds, and scale runs — not yet staged:

| Corpus | Where | Use |
|---|---|---|
| Wikidata SPARQL query logs | https://iccl.inf.tu-dresden.de/web/Wikidata_SPARQL_Logs/en → `analytics.wikimedia.org/datasets/one-off/wikidata/sparql_query_logs/` (CC-0, gzipped TSV, ~575M queries / ~3.5M organic) | real-traffic parser corpus, fuzz seeds, planner stress |
| Curated Wikidata log subset | https://github.com/ad-freiburg/wikidata-query-logs | cleaned question/SPARQL pairs |
| LSQ 2.0 | https://lsq.aksw.org/ | feature-selectable real queries (DBpedia/bio/geo endpoints) as RDF |
| Wikidata truthy dumps | https://dumps.wikimedia.org/wikidatawiki/entities/ | 10⁹-scale load + serve target |
| DBpedia | https://databus.dbpedia.org/ | mid-scale real data; pairs with LSQ's DBpedia logs |
| LDBC SNB | https://ldbcouncil.org/benchmarks/snb/ | adapted graph-workload comparison |
| Sparqloscope | https://ad-publications.cs.uni-freiburg.de/ISWC_sparqloscope_BKTU_2025.pdf | per-dataset engine performance profile vs QLever/Oxigraph |
