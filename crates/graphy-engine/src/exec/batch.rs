//! Columnar solution batches (doc 05 §2): the unit of exchange between
//! physical operators. One column per query variable (dense — the
//! variable table is small), values are [`B`] bindings (`TermId` or a
//! query-local computed term), `None` = unbound. Strings appear nowhere
//! until serialization.

use crate::eval::{Row, B};

/// Default batch capacity (doc 05 §2).
pub(crate) const BATCH_CAP: usize = 1024;

/// A fixed-capacity columnar batch of solution rows.
#[derive(Debug, Clone)]
pub(crate) struct Batch {
    /// One column per query variable, all `len` long.
    pub cols: Vec<Vec<Option<B>>>,
    pub len: usize,
}

impl Batch {
    pub fn new(nvars: usize) -> Batch {
        Batch {
            cols: (0..nvars).map(|_| Vec::with_capacity(BATCH_CAP)).collect(),
            len: 0,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn is_full(&self) -> bool {
        self.len >= BATCH_CAP
    }

    /// Append one row (must have exactly `nvars` cells).
    pub fn push_row(&mut self, row: &[Option<B>]) {
        debug_assert_eq!(row.len(), self.cols.len());
        for (col, cell) in self.cols.iter_mut().zip(row) {
            col.push(*cell);
        }
        self.len += 1;
    }

    /// Gather row `i` into `out` (resized to the variable count).
    pub fn row_into(&self, i: usize, out: &mut Row) {
        out.clear();
        out.extend(self.cols.iter().map(|c| c[i]));
    }

    /// Gather row `i` as a fresh [`Row`].
    pub fn row_at(&self, i: usize) -> Row {
        self.cols.iter().map(|c| c[i]).collect()
    }
}
