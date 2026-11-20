//! A list of fixed-width rows kept as one content, named by its identity (A-724 piece 8c; §4.17.5 "The partition's
//! snapshot, sized by its data"; main's ruling of 2026-11-20).
//!
//! **Why.** A git push's record names every object it brought (its type, content identity and length: 41 bytes), and
//! records sum into the partition's snapshot: about 410 MiB for the Linux kernel's ten million objects against a
//! 746,962,928-byte slot on the Studio, a monorepo many slots. Kept as content instead, the rows cost the store their
//! bytes once (deduplicated, shared, homed like any content) and the record one identity and a count: about 100 bytes a
//! push, whatever its size.
//!
//! **The form.** The rows are written back to back, each exactly `width` bytes, in their order, and put whole
//! ([`put`]: `ContentStore::put_object`, so a list longer than a window is cut at the store's window and named by its
//! root). The record keeps the root and the row count. [`read`] reads the content back by its root and checks, before it
//! allocates anything, that its length is the count times the width; [`rows_of`] cuts bytes into rows under the same
//! check. A mismatch is refused typed ([`RowsRefused`]), never a panic: the bytes may be hostile.

use crate::content::{ContentId, ContentRefused, ContentStore};

/// Why a row list was not put or read (module docs).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RowsRefused {
  /// A row of another width than the list's.
  Width {
    /// The row's width.
    got: usize,
    /// The list's.
    width: usize,
  },
  /// The content's length is not the count times the width, or the product overflows.
  Length {
    /// The content's length.
    len: u64,
    /// The rows asked.
    count: u64,
    /// The width.
    width: usize,
  },
  /// The store holds no content of that root whole.
  Missing,
  /// The store refused (its budget, or a count exhausted).
  Content(ContentRefused),
}

/// The bytes of `rows`, each `width` wide, back to back: refused when a row is of another width.
pub fn encode<'r>(
  rows: impl IntoIterator<Item = &'r [u8]>,
  width: usize,
) -> Result<Vec<u8>, RowsRefused> {
  let mut out = Vec::new();
  for row in rows {
    if row.len() != width {
      return Err(RowsRefused::Width {
        got: row.len(),
        width,
      });
    }
    out.extend_from_slice(row);
  }
  Ok(out)
}

/// Puts the row list `bytes` (as [`encode`] wrote it) into `store` whole (module docs): its root identity, held once more
/// for the caller.
pub fn put(store: &mut ContentStore, bytes: &[u8]) -> Result<ContentId, RowsRefused> {
  store.put_object(bytes).map_err(RowsRefused::Content)
}

/// The `count` rows of `width` in `bytes`, in order, after checking the length is exactly `count × width` (module docs).
pub fn rows_of(bytes: &[u8], count: u64, width: usize) -> Result<Vec<&[u8]>, RowsRefused> {
  let len = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
  let refused = RowsRefused::Length { len, count, width };
  let wide = u64::try_from(width).map_err(|_| refused.clone())?;
  if count.checked_mul(wide) != Some(len) || (width == 0 && count != 0) {
    return Err(refused);
  }
  if width == 0 {
    return Ok(Vec::new());
  }
  Ok(bytes.chunks_exact(width).collect())
}

/// The row list `root` names in `store`, `count` rows of `width`, read back whole (module docs): its bytes, checked as
/// [`rows_of`] checks them before anything is read past the length.
pub fn read(
  store: &ContentStore,
  root: &ContentId,
  count: u64,
  width: usize,
) -> Result<Vec<u8>, RowsRefused> {
  let wide = u64::try_from(width).unwrap_or(u64::MAX);
  let len = count.checked_mul(wide).ok_or(RowsRefused::Length {
    len: u64::MAX,
    count,
    width,
  })?;
  let bytes = store.object_bytes(root, len).ok_or(RowsRefused::Missing)?;
  rows_of(&bytes, count, width)?;
  Ok(bytes)
}

#[cfg(test)]
mod tests {
  use super::{RowsRefused, encode, put, read, rows_of};
  use crate::content::ContentStore;
  use proptest::prelude::*;

  /// Shape: a git name row's width (A-723): its type (1), its content identity (32) and its length (8).
  const WIDTH: usize = 41;
  /// Shape: a small window, so lists span several windows and their roots are lists.
  const WINDOW: usize = 512;
  /// Shape: a budget no test meets.
  const BUDGET: u64 = 1 << 30;

  proptest! {
    #![proptest_config(slates_test_seeds::unseeded(ProptestConfig::with_cases(200)))]

    /// A-724 piece 8c. Do: put any list of 41-byte rows (empty, one window, many windows), read it back by its root and
    /// count, and cut it. Expect: the rows as written, in order; one identity for the same rows put twice, holding them
    /// once more; a read with another count refused `Length` before anything past it is read.
    #[test]
    fn a_row_list_put_whole_reads_back_by_its_root_and_count(
      rows in proptest::collection::vec(proptest::collection::vec(any::<u8>(), WIDTH), 0..120),
    ) {
      let mut store = ContentStore::new(BUDGET, WINDOW).unwrap();
      let bytes = encode(rows.iter().map(Vec::as_slice), WIDTH).unwrap();
      let root = put(&mut store, &bytes).unwrap();
      let count = u64::try_from(rows.len()).unwrap();
      let back = read(&store, &root, count, WIDTH).unwrap();
      let cut: Vec<Vec<u8>> = rows_of(&back, count, WIDTH).unwrap().into_iter().map(<[u8]>::to_vec).collect();
      prop_assert_eq!(&cut, &rows);
      prop_assert_eq!(put(&mut store, &bytes).unwrap(), root);
      let off_count = read(&store, &root, count + 1, WIDTH);
      let refused = matches!(off_count, Err(RowsRefused::Length { .. } | RowsRefused::Missing));
      prop_assert!(refused, "another count was read: {:?}", off_count.map(|bytes| bytes.len()));
    }
  }

  /// A-724 piece 8c, hostile input. Do: cut bytes one short of their count, a count whose bytes overflow, a row of
  /// another width, rows of width zero, and read a root the store does not hold. Expect: each refused by its kind.
  #[test]
  fn hostile_row_lists_are_refused_typed() {
    let bytes = vec![7u8; WIDTH * 3 - 1];
    assert!(matches!(
      rows_of(&bytes, 3, WIDTH),
      Err(RowsRefused::Length { .. })
    ));
    assert!(matches!(
      rows_of(&bytes, u64::MAX, WIDTH),
      Err(RowsRefused::Length { .. })
    ));
    assert!(matches!(
      encode([&[1u8, 2][..]], WIDTH),
      Err(RowsRefused::Width {
        got: 2,
        width: WIDTH
      })
    ));
    assert_zero_width_rows_and_an_absent_root_refused();
  }

  /// Rows of width zero: one row refused by its length, none read as none; and a root the store does not hold refused
  /// `Missing`.
  fn assert_zero_width_rows_and_an_absent_root_refused() {
    assert!(matches!(
      rows_of(&[], 1, 0),
      Err(RowsRefused::Length { .. })
    ));
    assert_eq!(rows_of(&[], 0, 0), Ok(Vec::new()));
    let store = ContentStore::new(BUDGET, WINDOW).unwrap();
    let absent = crate::content::ContentId::of(b"never put");
    assert_eq!(read(&store, &absent, 1, WIDTH), Err(RowsRefused::Missing));
  }

  /// A-724 piece 8c, the figure that motivates it. Do: put the name rows of 12,232 objects (git/git cut at v1.0.0).
  /// Expect: the rows' bytes once in the store (501,512 bytes, about 41 a row) behind one 32-byte root, which with its
  /// count is all a record needs to keep.
  #[test]
  fn a_pushs_names_cost_a_record_one_root_and_a_count() {
    let mut store = ContentStore::new(BUDGET, WINDOW).unwrap();
    let rows: Vec<Vec<u8>> = (0..12_232u32)
      .map(|n| {
        let mut row = vec![0u8; WIDTH];
        row[..4].copy_from_slice(&n.to_le_bytes());
        row
      })
      .collect();
    let bytes = encode(rows.iter().map(Vec::as_slice), WIDTH).unwrap();
    assert_eq!(bytes.len(), 12_232 * WIDTH);
    let root = put(&mut store, &bytes).unwrap();
    assert_eq!(root.0.len(), 32);
    assert_eq!(read(&store, &root, 12_232, WIDTH).unwrap(), bytes);
  }
}
