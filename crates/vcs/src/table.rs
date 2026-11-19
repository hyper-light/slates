//! An immutable table of rows sorted by key, whose bytes are content (§4.17.1 "Objects" item 3: "immutable sorted
//! tables that are content themselves, so a fork shares its parent's tables by identity").
//!
//! **A row** is a key, the contents it names and a payload. The key is the tool's id (or any key a tool's own index
//! sorts by), the contents are the content identities the row holds (an object row names its raw bytes; a tool's row
//! may name more), and the payload is the rest of the row in the tool's own layout. Naming the contents in the row's
//! own bytes, rather than leaving them to a tool's codec, lets any holder release a freed table's contents without
//! knowing which tool wrote it.
//!
//! **The encoding** (all integers little-endian):
//!
//! ```text
//! table   = magic (8 bytes, TABLE_MAGIC) | rows (u64) | offsets ((rows + 1) × u64) | entries
//! offsets : entry i is entries[offset i .. offset i+1]; offset 0 is 0 and the last is the entries' length
//! entry   = key length (u32) | key | body
//! body    = count (u8) | count contents (each IDENTITY_BYTES) | payload (the rest)
//! ```
//!
//! Keys strictly increase, so a lookup is a binary search over the offsets and no row is read whole to find another.
//! A table is at most one window of the content plane ([`crate::content`]): the index cuts its rows into tables by
//! [`row_bytes`] and [`TABLE_FRAME_BYTES`], and the store refuses a longer one. Offsets are 64 bits, as wide as any
//! length the encoding names; a narrower offset for a window's table is a measured improvement left for later.
//!
//! **Hostile input (§4.9).** [`TableView::open`] admits bytes from anywhere: it checks the frame (the magic, the row
//! count against the bytes there are, the offsets' region, the entries' length), then every entry (its key length
//! inside it, its body's count inside it) and the keys' order, and refuses each fault by kind. A table this crate built,
//! or opened whole once, is reopened by [`TableView::open_known`], which checks the frame only; every read after it is
//! a checked `get`, so even a table that changed under it reads as absent rows, never a panic.

use std::cmp::Ordering;
use std::mem::size_of;

use crate::content::{ContentId, IDENTITY_BYTES};

/// Format: a table's first eight bytes, which say the bytes are one.
pub const TABLE_MAGIC: [u8; 8] = *b"slvcstb1";

/// Format: the bytes of one offset, and of the row count.
const WORD_BYTES: usize = size_of::<u64>();

/// Format: the header, the magic then the row count.
const HEADER_BYTES: usize = TABLE_MAGIC.len() + WORD_BYTES;

/// Format: the bytes of an entry's key length.
const KEY_LENGTH_BYTES: usize = size_of::<u32>();

/// Format: the bytes of a body's count of contents.
const COUNT_BYTES: usize = size_of::<u8>();

/// Format: the bytes a table takes beyond its rows' ([`row_bytes`]): the header, and the offset that ends the last
/// entry.
pub const TABLE_FRAME_BYTES: usize = HEADER_BYTES + WORD_BYTES;

/// The bytes a row of `key` and `body` takes in a table: its offset, its key's length, its key and its body; `None` past
/// the integers. A table of rows is [`TABLE_FRAME_BYTES`] plus each row's.
pub fn row_bytes(key: &[u8], body: &[u8]) -> Option<usize> {
  WORD_BYTES
    .checked_add(KEY_LENGTH_BYTES)?
    .checked_add(key.len())?
    .checked_add(body.len())
}

/// Why bytes are not a table, or why a row cannot be written into one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TableRefused {
  /// Shorter than its own frame says.
  Truncated,
  /// Its first bytes are not [`TABLE_MAGIC`].
  ForeignMagic,
  /// Longer than its own frame says.
  TrailingBytes,
  /// Entry `row` breaks the format: its offsets do not increase, its key length passes its end, or its body's count
  /// of contents does.
  Malformed {
    /// The entry, counted from zero.
    row: u64,
  },
  /// Entry `row`'s key is not after the key before it.
  Unsorted {
    /// The entry, counted from zero.
    row: u64,
  },
  /// A row names more contents than a body's count can say.
  TooManyContents,
  /// A length past the encoding's integers.
  TooLarge,
}

/// One row: its key and its body (module docs), read in place from a table or a tail.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RowRef<'a> {
  key: &'a [u8],
  contents: &'a [u8],
  payload: &'a [u8],
  body: &'a [u8],
}

impl<'a> RowRef<'a> {
  /// The row of `key` whose body is `body`, or `None` when the body's count passes its end.
  pub fn new(key: &'a [u8], body: &'a [u8]) -> Option<RowRef<'a>> {
    let (&count, rest) = body.split_first()?;
    let contents_len = usize::from(count).checked_mul(IDENTITY_BYTES)?;
    let contents = rest.get(..contents_len)?;
    let payload = rest.get(contents_len..)?;
    Some(RowRef {
      key,
      contents,
      payload,
      body,
    })
  }

  /// The key.
  pub fn key(&self) -> &'a [u8] {
    self.key
  }

  /// The body: the count, the contents and the payload as encoded.
  pub fn body(&self) -> &'a [u8] {
    self.body
  }

  /// The contents the row names, in its order.
  pub fn contents(&self) -> impl Iterator<Item = ContentId> + use<'a> {
    self
      .contents
      .chunks_exact(IDENTITY_BYTES)
      .filter_map(ContentId::from_slice)
  }

  /// The payload.
  pub fn payload(&self) -> &'a [u8] {
    self.payload
  }
}

/// The body of a row naming `contents` with `payload`.
pub fn encode_body(contents: &[ContentId], payload: &[u8]) -> Result<Vec<u8>, TableRefused> {
  let count = u8::try_from(contents.len()).map_err(|_| TableRefused::TooManyContents)?;
  let len = contents
    .len()
    .checked_mul(IDENTITY_BYTES)
    .and_then(|bytes| bytes.checked_add(COUNT_BYTES))
    .and_then(|bytes| bytes.checked_add(payload.len()))
    .ok_or(TableRefused::TooLarge)?;
  let mut body = Vec::with_capacity(len);
  body.push(count);
  for content in contents {
    body.extend_from_slice(&content.0);
  }
  body.extend_from_slice(payload);
  Ok(body)
}

/// Reads the little-endian word at `at` in `bytes`.
fn word(bytes: &[u8], at: usize) -> Option<u64> {
  let end = at.checked_add(WORD_BYTES)?;
  let field = bytes.get(at..end)?;
  <[u8; WORD_BYTES]>::try_from(field)
    .ok()
    .map(u64::from_le_bytes)
}

/// A table read in place (module docs).
#[derive(Clone, Copy, Debug)]
pub struct TableView<'a> {
  offsets: &'a [u8],
  entries: &'a [u8],
  rows: usize,
}

impl<'a> TableView<'a> {
  /// Opens bytes from anywhere: the frame, every entry and the keys' order are checked (module docs).
  pub fn open(bytes: &'a [u8]) -> Result<TableView<'a>, TableRefused> {
    let view = TableView::frame(bytes)?;
    let mut previous: Option<&[u8]> = None;
    for at in 0..view.rows {
      let row = u64::try_from(at).map_err(|_| TableRefused::TooLarge)?;
      let entry = view.entry(at).ok_or(TableRefused::Malformed { row })?;
      let found = TableView::split(entry).ok_or(TableRefused::Malformed { row })?;
      if previous.is_some_and(|previous| previous >= found.key) {
        return Err(TableRefused::Unsorted { row });
      }
      previous = Some(found.key);
    }
    Ok(view)
  }

  /// Opens a table this crate built, or opened whole before: the frame alone is checked (module docs).
  pub fn open_known(bytes: &'a [u8]) -> Result<TableView<'a>, TableRefused> {
    TableView::frame(bytes)
  }

  /// Checks the frame: the magic, the row count against the bytes, the offsets' ends.
  fn frame(bytes: &'a [u8]) -> Result<TableView<'a>, TableRefused> {
    let magic = bytes
      .get(..TABLE_MAGIC.len())
      .ok_or(TableRefused::Truncated)?;
    if magic != TABLE_MAGIC {
      return Err(TableRefused::ForeignMagic);
    }
    let rows = word(bytes, TABLE_MAGIC.len()).ok_or(TableRefused::Truncated)?;
    let after_header = bytes.get(HEADER_BYTES..).ok_or(TableRefused::Truncated)?;
    let offsets_len = usize::try_from(rows)
      .ok()
      .and_then(|rows| rows.checked_add(1))
      .and_then(|words| words.checked_mul(WORD_BYTES))
      .ok_or(TableRefused::Truncated)?;
    if offsets_len > after_header.len() {
      return Err(TableRefused::Truncated);
    }
    let (offsets, entries) = after_header.split_at(offsets_len);
    let rows = usize::try_from(rows).map_err(|_| TableRefused::TooLarge)?;
    let view = TableView {
      offsets,
      entries,
      rows,
    };
    view.check_ends()?;
    Ok(view)
  }

  /// The first offset is zero and the last is the entries' length.
  fn check_ends(&self) -> Result<(), TableRefused> {
    if self.offset(0) != Some(0) {
      return Err(TableRefused::Malformed { row: 0 });
    }
    let last = self.offset(self.rows).ok_or(TableRefused::Truncated)?;
    let entries = u64::try_from(self.entries.len()).map_err(|_| TableRefused::TooLarge)?;
    match last.cmp(&entries) {
      Ordering::Less => Err(TableRefused::TrailingBytes),
      Ordering::Greater => Err(TableRefused::Truncated),
      Ordering::Equal => Ok(()),
    }
  }

  /// Offset `at`.
  fn offset(&self, at: usize) -> Option<u64> {
    word(self.offsets, at.checked_mul(WORD_BYTES)?)
  }

  /// Entry `at`'s bytes, or `None` when its offsets do not bound a range of the entries.
  fn entry(&self, at: usize) -> Option<&'a [u8]> {
    let start = usize::try_from(self.offset(at)?).ok()?;
    let end = usize::try_from(self.offset(at.checked_add(1)?)?).ok()?;
    self.entries.get(start..end)
  }

  /// An entry's row, or `None` when its key length or its body's count passes its end.
  fn split(entry: &'a [u8]) -> Option<RowRef<'a>> {
    let length = entry.get(..KEY_LENGTH_BYTES)?;
    let key_len = usize::try_from(u32::from_le_bytes(
      <[u8; KEY_LENGTH_BYTES]>::try_from(length).ok()?,
    ))
    .ok()?;
    let key_end = KEY_LENGTH_BYTES.checked_add(key_len)?;
    let key = entry.get(KEY_LENGTH_BYTES..key_end)?;
    let body = entry.get(key_end..)?;
    RowRef::new(key, body)
  }

  /// The rows.
  pub fn rows(&self) -> usize {
    self.rows
  }

  /// Row `at`, counted from zero, or `None` past the last row or where the bytes are not a row.
  pub fn row(&self, at: usize) -> Option<RowRef<'a>> {
    if at >= self.rows {
      return None;
    }
    TableView::split(self.entry(at)?)
  }

  /// The row whose key is `key`, by binary search.
  pub fn get(&self, key: &[u8]) -> Option<RowRef<'a>> {
    let (mut low, mut high) = (0, self.rows);
    while low < high {
      let middle = low.midpoint(high);
      let row = self.row(middle)?;
      match row.key().cmp(key) {
        Ordering::Equal => return Some(row),
        Ordering::Less => low = middle.checked_add(1)?,
        Ordering::Greater => high = middle,
      }
    }
    None
  }

  /// Every row, in key order.
  pub fn iter(&self) -> impl Iterator<Item = RowRef<'a>> + use<'a> {
    let view = *self;
    (0..self.rows).map_while(move |at| view.row(at))
  }
}

/// Writes a table a row at a time, in key order (module docs).
#[derive(Debug, Default)]
pub struct TableBuilder {
  offsets: Vec<u8>,
  entries: Vec<u8>,
  rows: u64,
  /// Where the last key sits in `entries`, to hold the next key after it without a copy of each.
  last_key: Option<(usize, usize)>,
}

impl TableBuilder {
  /// An empty table.
  pub fn new() -> TableBuilder {
    TableBuilder::default()
  }

  /// Appends the row of `key` with `body`; refused, with nothing written, unless the key is after the last one and
  /// the body is one.
  pub fn push(&mut self, key: &[u8], body: &[u8]) -> Result<(), TableRefused> {
    let row = self.rows;
    let last = self
      .last_key
      .and_then(|(start, end)| self.entries.get(start..end));
    if last.is_some_and(|last| last >= key) {
      return Err(TableRefused::Unsorted { row });
    }
    if RowRef::new(key, body).is_none() {
      return Err(TableRefused::Malformed { row });
    }
    let key_len = u32::try_from(key.len()).map_err(|_| TableRefused::TooLarge)?;
    let start = u64::try_from(self.entries.len()).map_err(|_| TableRefused::TooLarge)?;
    let rows = row.checked_add(1).ok_or(TableRefused::TooLarge)?;
    self.offsets.extend_from_slice(&start.to_le_bytes());
    self.entries.extend_from_slice(&key_len.to_le_bytes());
    let key_start = self.entries.len();
    self.entries.extend_from_slice(key);
    self.last_key = Some((key_start, self.entries.len()));
    self.entries.extend_from_slice(body);
    self.rows = rows;
    Ok(())
  }

  /// The rows written so far.
  pub fn rows(&self) -> u64 {
    self.rows
  }

  /// The bytes the table would take finished now: [`TABLE_FRAME_BYTES`] and each row's [`row_bytes`].
  pub fn len(&self) -> usize {
    TABLE_FRAME_BYTES
      .saturating_add(self.offsets.len())
      .saturating_add(self.entries.len())
  }

  /// Whether no row is written yet.
  pub fn is_empty(&self) -> bool {
    self.rows == 0
  }

  /// The table's bytes.
  pub fn finish(self) -> Result<Vec<u8>, TableRefused> {
    let end = u64::try_from(self.entries.len()).map_err(|_| TableRefused::TooLarge)?;
    let len = HEADER_BYTES
      .checked_add(self.offsets.len())
      .and_then(|len| len.checked_add(WORD_BYTES))
      .and_then(|len| len.checked_add(self.entries.len()))
      .ok_or(TableRefused::TooLarge)?;
    let mut bytes = Vec::with_capacity(len);
    bytes.extend_from_slice(&TABLE_MAGIC);
    bytes.extend_from_slice(&self.rows.to_le_bytes());
    bytes.extend_from_slice(&self.offsets);
    bytes.extend_from_slice(&end.to_le_bytes());
    bytes.extend_from_slice(&self.entries);
    Ok(bytes)
  }
}

#[cfg(test)]
mod tests {
  use std::collections::BTreeMap;

  use proptest::prelude::*;

  use super::*;

  fn body(contents: &[ContentId], payload: &[u8]) -> Vec<u8> {
    encode_body(contents, payload).unwrap()
  }

  fn table(rows: &BTreeMap<Vec<u8>, Vec<u8>>) -> Vec<u8> {
    let mut builder = TableBuilder::new();
    for (key, body) in rows {
      builder.push(key, body).unwrap();
    }
    builder.finish().unwrap()
  }

  fn three_rows() -> Vec<u8> {
    let mut rows = BTreeMap::new();
    rows.insert(b"a".to_vec(), body(&[ContentId::of(b"x")], b"p"));
    rows.insert(b"bb".to_vec(), body(&[], b""));
    rows.insert(
      b"c".to_vec(),
      body(&[ContentId::of(b"y"), ContentId::of(b"z")], b"qq"),
    );
    table(&rows)
  }

  /// Sets the word at `at`.
  fn set_word(bytes: &mut [u8], at: usize, value: u64) {
    bytes[at..at + 8].copy_from_slice(&value.to_le_bytes());
  }

  /// A-581. Do: open a table of three rows and read it. Expect: three rows, a row's payload and contents read in place,
  /// and an absent key not found.
  #[test]
  fn a_table_reads_its_rows_in_place() {
    let bytes = three_rows();
    let view = TableView::open(&bytes).unwrap();
    assert_eq!(view.rows(), 3);
    let row = view.get(b"c").unwrap();
    assert_eq!(row.payload(), b"qq");
    assert_eq!(
      row.contents().collect::<Vec<_>>(),
      vec![ContentId::of(b"y"), ContentId::of(b"z")]
    );
    assert_eq!(view.get(b"b"), None);
  }

  /// `bytes` with the word at `at` set to `value`, opened.
  fn opened_with_word(bytes: &[u8], at: usize, value: u64) -> Result<(), TableRefused> {
    let mut changed = bytes.to_vec();
    set_word(&mut changed, at, value);
    TableView::open(&changed).map(drop)
  }

  /// A-581, §4.9. Do: break a table's frame every way: too short for a header, a foreign magic, a row count of
  /// `u64::MAX`, a row count one past its offsets, a first offset not zero, a last offset short of and past the entries,
  /// and a byte after them. Expect: each refused by its kind before a row is read.
  #[test]
  fn a_tables_frame_is_checked_before_a_row_is_read() {
    let bytes = three_rows();
    assert_eq!(
      TableView::open(&bytes[..10]).unwrap_err(),
      TableRefused::Truncated
    );
    let mut foreign = bytes.clone();
    foreign[0] ^= 1;
    assert_eq!(
      TableView::open(&foreign).unwrap_err(),
      TableRefused::ForeignMagic
    );
    assert_eq!(
      opened_with_word(&bytes, 8, u64::MAX),
      Err(TableRefused::Truncated)
    );
    assert!(opened_with_word(&bytes, 8, 4).is_err());
    assert_eq!(
      opened_with_word(&bytes, HEADER_BYTES, 1),
      Err(TableRefused::Malformed { row: 0 })
    );
    let last_at = HEADER_BYTES + 3 * WORD_BYTES;
    let entries = u64::try_from(bytes.len() - last_at - WORD_BYTES).unwrap();
    assert_eq!(
      opened_with_word(&bytes, last_at, entries - 1),
      Err(TableRefused::TrailingBytes)
    );
    assert_eq!(
      opened_with_word(&bytes, last_at, entries + 1),
      Err(TableRefused::Truncated)
    );
    let mut trailing = bytes.clone();
    trailing.push(0);
    assert_eq!(
      TableView::open(&trailing).unwrap_err(),
      TableRefused::TrailingBytes
    );
  }

  /// A-581, §4.9. Do: break one entry at a time: offsets that go backwards, a key length past its entry, a body's count
  /// past its end, two rows out of order, two rows with one key. Expect: each refused naming its row.
  #[test]
  fn a_tables_entries_and_their_order_are_checked() {
    let bytes = three_rows();
    let second = HEADER_BYTES + WORD_BYTES;
    assert!(matches!(
      opened_with_word(&bytes, second, 1_000),
      Err(TableRefused::Malformed { .. })
    ));
    let entries_at = HEADER_BYTES + 4 * WORD_BYTES;
    let mut key_past = bytes.clone();
    key_past[entries_at..entries_at + 4].copy_from_slice(&u32::MAX.to_le_bytes());
    assert_eq!(
      TableView::open(&key_past).unwrap_err(),
      TableRefused::Malformed { row: 0 }
    );
    let mut count_past = bytes.clone();
    count_past[entries_at + 4 + 1] = u8::MAX;
    assert_eq!(
      TableView::open(&count_past).unwrap_err(),
      TableRefused::Malformed { row: 0 }
    );
    let mut swapped = bytes.clone();
    swapped[entries_at + 4] = b'd';
    assert_eq!(
      TableView::open(&swapped).unwrap_err(),
      TableRefused::Unsorted { row: 1 }
    );
    let mut repeated = bytes.clone();
    let third_key = entries_at + (4 + 1 + 1 + 32 + 1) + (4 + 2 + 1) + 4;
    assert_eq!(repeated[third_key], b'c');
    repeated[third_key] = b'a';
    assert_eq!(
      TableView::open(&repeated).unwrap_err(),
      TableRefused::Unsorted { row: 2 }
    );
  }

  /// A-581. Do: push rows out of order, a repeated key and a body that is none into a builder, and encode a body naming
  /// more contents than its count can say. Expect: each refused naming its row, the rows before it kept; the body refused
  /// by kind.
  #[test]
  fn a_builder_refuses_a_row_out_of_order_or_malformed() {
    let mut builder = TableBuilder::new();
    builder.push(b"m", &body(&[], b"")).unwrap();
    assert_eq!(
      builder.push(b"m", &body(&[], b"")),
      Err(TableRefused::Unsorted { row: 1 })
    );
    assert_eq!(
      builder.push(b"a", &body(&[], b"")),
      Err(TableRefused::Unsorted { row: 1 })
    );
    assert_eq!(
      builder.push(b"z", &[3]),
      Err(TableRefused::Malformed { row: 1 })
    );
    assert_eq!((builder.rows(), builder.is_empty()), (1, false));
    assert_eq!(
      encode_body(&vec![ContentId::of(b""); 256], b""),
      Err(TableRefused::TooManyContents)
    );
  }

  fn rows() -> impl Strategy<Value = BTreeMap<Vec<u8>, Vec<u8>>> {
    proptest::collection::btree_map(
      proptest::collection::vec(any::<u8>(), 0..6),
      (0usize..3, proptest::collection::vec(any::<u8>(), 0..5)).prop_map(|(count, payload)| {
        let contents: Vec<ContentId> = (0..count)
          .map(|at| ContentId::of(&[u8::try_from(at).unwrap()]))
          .collect();
        encode_body(&contents, &payload).unwrap()
      }),
      0..40,
    )
  }

  proptest! {
    #![proptest_config(slates_test_seeds::unseeded(ProptestConfig::default()))]

    /// A-581. Do: write generated rows as a table and open it. Expect: every row read back in order, every key found
    /// by its search and every absent key not, as the ordered map they came from; its length the frame's and each row's
    /// (`row_bytes`), the measure the index cuts tables by.
    #[test]
    fn a_table_reads_back_as_the_ordered_map_it_was_written_from(rows in rows(), probes in proptest::collection::vec(proptest::collection::vec(any::<u8>(), 0..6), 0..20)) {
      let bytes = table(&rows);
      let view = TableView::open(&bytes).unwrap();
      let read: BTreeMap<Vec<u8>, Vec<u8>> = view.iter().map(|row| (row.key().to_vec(), row.body().to_vec())).collect();
      prop_assert_eq!(&read, &rows);
      prop_assert_eq!(view.rows(), rows.len());
      let counted = rows.iter().map(|(key, body)| row_bytes(key, body).unwrap()).sum::<usize>();
      prop_assert_eq!(bytes.len(), TABLE_FRAME_BYTES + counted);
      for probe in probes.iter().chain(rows.keys()) {
        prop_assert_eq!(view.get(probe).map(|row| row.body().to_vec()), rows.get(probe).cloned());
      }
    }

    /// A-581, §4.9. Do: change one byte of a valid table anywhere, or cut it anywhere. Expect: never a panic; either
    /// refused by kind, or opened as a table whose every row reads and whose keys strictly increase.
    #[test]
    fn a_changed_or_cut_table_is_refused_or_reads_whole(rows in rows(), at in any::<usize>(), flip in 1u8..=255, cut in any::<usize>()) {
      let bytes = table(&rows);
      let mut changed = bytes.clone();
      let at = at % changed.len();
      changed[at] ^= flip;
      let cut = &bytes[..cut % (bytes.len() + 1)];
      for candidate in [&changed[..], cut] {
        if let Ok(view) = TableView::open(candidate) {
          let keys: Vec<&[u8]> = view.iter().map(|row| row.key()).collect();
          prop_assert_eq!(keys.len(), view.rows());
          prop_assert!(keys.windows(2).all(|pair| pair[0] < pair[1]));
        }
      }
    }
  }
}
