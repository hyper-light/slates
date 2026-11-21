//! Wide-column types: RocksDB's `include/rocksdb/wide_columns.h`, `db/wide/wide_columns.cc` and
//! the write-path `AttributeGroup` of `include/rocksdb/attribute_groups.h`.
//!
//! RocksDB's `WideColumn` is two `Slice`s that do not own their bytes; here it is two borrowed
//! slices. `PinnableWideColumns` owns the buffers its columns point into: where RocksDB keeps a
//! `std::forward_list<PinnableSlice>` whose nodes never move, the port keeps boxed buffers,
//! which do not move when the owner does, and columns that name a buffer and a range in it.

use crate::db::blob::blob_index::BlobIndex;
use crate::db::wide::wide_column_serialization::{parse, serialized_size_v1};
use crate::error::Error;

/// `kDefaultWideColumnName` [R db/wide/wide_columns.cc:15]: the default column's name is empty.
pub const DEFAULT_WIDE_COLUMN_NAME: &[u8] = b"";

/// `WideColumn` [R include/rocksdb/wide_columns.h:28-110]: a column name and value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct WideColumn<'a> {
    pub name: &'a [u8],
    pub value: &'a [u8],
}

impl<'a> WideColumn<'a> {
    /// A column over `name` and `value`, which may be anything that is bytes (`&str`,
    /// `String`, `Vec<u8>`, `[u8]`), borrowed without copying, as RocksDB's forwarding
    /// constructor makes its `Slice`s.
    pub fn new<N, V>(name: &'a N, value: &'a V) -> Self
    where
        N: AsRef<[u8]> + ?Sized,
        V: AsRef<[u8]> + ?Sized,
    {
        Self {
            name: name.as_ref(),
            value: value.as_ref(),
        }
    }

    pub const fn name(&self) -> &'a [u8] {
        self.name
    }

    pub const fn value(&self) -> &'a [u8] {
        self.value
    }
}

/// `WideColumns` [R include/rocksdb/wide_columns.h:136].
pub type WideColumns<'a> = Vec<WideColumn<'a>>;

/// `AttributeGroup` [R include/rocksdb/attribute_groups.h:18-41]: the columns of one entity
/// that go to one column family. RocksDB names the column family by its handle; the batch
/// records only its id, so the port takes the id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttributeGroup<'a> {
    pub column_family: u32,
    pub columns: WideColumns<'a>,
}

impl<'a> AttributeGroup<'a> {
    pub const fn new(column_family: u32, columns: WideColumns<'a>) -> Self {
        Self {
            column_family,
            columns,
        }
    }
}

/// A range of one of a `PinnableWideColumns`'s buffers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Span {
    buffer: usize,
    start: usize,
    len: usize,
}

/// Where a resolved column's name or value lies: in a buffer the columns already hold, or in
/// one of the buffers handed over with the resolution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolvedBytes {
    /// A span the columns already hold (from `column_spans`).
    Held(Span),
    /// `len` bytes at `start` of extra buffer `buffer`.
    Extra {
        buffer: usize,
        start: usize,
        len: usize,
    },
}

/// `PinnableWideColumns` [R include/rocksdb/wide_columns.h:142-331; db/wide/wide_columns.cc:19-45]:
/// the columns of a read result, with the buffers they live in.
#[derive(Debug, Default)]
pub struct PinnableWideColumns {
    backing: Vec<Box<[u8]>>,
    columns: Vec<(Span, Span)>,
    unresolved_blob_column_indices: Vec<usize>,
}

impl PartialEq for PinnableWideColumns {
    fn eq(&self, other: &Self) -> bool {
        self.columns() == other.columns()
    }
}

impl PinnableWideColumns {
    pub fn new() -> Self {
        Self::default()
    }

    fn bytes(&self, span: Span) -> &[u8] {
        self.backing
            .get(span.buffer)
            .and_then(|b| b.get(span.start..span.start.saturating_add(span.len)))
            .unwrap_or(&[])
    }

    /// `columns()`: the columns, borrowed from the buffers held here.
    pub fn columns(&self) -> WideColumns<'_> {
        self.columns
            .iter()
            .map(|&(name, value)| WideColumn {
                name: self.bytes(name),
                value: self.bytes(value),
            })
            .collect()
    }

    /// The buffer ranges of each column's name and value, for building a resolution.
    pub fn column_spans(&self) -> &[(Span, Span)] {
        &self.columns
    }

    /// `payload_size()`: the sum of the names' and values' sizes. A sum of the lengths of
    /// buffers in memory cannot pass `usize`, so the saturation never takes effect.
    pub fn payload_size(&self) -> usize {
        self.columns.iter().fold(0usize, |acc, (n, v)| {
            acc.saturating_add(n.len).saturating_add(v.len)
        })
    }

    /// `serialized_size()`: the length of these columns in the version-1 format.
    pub fn serialized_size(&self) -> usize {
        serialized_size_v1(&self.columns())
    }

    /// `SetPlainValue`: one default column holding `value`, which is moved in when owned and
    /// copied when borrowed.
    pub fn set_plain_value(&mut self, value: impl Into<Vec<u8>>) {
        let value: Box<[u8]> = value.into().into_boxed_slice();
        let len = value.len();
        self.backing.clear();
        self.backing.push(value);
        self.columns.clear();
        self.columns.push((
            Span::default(),
            Span {
                buffer: 0,
                start: 0,
                len,
            },
        ));
        self.unresolved_blob_column_indices.clear();
    }

    /// `SetWideColumnValue`: the columns of a serialized entity, which is moved in when owned.
    /// A blob column's value stays its serialized blob index, and its index is recorded as
    /// unresolved. On an error the columns are reset, as RocksDB resets them.
    pub fn set_wide_column_value(&mut self, value: impl Into<Vec<u8>>) -> Result<(), Error> {
        self.reset();
        let value: Box<[u8]> = value.into().into_boxed_slice();
        let (ranges, blobs) = parse(&value)?;
        // `Deserialize` with blob columns requested decodes each blob index.
        for &i in &blobs {
            let blob_value = ranges.get(i).and_then(|(_, v)| value.get(v.clone()));
            match blob_value {
                Some(bytes) if !bytes.is_empty() => {
                    BlobIndex::decode_from(bytes).map_err(|e| match e {
                        Error::Corruption { why, .. } => {
                            Error::corruption("blob index in wide column", why)
                        }
                        other => other,
                    })?;
                }
                _ => return Err(Error::truncated("blob index in wide column")),
            }
        }
        let span = |r: &std::ops::Range<usize>| Span {
            buffer: 0,
            start: r.start,
            len: r.end.saturating_sub(r.start),
        };
        self.columns = ranges.iter().map(|(n, v)| (span(n), span(v))).collect();
        self.unresolved_blob_column_indices = blobs;
        self.backing.push(value);
        Ok(())
    }

    /// `Reset`.
    pub fn reset(&mut self) {
        self.backing.clear();
        self.columns.clear();
        self.unresolved_blob_column_indices.clear();
    }

    /// `PinnableWideColumnsHelper::ResolveColumns` [R db/wide/wide_columns_helper.h:86-93]:
    /// replaces the columns with a resolved set whose bytes lie in buffers already held or in
    /// `extra_buffers`, which the columns take over. Nothing is copied. A range outside its
    /// buffer is refused and leaves the columns as they were.
    pub fn resolve_columns(
        &mut self,
        resolved: &[(ResolvedBytes, ResolvedBytes)],
        extra_buffers: Vec<Box<[u8]>>,
    ) -> Result<(), Error> {
        let held = self.backing.len();
        let fits = |len_of: &dyn Fn(usize) -> Option<usize>, buffer, start: usize, len| {
            start
                .checked_add(len)
                .zip(len_of(buffer))
                .is_some_and(|(end, have)| end <= have)
        };
        let held_len = |i: usize| self.backing.get(i).map(|b| b.len());
        let extra_len = |i: usize| extra_buffers.get(i).map(|b| b.len());
        let map = |r: ResolvedBytes| -> Option<Span> {
            match r {
                ResolvedBytes::Held(s) => {
                    if fits(&held_len, s.buffer, s.start, s.len) {
                        Some(s)
                    } else {
                        None
                    }
                }
                ResolvedBytes::Extra { buffer, start, len } => fits(&extra_len, buffer, start, len)
                    .then(|| Span {
                        buffer: held.saturating_add(buffer),
                        start,
                        len,
                    }),
            }
        };
        let columns = resolved
            .iter()
            .map(|&(n, v)| map(n).zip(map(v)))
            .collect::<Option<Vec<_>>>()
            .ok_or(Error::InvalidArgument {
                what: "resolved wide column outside its buffer",
            })?;
        self.backing.extend(extra_buffers);
        self.columns = columns;
        self.unresolved_blob_column_indices.clear();
        Ok(())
    }

    /// `PinnableWideColumnsHelper::GetUnresolvedBlobColumnIndices`: the columns whose values
    /// are still encoded blob indexes.
    pub fn unresolved_blob_column_indices(&self) -> &[usize] {
        &self.unresolved_blob_column_indices
    }
}
