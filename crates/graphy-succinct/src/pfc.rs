//! Plain front coding for sorted byte-string dictionaries (doc 02). Keys are
//! grouped into blocks of `block_size`; each block stores its head verbatim
//! (`varint(len) bytes`) and every other entry as
//! `varint(lcp) varint(suffix_len) suffix` against its predecessor.
//! `locate` binary-searches the heads, then scans one block.

use crate::mem::{Bytes, Words};
use crate::varint;

pub const DEFAULT_BLOCK_SIZE: usize = 32;

/// Builder; keys must be pushed in ascending byte order for [`Pfc::locate`]
/// to work (enforced by debug_assert; duplicates are allowed but pointless).
#[derive(Debug)]
pub struct PfcBuilder {
    block_size: usize,
    n: usize,
    data: Vec<u8>,
    block_offsets: Vec<u64>,
    last: Vec<u8>,
}

impl Default for PfcBuilder {
    fn default() -> Self {
        Self::new(DEFAULT_BLOCK_SIZE)
    }
}

impl PfcBuilder {
    pub fn new(block_size: usize) -> PfcBuilder {
        assert!(block_size >= 1);
        PfcBuilder {
            block_size,
            n: 0,
            data: Vec::new(),
            block_offsets: Vec::new(),
            last: Vec::new(),
        }
    }

    pub fn push(&mut self, key: &[u8]) {
        debug_assert!(
            self.n == 0 || self.last.as_slice() <= key,
            "PFC keys must be pushed in sorted order"
        );
        if self.n % self.block_size == 0 {
            self.block_offsets.push(self.data.len() as u64);
            varint::write(&mut self.data, key.len() as u64);
            self.data.extend_from_slice(key);
        } else {
            let lcp = common_prefix(&self.last, key);
            varint::write(&mut self.data, lcp as u64);
            varint::write(&mut self.data, (key.len() - lcp) as u64);
            self.data.extend_from_slice(&key[lcp..]);
        }
        self.last.clear();
        self.last.extend_from_slice(key);
        self.n += 1;
    }

    pub fn build(self) -> Pfc {
        Pfc {
            block_size: self.block_size,
            n: self.n,
            data: Bytes::from_vec(self.data),
            block_offsets: Words::from_vec(self.block_offsets),
        }
    }
}

/// An immutable front-coded dictionary of sorted byte strings. The coded
/// heap and offset index may be zero-copy views (mmap).
#[derive(Debug, Clone)]
pub struct Pfc {
    block_size: usize,
    n: usize,
    data: Bytes,
    block_offsets: Words,
}

impl Pfc {
    pub fn len(&self) -> usize {
        self.n
    }

    pub fn is_empty(&self) -> bool {
        self.n == 0
    }

    pub fn block_size(&self) -> usize {
        self.block_size
    }

    /// The raw front-coded byte heap.
    pub fn data(&self) -> &[u8] {
        &self.data[..]
    }

    /// Byte offset of each block within the heap.
    pub fn block_offsets(&self) -> &[u64] {
        &self.block_offsets[..]
    }

    /// Reassemble from raw parts (deserialization), walking every block with
    /// bounds-checked decoding so accessors can never panic afterwards.
    /// Sort order of the keys is NOT verified here (callers relying on
    /// `locate` over untrusted data should verify separately).
    pub fn from_parts(
        block_size: usize,
        n: usize,
        data: Vec<u8>,
        block_offsets: Vec<u64>,
    ) -> Result<Pfc, String> {
        Self::from_views(
            block_size,
            n,
            Bytes::from_vec(data),
            Words::from_vec(block_offsets),
        )
    }

    /// [`Pfc::from_parts`] over owner-backed (possibly zero-copy) views.
    pub fn from_views(
        block_size: usize,
        n: usize,
        data: Bytes,
        block_offsets: Words,
    ) -> Result<Pfc, String> {
        if block_size == 0 {
            return Err("block size must be at least 1".to_owned());
        }
        if block_offsets.len() != n.div_ceil(block_size) {
            return Err("block offset count does not match key count".to_owned());
        }
        let pfc = Pfc {
            block_size,
            n,
            data,
            block_offsets,
        };
        pfc.validate_walk()?;
        Ok(pfc)
    }

    /// Bounds-checked decode of every entry (validation for untrusted input).
    fn validate_walk(&self) -> Result<(), String> {
        let read_varint = |at: &mut usize| -> Result<u64, String> {
            match varint::read(self.data.get(*at..).unwrap_or(&[])) {
                Some((v, n)) => {
                    *at += n;
                    Ok(v)
                }
                None => Err(format!("truncated varint at byte {at}")),
            }
        };
        for (b, &off) in self.block_offsets.iter().enumerate() {
            let mut at = off as usize;
            if at > self.data.len() {
                return Err(format!("block {b} offset {at} out of bounds"));
            }
            if b + 1 < self.block_offsets.len() && self.block_offsets[b + 1] <= off {
                return Err(format!("block offsets not strictly increasing at {b}"));
            }
            let in_block = (self.n - b * self.block_size).min(self.block_size);
            let head_len = read_varint(&mut at)? as usize;
            let mut key_len = head_len;
            at = at
                .checked_add(head_len)
                .filter(|&e| e <= self.data.len())
                .ok_or_else(|| format!("block {b} head out of bounds"))?;
            for e in 1..in_block {
                let lcp = read_varint(&mut at)? as usize;
                let suffix = read_varint(&mut at)? as usize;
                if lcp > key_len {
                    return Err(format!("block {b} entry {e}: lcp exceeds previous key"));
                }
                key_len = lcp + suffix;
                at = at
                    .checked_add(suffix)
                    .filter(|&e| e <= self.data.len())
                    .ok_or_else(|| format!("block {b} entry {e} out of bounds"))?;
            }
        }
        Ok(())
    }

    /// The `idx`-th key in sorted order.
    pub fn get(&self, idx: usize) -> Option<Vec<u8>> {
        if idx >= self.n {
            return None;
        }
        let mut cursor = BlockCursor::new(self, idx / self.block_size);
        for _ in 0..idx % self.block_size {
            cursor.advance();
        }
        Some(cursor.key)
    }

    /// Index of `key`, if present.
    pub fn locate(&self, key: &[u8]) -> Option<usize> {
        if self.n == 0 {
            return None;
        }
        // Last block whose head is <= key.
        let block = self.partition_by_head(key)?;
        let mut cursor = BlockCursor::new(self, block);
        let in_block = (self.n - block * self.block_size).min(self.block_size);
        for i in 0..in_block {
            match cursor.key.as_slice().cmp(key) {
                std::cmp::Ordering::Equal => return Some(block * self.block_size + i),
                std::cmp::Ordering::Greater => return None,
                std::cmp::Ordering::Less => {}
            }
            if i + 1 < in_block {
                cursor.advance();
            }
        }
        None
    }

    /// All keys in order.
    pub fn iter(&self) -> PfcIter<'_> {
        PfcIter {
            pfc: self,
            next: 0,
            cursor: None,
        }
    }

    /// Largest block index whose head is `<= key` (`None` if even the first
    /// head is greater).
    fn partition_by_head(&self, key: &[u8]) -> Option<usize> {
        let (mut lo, mut hi) = (0, self.block_offsets.len());
        // Invariant: heads before lo are <= key, heads at/after hi are > key.
        while lo < hi {
            let mid = (lo + hi) / 2;
            if self.head(mid) <= key {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        lo.checked_sub(1)
    }

    fn head(&self, block: usize) -> &[u8] {
        let at = self.block_offsets[block] as usize;
        let (len, n) = varint::read(&self.data[at..]).expect("valid PFC block header");
        &self.data[at + n..at + n + len as usize]
    }
}

/// Sequential decoder positioned inside one block.
#[derive(Debug)]
struct BlockCursor<'a> {
    pfc: &'a Pfc,
    at: usize,
    key: Vec<u8>,
}

impl<'a> BlockCursor<'a> {
    fn new(pfc: &'a Pfc, block: usize) -> BlockCursor<'a> {
        let at = pfc.block_offsets[block] as usize;
        let (len, n) = varint::read(&pfc.data[at..]).expect("valid PFC block header");
        let start = at + n;
        let end = start + len as usize;
        BlockCursor {
            pfc,
            at: end,
            key: pfc.data[start..end].to_vec(),
        }
    }

    /// Decode the next entry of the block into `self.key`.
    fn advance(&mut self) {
        let (lcp, n) = varint::read(&self.pfc.data[self.at..]).expect("valid PFC entry");
        self.at += n;
        let (suffix_len, n) = varint::read(&self.pfc.data[self.at..]).expect("valid PFC entry");
        self.at += n;
        self.key.truncate(lcp as usize);
        self.key
            .extend_from_slice(&self.pfc.data[self.at..self.at + suffix_len as usize]);
        self.at += suffix_len as usize;
    }
}

/// Iterator over all keys in sorted order.
#[derive(Debug)]
pub struct PfcIter<'a> {
    pfc: &'a Pfc,
    next: usize,
    cursor: Option<BlockCursor<'a>>,
}

impl Iterator for PfcIter<'_> {
    type Item = Vec<u8>;

    fn next(&mut self) -> Option<Vec<u8>> {
        if self.next >= self.pfc.n {
            return None;
        }
        if self.next % self.pfc.block_size == 0 {
            self.cursor = Some(BlockCursor::new(self.pfc, self.next / self.pfc.block_size));
        } else {
            self.cursor
                .as_mut()
                .expect("cursor set at block head")
                .advance();
        }
        self.next += 1;
        Some(self.cursor.as_ref().expect("cursor set above").key.clone())
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let rem = self.pfc.n - self.next;
        (rem, Some(rem))
    }
}

fn common_prefix(a: &[u8], b: &[u8]) -> usize {
    a.iter().zip(b).take_while(|(x, y)| x == y).count()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_keys(n: usize) -> Vec<Vec<u8>> {
        let mut keys: Vec<Vec<u8>> = (0..n)
            .map(|i| format!("http://ex.example/resource/{:06}/{}", i * 7 % n, i).into_bytes())
            .collect();
        keys.sort();
        keys.dedup();
        keys
    }

    #[test]
    fn round_trip_get_and_iter() {
        for block_size in [1, 2, 32, 1000] {
            let keys = sample_keys(300);
            let mut b = PfcBuilder::new(block_size);
            for k in &keys {
                b.push(k);
            }
            let pfc = b.build();
            assert_eq!(pfc.len(), keys.len());
            for (i, k) in keys.iter().enumerate() {
                assert_eq!(
                    pfc.get(i).as_ref(),
                    Some(k),
                    "block_size {block_size} get({i})"
                );
            }
            assert_eq!(pfc.get(keys.len()), None);
            assert_eq!(pfc.iter().collect::<Vec<_>>(), keys);
        }
    }

    #[test]
    fn locate_hits_and_misses() {
        let keys = sample_keys(300);
        let mut b = PfcBuilder::new(7);
        for k in &keys {
            b.push(k);
        }
        let pfc = b.build();
        for (i, k) in keys.iter().enumerate() {
            assert_eq!(pfc.locate(k), Some(i));
        }
        // Misses: before-all, between, after-all, prefixes and extensions.
        assert_eq!(pfc.locate(b""), None);
        assert_eq!(pfc.locate(b"http://"), None);
        assert_eq!(pfc.locate(b"zzz"), None);
        let mut extended = keys[17].clone();
        extended.push(b'!');
        assert_eq!(pfc.locate(&extended), None);
        let truncated = &keys[17][..keys[17].len() - 1];
        assert!(pfc.locate(truncated).is_none() || keys.iter().any(|k| k == truncated));
    }

    #[test]
    fn empty_and_single() {
        let pfc = PfcBuilder::new(32).build();
        assert!(pfc.is_empty());
        assert_eq!(pfc.get(0), None);
        assert_eq!(pfc.locate(b"x"), None);
        assert_eq!(pfc.iter().count(), 0);

        let mut b = PfcBuilder::new(32);
        b.push(b"only");
        let pfc = b.build();
        assert_eq!(pfc.get(0).as_deref(), Some(b"only".as_slice()));
        assert_eq!(pfc.locate(b"only"), Some(0));
        assert_eq!(pfc.locate(b"onl"), None);
        assert_eq!(pfc.locate(b"onlyy"), None);
    }

    #[test]
    fn empty_key_and_shared_prefixes() {
        let keys: Vec<&[u8]> = vec![b"", b"a", b"aa", b"aaa", b"aab", b"ab", b"b"];
        let mut b = PfcBuilder::new(3);
        for k in &keys {
            b.push(k);
        }
        let pfc = b.build();
        for (i, k) in keys.iter().enumerate() {
            assert_eq!(pfc.get(i).as_deref(), Some(*k));
            assert_eq!(pfc.locate(k), Some(i));
        }
    }
}
