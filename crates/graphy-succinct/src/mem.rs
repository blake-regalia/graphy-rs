//! Owner-backed byte and word views (doc 02 §6): the storage seam that lets
//! every succinct structure hold either heap-owned data or a zero-copy view
//! into an mmap'd segment component, without generics or lifetimes leaking
//! into the structure types.
//!
//! [`Bytes`] / [`Words`] pair a raw slice pointer with an `Arc` keep-alive of
//! whatever owns the allocation (a `Vec` or a memory map). They are cheap to
//! clone and slice. [`Words`] additionally guarantees 8-byte alignment so it
//! can be dereferenced as `[u64]` — the format's v2 alignment rule
//! (docs/08 §1) makes every word array in a component payload eligible.
//!
//! Word views reinterpret little-endian file bytes in place, so they are only
//! correct on little-endian hosts (the format is LE by design; big-endian
//! hosts are outside the tuning envelope and rejected at compile time).

#[cfg(target_endian = "big")]
compile_error!("graphy-succinct zero-copy views assume a little-endian host");

use std::any::Any;
use std::io;
use std::ops::Deref;
use std::sync::Arc;

/// Type-erased keep-alive for the allocation a view points into.
type Owner = Arc<dyn Any + Send + Sync>;

/// A cheaply-cloneable, owner-backed `[u8]` view.
pub struct Bytes {
    owner: Owner,
    ptr: *const u8,
    len: usize,
}

// SAFETY: `ptr` addresses immutable memory kept alive by `owner`
// (`Owner: Send + Sync`); no interior mutability is reachable through it,
// so sharing/sending the view is as safe as sharing `&[u8]`.
unsafe impl Send for Bytes {}
// SAFETY: as above — the view is read-only.
unsafe impl Sync for Bytes {}

impl Bytes {
    /// View an owned byte vector (no alignment guarantee — see
    /// [`Bytes::from_vec_aligned`] when word views will be carved out).
    pub fn from_vec(v: Vec<u8>) -> Bytes {
        let owner: Owner = Arc::new(v);
        let slice: &[u8] = owner
            .downcast_ref::<Vec<u8>>()
            .expect("owner constructed above")
            .as_slice();
        Bytes {
            ptr: slice.as_ptr(),
            len: slice.len(),
            owner,
        }
    }

    /// View an owned byte vector, copying into a `u64`-backed allocation iff
    /// the vector's buffer is not 8-byte aligned — the cheap guarantee that
    /// [`Cursor::take_words`] succeeds on heap-loaded payloads.
    pub fn from_vec_aligned(v: Vec<u8>) -> Bytes {
        if v.as_ptr() as usize % 8 == 0 {
            return Bytes::from_vec(v);
        }
        let len = v.len();
        let mut words = vec![0u64; len.div_ceil(8)];
        for (i, chunk) in v.chunks(8).enumerate() {
            let mut b = [0u8; 8];
            b[..chunk.len()].copy_from_slice(chunk);
            words[i] = u64::from_le_bytes(b);
        }
        let mut bytes = Bytes::from_vec_u64(words);
        bytes.len = len;
        bytes
    }

    /// View an owned word vector as its little-endian bytes (aligned).
    pub fn from_vec_u64(v: Vec<u64>) -> Bytes {
        let owner: Owner = Arc::new(v);
        let slice: &[u64] = owner
            .downcast_ref::<Vec<u64>>()
            .expect("owner constructed above")
            .as_slice();
        Bytes {
            ptr: slice.as_ptr().cast::<u8>(),
            len: slice.len() * 8,
            owner,
        }
    }

    /// View the full extent of an arbitrary owner (e.g. a memory map). The
    /// slice is derived *after* the owner is pinned behind `Arc`, so owners
    /// whose bytes live inline (rather than behind a pointer) are still safe.
    pub fn from_owner<T: AsRef<[u8]> + Send + Sync + 'static>(owner: T) -> Bytes {
        let owner: Owner = Arc::new(owner);
        let slice: &[u8] = owner
            .downcast_ref::<T>()
            .expect("owner constructed above")
            .as_ref();
        Bytes {
            ptr: slice.as_ptr(),
            len: slice.len(),
            owner,
        }
    }

    /// A sub-view sharing the same owner.
    pub fn slice(&self, offset: usize, len: usize) -> Bytes {
        assert!(
            offset.checked_add(len).is_some_and(|end| end <= self.len),
            "slice {offset}+{len} out of bounds ({})",
            self.len
        );
        Bytes {
            owner: Arc::clone(&self.owner),
            // SAFETY: `offset + len <= self.len` (asserted above), so the
            // result stays inside the view's allocation.
            ptr: unsafe { self.ptr.add(offset) },
            len,
        }
    }
}

impl Deref for Bytes {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        // SAFETY: `ptr..ptr+len` was derived from a live `&[u8]` of the
        // owner's allocation at construction; the `Arc` keep-alive pins it
        // and nothing can mutate it (no `&mut` is ever reachable).
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }
}

impl Clone for Bytes {
    fn clone(&self) -> Bytes {
        Bytes {
            owner: Arc::clone(&self.owner),
            ptr: self.ptr,
            len: self.len,
        }
    }
}

impl std::fmt::Debug for Bytes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Bytes").field("len", &self.len).finish()
    }
}

/// A cheaply-cloneable, owner-backed, 8-byte-aligned `[u64]` view.
pub struct Words {
    owner: Owner,
    ptr: *const u64,
    len: usize,
}

// SAFETY: read-only view into owner-pinned immutable memory (see `Bytes`).
unsafe impl Send for Words {}
// SAFETY: as above.
unsafe impl Sync for Words {}

impl Words {
    /// View an owned word vector.
    pub fn from_vec(v: Vec<u64>) -> Words {
        let owner: Owner = Arc::new(v);
        let slice: &[u64] = owner
            .downcast_ref::<Vec<u64>>()
            .expect("owner constructed above")
            .as_slice();
        Words {
            ptr: slice.as_ptr(),
            len: slice.len(),
            owner,
        }
    }

    /// Reinterpret a byte view as words. Errors if the view is not 8-byte
    /// aligned or not a whole number of words.
    pub fn try_from_bytes(bytes: Bytes) -> Result<Words, String> {
        if bytes.ptr as usize % 8 != 0 {
            return Err("byte view is not 8-byte aligned".to_owned());
        }
        if bytes.len % 8 != 0 {
            return Err(format!("byte length {} is not a whole word", bytes.len));
        }
        Ok(Words {
            ptr: bytes.ptr.cast::<u64>(),
            len: bytes.len / 8,
            owner: bytes.owner,
        })
    }
}

impl Deref for Words {
    type Target = [u64];

    fn deref(&self) -> &[u64] {
        // SAFETY: `ptr` is 8-byte aligned (checked at every construction
        // site) and `ptr..ptr+len` words lie inside the owner's pinned,
        // immutable allocation (see `Bytes::deref`). Words reinterpret
        // little-endian bytes; the module guards against big-endian hosts.
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }
}

impl Clone for Words {
    fn clone(&self) -> Words {
        Words {
            owner: Arc::clone(&self.owner),
            ptr: self.ptr,
            len: self.len,
        }
    }
}

impl std::fmt::Debug for Words {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Words").field("len", &self.len).finish()
    }
}

/// A forward-only reader over a [`Bytes`] view that hands out sub-views
/// (zero-copy deserialization; the counterpart of the `Read`-based path).
#[derive(Debug, Clone)]
pub struct Cursor {
    bytes: Bytes,
    at: usize,
}

impl Cursor {
    pub fn new(bytes: Bytes) -> Cursor {
        Cursor { bytes, at: 0 }
    }

    pub fn remaining(&self) -> usize {
        self.bytes.len - self.at
    }

    pub fn is_empty(&self) -> bool {
        self.remaining() == 0
    }

    /// Byte offset from the start of the underlying view.
    pub fn position(&self) -> usize {
        self.at
    }

    /// Skip zero padding up to the next 8-byte boundary (the reader-side
    /// counterpart of the writer's `pad_to_8`).
    pub fn align8(&mut self) -> io::Result<()> {
        let pad = (8 - self.at % 8) % 8;
        if pad > 0 {
            self.take_bytes(pad)?;
        }
        Ok(())
    }

    fn short(&self, need: usize) -> io::Error {
        io::Error::new(
            io::ErrorKind::UnexpectedEof,
            format!(
                "need {need} bytes at offset {}, have {}",
                self.at,
                self.remaining()
            ),
        )
    }

    pub fn read_u64(&mut self) -> io::Result<u64> {
        if self.remaining() < 8 {
            return Err(self.short(8));
        }
        let b: [u8; 8] = self.bytes[self.at..self.at + 8]
            .try_into()
            .expect("8 bytes");
        self.at += 8;
        Ok(u64::from_le_bytes(b))
    }

    /// Take the next `n` words as an owner-backed view (requires the
    /// underlying bytes to be 8-byte aligned — see [`Bytes::from_vec_aligned`]
    /// and the format's alignment rule).
    pub fn take_words(&mut self, n: usize) -> io::Result<Words> {
        let need = n.checked_mul(8).ok_or_else(|| self.short(usize::MAX))?;
        if self.remaining() < need {
            return Err(self.short(need));
        }
        let view = self.bytes.slice(self.at, need);
        let words = Words::try_from_bytes(view)
            .map_err(|m| io::Error::new(io::ErrorKind::InvalidData, m))?;
        self.at += need;
        Ok(words)
    }

    /// Take the next `n` bytes as an owner-backed view.
    pub fn take_bytes(&mut self, n: usize) -> io::Result<Bytes> {
        if self.remaining() < n {
            return Err(self.short(n));
        }
        let view = self.bytes.slice(self.at, n);
        self.at += n;
        Ok(view)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bytes_slice_and_clone() {
        let b = Bytes::from_vec((0u8..64).collect());
        assert_eq!(b.len(), 64);
        let s = b.slice(8, 4);
        assert_eq!(&*s, &[8, 9, 10, 11]);
        let s2 = s.clone();
        drop(b);
        drop(s);
        // Owner survives through the clone.
        assert_eq!(&*s2, &[8, 9, 10, 11]);
    }

    #[test]
    #[should_panic(expected = "out of bounds")]
    fn bytes_slice_bounds() {
        Bytes::from_vec(vec![0; 8]).slice(4, 5);
    }

    #[test]
    fn words_from_u64_vec() {
        let w = Words::from_vec(vec![1, 2, 3]);
        assert_eq!(&*w, &[1, 2, 3]);
        let b = Bytes::from_vec_u64(vec![0x0807060504030201, 0xFF]);
        assert_eq!(b[0], 0x01);
        assert_eq!(b[7], 0x08);
        let w = Words::try_from_bytes(b).unwrap();
        assert_eq!(w[1], 0xFF);
    }

    #[test]
    fn words_reject_misaligned_and_ragged() {
        let b = Bytes::from_vec_u64(vec![1, 2]);
        assert!(Words::try_from_bytes(b.slice(1, 8)).is_err()); // misaligned
        assert!(Words::try_from_bytes(b.slice(0, 12)).is_err()); // ragged
        assert!(Words::try_from_bytes(b.slice(8, 8)).is_ok());
    }

    #[test]
    fn from_vec_aligned_preserves_bytes() {
        // Whatever the allocator returns, content and length are identical.
        for len in [0usize, 1, 7, 8, 9, 4097] {
            let v: Vec<u8> = (0..len).map(|i| (i * 31) as u8).collect();
            let b = Bytes::from_vec_aligned(v.clone());
            assert_eq!(&*b, v.as_slice(), "len {len}");
            assert_eq!(b.ptr as usize % 8, 0, "aligned result");
        }
    }

    #[test]
    fn cursor_reads_and_views() {
        let mut payload = Vec::new();
        payload.extend_from_slice(&7u64.to_le_bytes());
        payload.extend_from_slice(&u64::MAX.to_le_bytes());
        payload.extend_from_slice(&42u64.to_le_bytes());
        payload.extend_from_slice(b"tail");
        let mut c = Cursor::new(Bytes::from_vec_aligned(payload));
        assert_eq!(c.read_u64().unwrap(), 7);
        let w = c.take_words(2).unwrap();
        assert_eq!(&*w, &[u64::MAX, 42]);
        let t = c.take_bytes(4).unwrap();
        assert_eq!(&*t, b"tail");
        assert!(c.is_empty());
        assert!(c.read_u64().is_err());
        // Views outlive the cursor.
        drop(c);
        assert_eq!(w[1], 42);
    }

    #[test]
    fn owner_backed_view() {
        // An arbitrary AsRef<[u8]> owner (stand-in for a memory map).
        let b = Bytes::from_owner(vec![9u8; 24].into_boxed_slice());
        assert_eq!(b.len(), 24);
        assert_eq!(b[23], 9);
    }
}
