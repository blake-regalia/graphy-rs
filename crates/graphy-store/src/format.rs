//! Component file format v2 (doc 02 §6, docs/08-segment-format.md).
//!
//! Every component file = a 64-byte header followed by the payload:
//!
//! ```text
//! [0..8)   magic  "GRFYCMP1"
//! [8..10)  kind   u16 LE
//! [10..12) version u16 LE (currently 2)
//! [12..16) reserved (zero)
//! [16..24) payload length u64 LE
//! [24..32) payload xxh3-64 digest u64 LE
//! [32..64) reserved (zero)
//! ```
//!
//! All multi-byte integers little-endian. Payloads are written streaming
//! through a counting/hashing writer; the header is patched afterwards.
//!
//! v2 alignment rule (docs/08 §1): every u64-typed field/array starts at an
//! 8-byte-aligned payload offset — interior byte-granular data is padded via
//! [`ComponentWriter::pad_to_8`] so mmap'd payloads (M3) can be viewed as
//! word slices without copying.

use std::fs::File;
use std::io::{self, BufWriter, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use xxhash_rust::xxh3::Xxh3;

pub const MAGIC: &[u8; 8] = b"GRFYCMP1";
pub const HEADER_LEN: usize = 64;
pub const FORMAT_VERSION: u16 = 2;

/// Component kinds (stable format identifiers).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
pub enum Kind {
    /// PFC dictionary section.
    Dict = 1,
    /// Sorted triple-term records.
    TripleTerms = 2,
    /// One BitmapTriples ordering.
    BitmapTriples = 3,
    /// Per-graph Roaring bitmaps over SPO triple ordinals.
    GraphsAt = 4,
    /// Triple-ordinal → graph-set accessor.
    GraphsTg = 5,
    /// Per-predicate statistics.
    PredStats = 6,
    /// Characteristic sets.
    CharSets = 7,
    /// Rebuildable term→ordinal hash sidecar for one dictionary section.
    HashSidecar = 8,
    /// FoQ wavelet accessors (compact profile).
    Foq = 9,
}

impl Kind {
    fn from_u16(v: u16) -> Option<Kind> {
        Some(match v {
            1 => Kind::Dict,
            2 => Kind::TripleTerms,
            3 => Kind::BitmapTriples,
            4 => Kind::GraphsAt,
            5 => Kind::GraphsTg,
            6 => Kind::PredStats,
            7 => Kind::CharSets,
            8 => Kind::HashSidecar,
            9 => Kind::Foq,
            _ => return None,
        })
    }
}

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("io error on {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("{path}: {message}")]
    Format { path: PathBuf, message: String },
    #[error("segment invariant violated: {0}")]
    Corrupt(String),
    #[error("manifest: {0}")]
    Manifest(String),
    #[error("unsupported on an ephemeral store: {0}")]
    Ephemeral(&'static str),
}

impl StoreError {
    pub(crate) fn io(path: &Path, source: io::Error) -> StoreError {
        StoreError::Io {
            path: path.to_owned(),
            source,
        }
    }

    pub(crate) fn format(path: &Path, message: impl Into<String>) -> StoreError {
        StoreError::Format {
            path: path.to_owned(),
            message: message.into(),
        }
    }
}

/// Streaming payload writer that counts bytes and folds them into xxh3.
pub struct ComponentWriter {
    inner: BufWriter<File>,
    hasher: Xxh3,
    written: u64,
}

impl std::fmt::Debug for ComponentWriter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ComponentWriter")
            .field("written", &self.written)
            .finish_non_exhaustive()
    }
}

impl ComponentWriter {
    /// Zero-pad the payload to the next 8-byte boundary (v2 alignment rule:
    /// call after interior byte-granular data, before the next u64 field).
    pub fn pad_to_8(&mut self) -> io::Result<()> {
        let rem = (self.written % 8) as usize;
        if rem != 0 {
            self.write_all(&[0u8; 8][..8 - rem])?;
        }
        Ok(())
    }
}

impl Write for ComponentWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.hasher.update(&buf[..n]);
        self.written += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// Write one component file: zeroed header, streamed payload, patched header.
/// Returns (payload length, digest) for the manifest.
pub fn write_component(
    path: &Path,
    kind: Kind,
    payload: impl FnOnce(&mut ComponentWriter) -> io::Result<()>,
) -> Result<(u64, u64), StoreError> {
    let run = || -> io::Result<(u64, u64)> {
        let file = File::create(path)?;
        let mut w = ComponentWriter {
            inner: BufWriter::new(file),
            hasher: Xxh3::new(),
            written: 0,
        };
        w.inner.write_all(&[0u8; HEADER_LEN])?;
        payload(&mut w)?;
        let len = w.written;
        let digest = w.hasher.digest();
        w.inner.flush()?;
        let mut file = w.inner.into_inner().map_err(io::Error::from)?;
        let mut header = [0u8; HEADER_LEN];
        header[..8].copy_from_slice(MAGIC);
        header[8..10].copy_from_slice(&(kind as u16).to_le_bytes());
        header[10..12].copy_from_slice(&FORMAT_VERSION.to_le_bytes());
        header[16..24].copy_from_slice(&len.to_le_bytes());
        header[24..32].copy_from_slice(&digest.to_le_bytes());
        file.seek(SeekFrom::Start(0))?;
        file.write_all(&header)?;
        file.sync_all()?;
        Ok((len, digest))
    };
    run().map_err(|e| StoreError::io(path, e))
}

/// Memory-map a component and return its payload as a zero-copy view (mmap
/// open mode, doc 02 §6). Validates magic, kind, version, and length; the
/// payload digest is NOT checked — checksumming would fault every page in,
/// defeating lazy loading. `graphy verify` (heap mode) covers digests.
/// The payload view is 8-byte aligned (page-aligned map + 64-byte header).
pub fn map_component(path: &Path, kind: Kind) -> Result<graphy_succinct::Bytes, StoreError> {
    let file = File::open(path).map_err(|e| StoreError::io(path, e))?;
    // SAFETY: mapping a file that another process truncates or rewrites
    // concurrently is undefined behavior at the OS level (SIGBUS). Manifest-
    // referenced segment components are immutable by design (doc 02 §6) —
    // they are written once, fsynced, and only ever replaced by whole new
    // segment directories — so a live segment's components never change
    // under a reader.
    let mmap = unsafe { memmap2::Mmap::map(&file) }.map_err(|e| StoreError::io(path, e))?;
    if mmap.len() < HEADER_LEN {
        return Err(StoreError::format(path, "file shorter than header"));
    }
    let header = &mmap[..HEADER_LEN];
    if &header[..8] != MAGIC {
        return Err(StoreError::format(path, "bad magic"));
    }
    let got_kind = u16::from_le_bytes(header[8..10].try_into().expect("2 bytes"));
    if Kind::from_u16(got_kind) != Some(kind) {
        return Err(StoreError::format(
            path,
            format!("component kind {got_kind} (expected {:?})", kind),
        ));
    }
    let version = u16::from_le_bytes(header[10..12].try_into().expect("2 bytes"));
    if version != FORMAT_VERSION {
        return Err(StoreError::format(
            path,
            format!("format version {version}"),
        ));
    }
    let len = u64::from_le_bytes(header[16..24].try_into().expect("8 bytes"));
    if mmap.len() as u64 != HEADER_LEN as u64 + len {
        return Err(StoreError::format(
            path,
            format!(
                "file is {} bytes, header says {}",
                mmap.len(),
                HEADER_LEN as u64 + len
            ),
        ));
    }
    let bytes = graphy_succinct::Bytes::from_owner(mmap);
    Ok(bytes.slice(HEADER_LEN, len as usize))
}

/// Read a whole component payload (heap mode), verifying magic, kind,
/// version, length, and digest.
pub fn read_component(path: &Path, kind: Kind) -> Result<Vec<u8>, StoreError> {
    let bytes = std::fs::read(path).map_err(|e| StoreError::io(path, e))?;
    parse_component(&bytes, kind, path)
}

/// [`read_component`] over in-memory bytes (embedded segment images,
/// docs/11): the identical header + digest verification, `ctx` names the
/// component in errors.
pub fn parse_component(bytes: &[u8], kind: Kind, ctx: &Path) -> Result<Vec<u8>, StoreError> {
    if bytes.len() < HEADER_LEN {
        return Err(StoreError::format(ctx, "shorter than header"));
    }
    let (header, payload) = bytes.split_at(HEADER_LEN);
    if &header[..8] != MAGIC {
        return Err(StoreError::format(ctx, "bad magic"));
    }
    let got_kind = u16::from_le_bytes(header[8..10].try_into().expect("2 bytes"));
    if Kind::from_u16(got_kind) != Some(kind) {
        return Err(StoreError::format(
            ctx,
            format!("component kind {got_kind} (expected {:?})", kind),
        ));
    }
    let version = u16::from_le_bytes(header[10..12].try_into().expect("2 bytes"));
    if version != FORMAT_VERSION {
        return Err(StoreError::format(ctx, format!("format version {version}")));
    }
    let len = u64::from_le_bytes(header[16..24].try_into().expect("8 bytes"));
    let digest = u64::from_le_bytes(header[24..32].try_into().expect("8 bytes"));
    if payload.len() as u64 != len {
        return Err(StoreError::format(
            ctx,
            format!("payload is {} bytes, header says {len}", payload.len()),
        ));
    }
    if xxhash_rust::xxh3::xxh3_64(payload) != digest {
        return Err(StoreError::format(ctx, "checksum mismatch"));
    }
    Ok(payload.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("graphy-store-fmt-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(name)
    }

    #[test]
    fn component_round_trip_and_corruption() {
        let path = scratch("c1.bin");
        let (len, _) =
            write_component(&path, Kind::Dict, |w| w.write_all(b"hello payload")).unwrap();
        assert_eq!(len, 13);
        assert_eq!(read_component(&path, Kind::Dict).unwrap(), b"hello payload");
        // Wrong kind.
        assert!(read_component(&path, Kind::PredStats).is_err());
        // Flip a payload byte: checksum failure.
        let mut bytes = std::fs::read(&path).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 0xFF;
        std::fs::write(&path, &bytes).unwrap();
        let err = read_component(&path, Kind::Dict).unwrap_err();
        assert!(err.to_string().contains("checksum"));
    }
}
