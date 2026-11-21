//! `WideColumnsHelper`: RocksDB's `db/wide/wide_columns_helper.{h,cc}` and the
//! `operator<<(WideColumn)` of `include/rocksdb/wide_columns.h`, which the dump tools print.
//! `PinnableWideColumnsHelper` is on `PinnableWideColumns` itself.

use std::fmt::Write as _;

use crate::db::wide::wide_column_serialization::deserialize_simple;
use crate::db::wide::wide_columns::{DEFAULT_WIDE_COLUMN_NAME, WideColumn};
use crate::error::Error;

/// `Slice::ToString(hex)` [R util/slice.cc:282-296]: two uppercase hex digits per byte, or the
/// bytes themselves (lossily, where they are not UTF-8).
fn slice_to_string(out: &mut String, bytes: &[u8], hex: bool) {
    if hex {
        for b in bytes {
            // Writing to a String does not fail.
            let _ = write!(out, "{b:02X}");
        }
    } else {
        out.push_str(&String::from_utf8_lossy(bytes));
    }
}

/// `operator<<(std::ostream&, const WideColumn&)` [R include/rocksdb/wide_columns.h:101-117]:
/// `name:value`, each part with a `0x` prefix in hex and left out when empty.
pub fn dump_wide_column(out: &mut String, column: &WideColumn<'_>, hex: bool) {
    for (i, part) in [column.name, column.value].into_iter().enumerate() {
        if i == 1 {
            out.push(':');
        }
        if !part.is_empty() {
            if hex {
                out.push_str("0x");
            }
            slice_to_string(out, part, hex);
        }
    }
}

/// `DumpWideColumns` [R db/wide/wide_columns_helper.cc:12-30]: the columns separated by spaces.
pub fn dump_wide_columns(columns: &[WideColumn<'_>], out: &mut String, hex: bool) {
    for (i, column) in columns.iter().enumerate() {
        if i > 0 {
            out.push(' ');
        }
        dump_wide_column(out, column, hex);
    }
}

/// `DumpSliceAsWideColumns` [R db/wide/wide_columns_helper.cc:32-43]: a serialized entity's
/// columns, dumped when it decodes.
pub fn dump_slice_as_wide_columns(value: &[u8], out: &mut String, hex: bool) -> Result<(), Error> {
    let columns = deserialize_simple(value)?;
    dump_wide_columns(&columns, out, hex);
    Ok(())
}

/// `HasDefaultColumn`: the first column has the default (empty) name.
pub fn has_default_column(columns: &[WideColumn<'_>]) -> bool {
    columns
        .first()
        .is_some_and(|c| c.name == DEFAULT_WIDE_COLUMN_NAME)
}

/// `HasDefaultColumnOnly`.
pub fn has_default_column_only(columns: &[WideColumn<'_>]) -> bool {
    columns.len() == 1 && has_default_column(columns)
}

/// `GetDefaultColumn`: the default column's value, `None` when there is no default column
/// (RocksDB asserts there is one).
pub fn get_default_column<'a>(columns: &[WideColumn<'a>]) -> Option<&'a [u8]> {
    columns
        .first()
        .filter(|c| c.name == DEFAULT_WIDE_COLUMN_NAME)
        .map(|c| c.value)
}

/// `SortColumns`: bytewise by name.
pub fn sort_columns(columns: &mut [WideColumn<'_>]) {
    columns.sort_by(|a, b| a.name.cmp(b.name));
}

/// `Find`: the index of the column named `column_name` in columns sorted by name.
pub fn find(columns: &[WideColumn<'_>], column_name: &[u8]) -> Option<usize> {
    columns.binary_search_by(|c| c.name.cmp(column_name)).ok()
}
