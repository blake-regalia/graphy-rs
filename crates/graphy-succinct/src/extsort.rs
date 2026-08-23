//! Disk-backed external sorting of fixed-size records (doc 02): buffer up to
//! a memory budget, spill sorted runs to a scratch directory, then k-way
//! merge with a binary heap over buffered readers. More than
//! [`MAX_OPEN_RUNS`] runs are first merged group-wise into fewer, larger runs
//! (multi-pass) so the final merge never exceeds the open-file cap.

use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::fs::{self, File};
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

/// Maximum runs merged in one pass (open-file budget).
pub const MAX_OPEN_RUNS: usize = 64;

/// A fixed-size, totally ordered record that can round-trip through bytes.
pub trait Record: Copy + Ord {
    const SIZE: usize;
    fn write_bytes(&self, out: &mut Vec<u8>);
    fn read_bytes(buf: &[u8]) -> Self;
}

impl Record for u64 {
    const SIZE: usize = 8;

    fn write_bytes(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.to_le_bytes());
    }

    fn read_bytes(buf: &[u8]) -> u64 {
        u64::from_le_bytes(buf[..8].try_into().expect("record buffer sized"))
    }
}

macro_rules! record_for_u64_array {
    ($n:literal) => {
        impl Record for [u64; $n] {
            const SIZE: usize = 8 * $n;

            fn write_bytes(&self, out: &mut Vec<u8>) {
                for v in self {
                    out.extend_from_slice(&v.to_le_bytes());
                }
            }

            fn read_bytes(buf: &[u8]) -> [u64; $n] {
                let mut a = [0u64; $n];
                for (i, v) in a.iter_mut().enumerate() {
                    *v = u64::from_le_bytes(
                        buf[i * 8..(i + 1) * 8]
                            .try_into()
                            .expect("record buffer sized"),
                    );
                }
                a
            }
        }
    };
}

record_for_u64_array!(2);
record_for_u64_array!(3);
record_for_u64_array!(4);

/// A spilled sorted run; the backing file is removed on drop.
#[derive(Debug)]
struct Run {
    path: PathBuf,
    records: u64,
}

impl Drop for Run {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

static RUN_SEQ: AtomicU64 = AtomicU64::new(0);

/// Memory-budgeted external sorter. Push records in any order; `finish`
/// yields them ascending (duplicates preserved).
#[derive(Debug)]
pub struct ExtSorter<R: Record> {
    scratch_dir: PathBuf,
    max_buffered: usize,
    buffer: Vec<R>,
    runs: Vec<Run>,
    /// Merge fan-in; [`MAX_OPEN_RUNS`] except in tests.
    max_open_runs: usize,
}

impl<R: Record> ExtSorter<R> {
    /// `memory_budget_bytes` bounds the in-memory buffer (min one record).
    /// The scratch directory is created if missing; spill files are removed
    /// as they are consumed or dropped.
    pub fn new(scratch_dir: impl Into<PathBuf>, memory_budget_bytes: usize) -> io::Result<Self> {
        let scratch_dir = scratch_dir.into();
        fs::create_dir_all(&scratch_dir)?;
        Ok(ExtSorter {
            scratch_dir,
            max_buffered: (memory_budget_bytes / R::SIZE).max(1),
            buffer: Vec::new(),
            runs: Vec::new(),
            max_open_runs: MAX_OPEN_RUNS,
        })
    }

    pub fn push(&mut self, record: R) -> io::Result<()> {
        self.buffer.push(record);
        if self.buffer.len() >= self.max_buffered {
            self.spill()?;
        }
        Ok(())
    }

    fn spill(&mut self) -> io::Result<()> {
        if self.buffer.is_empty() {
            return Ok(());
        }
        self.buffer.sort_unstable();
        let path = self.fresh_run_path();
        let mut out = BufWriter::new(File::create(&path)?);
        let mut bytes = Vec::with_capacity(R::SIZE);
        for r in &self.buffer {
            bytes.clear();
            r.write_bytes(&mut bytes);
            out.write_all(&bytes)?;
        }
        out.flush()?;
        self.runs.push(Run {
            path,
            records: self.buffer.len() as u64,
        });
        self.buffer.clear();
        Ok(())
    }

    fn fresh_run_path(&self) -> PathBuf {
        let seq = RUN_SEQ.fetch_add(1, Ordering::Relaxed);
        self.scratch_dir
            .join(format!("extsort-{}-{seq}.run", std::process::id()))
    }

    /// Sort everything pushed so far and return the ascending stream.
    pub fn finish(mut self) -> io::Result<ExtSorted<R>> {
        if self.runs.is_empty() {
            self.buffer.sort_unstable();
            return Ok(ExtSorted::Memory(
                std::mem::take(&mut self.buffer).into_iter(),
            ));
        }
        self.spill()?;
        // Multi-pass: collapse groups until one merge pass fits the cap.
        while self.runs.len() > self.max_open_runs {
            let group: Vec<Run> = self.runs.drain(..self.max_open_runs).collect();
            let merged = self.merge_to_run(group)?;
            self.runs.push(merged);
        }
        let runs = std::mem::take(&mut self.runs);
        Ok(ExtSorted::Merge(Merge::new(runs)?))
    }

    fn merge_to_run(&self, group: Vec<Run>) -> io::Result<Run> {
        let records = group.iter().map(|r| r.records).sum();
        let mut merge: Merge<R> = Merge::new(group)?;
        let path = self.fresh_run_path();
        let mut out = BufWriter::new(File::create(&path)?);
        let mut bytes = Vec::with_capacity(R::SIZE);
        while let Some(r) = merge.next_record()? {
            bytes.clear();
            r.write_bytes(&mut bytes);
            out.write_all(&bytes)?;
        }
        out.flush()?;
        Ok(Run { path, records })
    }
}

struct RunReader<R: Record> {
    reader: BufReader<File>,
    remaining: u64,
    bytes: Vec<u8>,
    /// Keeps the backing file alive (and deleted afterwards).
    _run: Run,
    _marker: std::marker::PhantomData<R>,
}

impl<R: Record> RunReader<R> {
    fn open(run: Run) -> io::Result<Self> {
        Ok(RunReader {
            reader: BufReader::new(File::open(&run.path)?),
            remaining: run.records,
            bytes: vec![0; R::SIZE],
            _run: run,
            _marker: std::marker::PhantomData,
        })
    }

    fn next_record(&mut self) -> io::Result<Option<R>> {
        if self.remaining == 0 {
            return Ok(None);
        }
        self.reader.read_exact(&mut self.bytes)?;
        self.remaining -= 1;
        Ok(Some(R::read_bytes(&self.bytes)))
    }
}

/// K-way heap merge over sorted runs.
pub struct Merge<R: Record> {
    readers: Vec<RunReader<R>>,
    heap: BinaryHeap<Reverse<(R, usize)>>,
}

impl<R: Record> Merge<R> {
    fn new(runs: Vec<Run>) -> io::Result<Self> {
        let mut readers = runs
            .into_iter()
            .map(RunReader::open)
            .collect::<io::Result<Vec<_>>>()?;
        let mut heap = BinaryHeap::with_capacity(readers.len());
        for (i, reader) in readers.iter_mut().enumerate() {
            if let Some(r) = reader.next_record()? {
                heap.push(Reverse((r, i)));
            }
        }
        Ok(Merge { readers, heap })
    }

    fn next_record(&mut self) -> io::Result<Option<R>> {
        let Some(Reverse((record, source))) = self.heap.pop() else {
            return Ok(None);
        };
        if let Some(next) = self.readers[source].next_record()? {
            self.heap.push(Reverse((next, source)));
        }
        Ok(Some(record))
    }
}

impl<R: Record> std::fmt::Debug for Merge<R> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Merge")
            .field("runs", &self.readers.len())
            .field("pending", &self.heap.len())
            .finish()
    }
}

/// The sorted output stream: either the in-memory buffer (nothing spilled)
/// or a k-way merge over spill files. I/O errors surface as `Err` items.
#[derive(Debug)]
pub enum ExtSorted<R: Record> {
    Memory(std::vec::IntoIter<R>),
    Merge(Merge<R>),
}

impl<R: Record> Iterator for ExtSorted<R> {
    type Item = io::Result<R>;

    fn next(&mut self) -> Option<io::Result<R>> {
        match self {
            ExtSorted::Memory(it) => it.next().map(Ok),
            ExtSorted::Merge(m) => m.next_record().transpose(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn xorshift(state: &mut u64) -> u64 {
        *state ^= *state << 13;
        *state ^= *state >> 7;
        *state ^= *state << 17;
        *state
    }

    fn scratch(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("graphy-extsort-test-{}-{name}", std::process::id()))
    }

    fn run_sort(name: &str, budget: usize, n: usize, max_open: Option<usize>) {
        let dir = scratch(name);
        let mut state = 0x1234_5678_9ABC_DEF0 ^ n as u64;
        let values: Vec<u64> = (0..n).map(|_| xorshift(&mut state) % 1000).collect();
        let mut sorter: ExtSorter<u64> = ExtSorter::new(&dir, budget).unwrap();
        if let Some(m) = max_open {
            sorter.max_open_runs = m;
        }
        for &v in &values {
            sorter.push(v).unwrap();
        }
        let got: Vec<u64> = sorter.finish().unwrap().map(Result::unwrap).collect();
        let mut expected = values;
        expected.sort_unstable();
        assert_eq!(got, expected);
        // All spill files cleaned up.
        let leftovers = fs::read_dir(&dir).map(|d| d.count()).unwrap_or(0);
        assert_eq!(leftovers, 0, "spill files leaked");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    #[cfg_attr(miri, ignore)] // touches the filesystem via create_dir_all
    fn in_memory_when_under_budget() {
        run_sort("mem", 1 << 20, 1000, None);
    }

    #[test]
    #[cfg_attr(miri, ignore)] // file I/O
    fn spills_with_tiny_budget() {
        // 16 records per run over 10k records → ~625 runs is too many files;
        // use 160-record runs → 63 runs, single merge pass.
        run_sort("spill", 160 * 8, 10_000, None);
    }

    #[test]
    #[cfg_attr(miri, ignore)] // file I/O
    fn multi_pass_merge() {
        // 3-way fan-in over ~100 runs forces several merge passes.
        run_sort("multipass", 8 * 8, 800, Some(3));
    }

    #[test]
    #[cfg_attr(miri, ignore)] // file I/O
    fn wide_records_and_duplicates() {
        let dir = scratch("wide");
        let mut sorter: ExtSorter<[u64; 2]> = ExtSorter::new(&dir, 32 * 16).unwrap();
        let mut state = 7;
        let values: Vec<[u64; 2]> = (0..2000)
            .map(|_| [xorshift(&mut state) % 50, xorshift(&mut state) % 50])
            .collect();
        for &v in &values {
            sorter.push(v).unwrap();
        }
        let got: Vec<[u64; 2]> = sorter.finish().unwrap().map(Result::unwrap).collect();
        let mut expected = values;
        expected.sort_unstable();
        assert_eq!(got, expected);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    #[cfg_attr(miri, ignore)] // touches the filesystem via create_dir_all
    fn empty_input() {
        let dir = scratch("empty");
        let sorter: ExtSorter<u64> = ExtSorter::new(&dir, 64).unwrap();
        assert_eq!(sorter.finish().unwrap().count(), 0);
        let _ = fs::remove_dir_all(&dir);
    }
}
