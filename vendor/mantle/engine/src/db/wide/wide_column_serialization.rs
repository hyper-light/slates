//! Wide-column entity serialization: RocksDB's `db/wide/wide_column_serialization.{h,cc}`
//! (docs/research/24 §1.17).
//!
//! Version 1 [R db/wide/wide_column_serialization.h:26-47]:
//! `varint32 1 ‖ varint32 N ‖ {varint32 name_size ‖ name ‖ varint32 value_size}×N ‖ values`.
//!
//! Version 2 [R db/wide/wide_column_serialization.h:49-114]:
//! `varint32 2 ‖ varint32 N ‖ varint32 name_sizes_bytes ‖ varint32 value_sizes_bytes ‖
//! varint32 names_bytes ‖ u8 type×N ‖ varint32 name_size×N ‖ varint32 value_size×N ‖ names ‖
//! values`, where a type is `kTypeValue` (0x01, inline) or `kTypeBlobIndex` (0x11, the value is
//! a serialized [`BlobIndex`]).
//!
//! Column names are strictly ascending bytewise; an entity is the whole stored value, so bytes
//! after it are corruption; a version above 2 is corruption, and a version below 2 (including 0)
//! parses as version 1 [R db/wide/wide_column_serialization.cc:470-490]. RocksDB's error
//! messages become the `what` of [`Error::Corruption`], so a caller (and the ported tests) can
//! tell which field failed.
//!
//! A blob column is resolved through a [`BlobFetcher`], which the blob-file reader implements
//! (P13); with none, a non-inlined blob reference is corruption, as RocksDB reports it.

use std::borrow::Cow;
use std::ops::Range;

use crate::db::blob::blob_index::BlobIndex;
use crate::db::dbformat::ValueType;
use crate::db::wide::wide_columns::{DEFAULT_WIDE_COLUMN_NAME, WideColumn, WideColumns};
use crate::error::{Error, Malformed};
use crate::util::coding::{
    get_length_prefixed_slice, get_varint32, put_length_prefixed_slice, put_varint32, varint_length,
};

/// `kVersion1` [R db/wide/wide_column_serialization.h:124]: inline values only.
pub const VERSION1: u32 = 1;
/// `kVersion2` [R db/wide/wide_column_serialization.h:125]: columns may be blob references.
pub const VERSION2: u32 = 2;

/// A column's name and value as ranges of the serialized entity.
pub(crate) type ColumnRanges = (Range<usize>, Range<usize>);

/// Reads a blob a blob column or a blob-backed default column refers to: RocksDB's
/// `BlobFetcher`, implemented by the blob-file reader (P13).
pub trait BlobFetcher {
    /// `FetchBlob`: the value `blob_index` refers to, for `user_key`.
    fn fetch_blob(&self, user_key: &[u8], blob_index: &BlobIndex<'_>) -> Result<Vec<u8>, Error>;
}

/// `ValidateWideColumnLimit` [R db/wide/wide_column_serialization.h:260-265]: a size the format
/// stores in a varint32.
fn limit_u32(size: usize, what: &'static str) -> Result<u32, Error> {
    u32::try_from(size).map_err(|_| Error::InvalidArgument { what })
}

/// `ValidateColumnOrder` [R db/wide/wide_column_serialization.h:267-273]: names strictly
/// ascending bytewise, so a duplicate is out of order too.
fn check_order(prev: Option<&[u8]>, name: &[u8]) -> Result<(), Error> {
    match prev {
        Some(p) if p >= name => Err(Error::corruption("wide columns", Malformed::OutOfOrder)),
        _ => Ok(()),
    }
}

/// Replaces the `what` of a decode error with the field RocksDB names.
fn at(what: &'static str) -> impl Fn(Error) -> Error {
    move |e| match e {
        Error::Corruption { why, .. } => Error::corruption(what, why),
        other => other,
    }
}

/// `Serialize` [R db/wide/wide_column_serialization.cc:54-107]: appends `columns` in version 1.
/// The columns are checked before anything is written, so a refused entity leaves `output` as
/// it was.
pub fn serialize(columns: &[WideColumn<'_>], output: &mut Vec<u8>) -> Result<(), Error> {
    let num_columns = limit_u32(columns.len(), "Too many wide columns")?;
    let mut prev: Option<&[u8]> = None;
    for column in columns {
        limit_u32(column.name.len(), "Wide column name too long")?;
        check_order(prev, column.name)?;
        limit_u32(column.value.len(), "Wide column value too long")?;
        prev = Some(column.name);
    }
    put_varint32(output, VERSION1);
    put_varint32(output, num_columns);
    for column in columns {
        put_length_prefixed_slice(output, column.name)?;
        put_varint32(
            output,
            limit_u32(column.value.len(), "Wide column value too long")?,
        );
    }
    for column in columns {
        output.extend_from_slice(column.value);
    }
    Ok(())
}

/// `SerializedSizeV1` [R db/wide/wide_column_serialization.cc:109-121]: the length `serialize`
/// writes. The sum of lengths of slices in memory cannot pass `usize`; it saturates rather than
/// wraps if it could.
pub fn serialized_size_v1(columns: &[WideColumn<'_>]) -> usize {
    let len = |n: usize| varint_length(u64::try_from(n).unwrap_or(u64::MAX));
    columns.iter().fold(
        varint_length(u64::from(VERSION1)).saturating_add(len(columns.len())),
        |size, c| {
            size.saturating_add(len(c.name.len()))
                .saturating_add(c.name.len())
                .saturating_add(len(c.value.len()))
                .saturating_add(c.value.len())
        },
    )
}

/// `SerializeV2` [R db/wide/wide_column_serialization.cc:123-272]: appends `columns` in version
/// 2, the columns named in `blob_columns` (by index) stored as those blob indexes instead of
/// their values.
pub fn serialize_v2(
    columns: &[WideColumn<'_>],
    blob_columns: &[(usize, BlobIndex<'_>)],
    output: &mut Vec<u8>,
) -> Result<(), Error> {
    let num_columns = limit_u32(columns.len(), "Too many wide columns")?;
    // `BuildBlobIndexMap`: a later entry for the same column replaces an earlier one.
    let mut blob_map: Vec<Option<&BlobIndex<'_>>> = vec![None; columns.len()];
    for (index, blob_index) in blob_columns {
        let slot = blob_map.get_mut(*index).ok_or(Error::InvalidArgument {
            what: "Blob column index out of range",
        })?;
        *slot = Some(blob_index);
    }

    let mut prev: Option<&[u8]> = None;
    let mut name_sizes = Vec::new();
    let mut value_sizes = Vec::new();
    let mut types = Vec::new();
    let mut encoded_blobs: Vec<Vec<u8>> = Vec::new();
    let (mut name_sizes_bytes, mut names_bytes) = (0u64, 0u64);
    let (mut value_sizes_bytes, mut values_bytes) = (0u64, 0u64);
    for (column, blob) in columns.iter().zip(&blob_map) {
        let name_size = limit_u32(column.name.len(), "Wide column name too long")?;
        check_order(prev, column.name)?;
        let value_size = match blob {
            Some(blob_index) => {
                let mut encoded = Vec::new();
                blob_index.encode_to(&mut encoded);
                let size = limit_u32(encoded.len(), "Wide column value too long")?;
                encoded_blobs.push(encoded);
                types.push(ValueType::BlobIndex.as_u8());
                size
            }
            None => {
                types.push(ValueType::Value.as_u8());
                limit_u32(column.value.len(), "Wide column value too long")?
            }
        };
        name_sizes.push(name_size);
        value_sizes.push(value_size);
        // At most 2^32 columns of at most 2^32 − 1 bytes and 5-byte sizes each: every sum is
        // below 2^64, so the saturation never takes effect.
        name_sizes_bytes = name_sizes_bytes.saturating_add(varint_len(name_size));
        names_bytes = names_bytes.saturating_add(name_size.into());
        value_sizes_bytes = value_sizes_bytes.saturating_add(varint_len(value_size));
        values_bytes = values_bytes.saturating_add(value_size.into());
        prev = Some(column.name);
    }
    let too_large = |v: u64, what| u32::try_from(v).map_err(|_| Error::InvalidArgument { what });
    let name_sizes_bytes = too_large(name_sizes_bytes, "Wide column metadata too large")?;
    let value_sizes_bytes = too_large(value_sizes_bytes, "Wide column metadata too large")?;
    let names_bytes = too_large(names_bytes, "Wide column names too large")?;
    too_large(values_bytes, "Wide column values too large")?;

    // Sections 1–3: header, skip info, column types.
    put_varint32(output, VERSION2);
    put_varint32(output, num_columns);
    put_varint32(output, name_sizes_bytes);
    put_varint32(output, value_sizes_bytes);
    put_varint32(output, names_bytes);
    output.extend_from_slice(&types);
    // Sections 4–7: name sizes, value sizes, names, values.
    for &size in &name_sizes {
        put_varint32(output, size);
    }
    for &size in &value_sizes {
        put_varint32(output, size);
    }
    for column in columns {
        output.extend_from_slice(column.name);
    }
    let mut blobs = encoded_blobs.iter();
    for (column, blob) in columns.iter().zip(&blob_map) {
        match blob {
            Some(_) => output.extend_from_slice(blobs.next().map_or(&[][..], Vec::as_slice)),
            None => output.extend_from_slice(column.value),
        }
    }
    Ok(())
}

/// The length of a varint32, as a u64 (at most 5).
fn varint_len(v: u32) -> u64 {
    u64::try_from(varint_length(v.into())).unwrap_or(u64::MAX)
}

/// The offset in `input` of the start of `rest`, a suffix of it.
fn offset(input: &[u8], rest: &[u8]) -> usize {
    input.len().saturating_sub(rest.len())
}

/// The version and column count every entry point reads first.
fn read_header(rest: &mut &[u8]) -> Result<(u32, u32), Error> {
    let version = get_varint32(rest).map_err(at("wide column version"))?;
    // A too-new version is corruption, never "not supported" [R :470-477].
    if version > VERSION2 {
        return Err(Error::corruption(
            "wide column version",
            Malformed::UnknownVersion(version.into()),
        ));
    }
    let num_columns = get_varint32(rest).map_err(at("number of wide columns"))?;
    Ok((version, num_columns))
}

/// `IsValidColumnValueType` [R db/wide/wide_column_serialization.h:299-301]: inline or blob
/// index; an entity type is refused, so entities do not nest.
fn check_column_type(t: u8) -> Result<bool, Error> {
    if t == ValueType::Value.as_u8() {
        Ok(false)
    } else if t == ValueType::BlobIndex.as_u8() {
        Ok(true)
    } else {
        Err(Error::corruption(
            "wide column ValueType",
            Malformed::UnknownTag(t),
        ))
    }
}

/// `DeserializeV1` [R db/wide/wide_column_serialization.cc:274-325].
fn parse_v1(input: &[u8], mut rest: &[u8], num_columns: u32) -> Result<Vec<ColumnRanges>, Error> {
    let mut names: Vec<Range<usize>> = Vec::new();
    let mut value_sizes = Vec::new();
    let mut prev: Option<&[u8]> = None;
    for _ in 0..num_columns {
        let name = get_length_prefixed_slice(&mut rest).map_err(at("wide column name"))?;
        check_order(prev, name)?;
        prev = Some(name);
        let end = offset(input, rest);
        names.push(end.saturating_sub(name.len())..end);
        value_sizes.push(get_varint32(&mut rest).map_err(at("wide column value size"))?);
    }
    let mut pos = offset(input, rest);
    let mut columns = Vec::new();
    for (name, size) in names.into_iter().zip(value_sizes) {
        let end = usize::try_from(size)
            .ok()
            .and_then(|s| pos.checked_add(s))
            .filter(|&end| end <= input.len())
            .ok_or(Error::truncated("wide column value payload"))?;
        columns.push((name, pos..end));
        pos = end;
    }
    // An entity is the whole value: trailing bytes are corruption [R :316-322].
    if pos != input.len() {
        return Err(Error::corruption(
            "wide column entity",
            Malformed::TrailingBytes,
        ));
    }
    Ok(columns)
}

/// Splits `n` bytes off the front of `rest`.
fn take<'a>(rest: &mut &'a [u8], n: usize, what: &'static str) -> Result<&'a [u8], Error> {
    let (head, tail) = rest.split_at_checked(n).ok_or(Error::truncated(what))?;
    *rest = tail;
    Ok(head)
}

/// The three skip-info varints of version 2.
fn read_skip_info(rest: &mut &[u8]) -> Result<(usize, usize, usize), Error> {
    let mut next = |what| {
        get_varint32(rest)
            .map_err(at(what))
            .and_then(|v| usize::try_from(v).map_err(|_| Error::truncated(what)))
    };
    let name_sizes = next("wide column name sizes bytes")?;
    let value_sizes = next("wide column value sizes bytes")?;
    let names = next("wide column names bytes")?;
    Ok((name_sizes, value_sizes, names))
}

/// `DeserializeV2Impl` [R db/wide/wide_column_serialization.cc:327-437]: the columns and whether
/// each is a blob reference.
fn parse_v2(
    input: &[u8],
    mut rest: &[u8],
    num_columns: u32,
) -> Result<(Vec<ColumnRanges>, Vec<bool>), Error> {
    let (name_sizes_bytes, value_sizes_bytes, names_bytes) = read_skip_info(&mut rest)?;
    let n = usize::try_from(num_columns).map_err(|_| Error::truncated("wide column types"))?;
    let types = take(&mut rest, n, "wide column types")?
        .iter()
        .map(|&t| check_column_type(t))
        .collect::<Result<Vec<bool>, Error>>()?;
    // Sections 4–6 must fit before the values [R :352-357].
    name_sizes_bytes
        .checked_add(value_sizes_bytes)
        .and_then(|s| s.checked_add(names_bytes))
        .filter(|&m| m <= rest.len())
        .ok_or(Error::truncated("wide column sections"))?;
    let mut s4 = take(&mut rest, name_sizes_bytes, "wide column sections")?;
    let mut s5 = take(&mut rest, value_sizes_bytes, "wide column sections")?;
    let s6_start = offset(input, rest);
    take(&mut rest, names_bytes, "wide column sections")?;
    let s7_start = offset(input, rest);
    let values_bytes = rest.len();

    let mut columns = Vec::new();
    let (mut name_pos, mut value_pos) = (0usize, 0usize);
    let mut prev: Option<Range<usize>> = None;
    for _ in 0..num_columns {
        let ns = get_varint32(&mut s4).map_err(at("wide column name size"))?;
        let vs = get_varint32(&mut s5).map_err(at("wide column value size"))?;
        let name_end = usize::try_from(ns)
            .ok()
            .and_then(|ns| name_pos.checked_add(ns))
            .filter(|&e| e <= names_bytes)
            .ok_or(Error::truncated("wide column name"))?;
        let name = s6_start.saturating_add(name_pos)..s6_start.saturating_add(name_end);
        check_order(
            prev.clone().and_then(|r| input.get(r)),
            input.get(name.clone()).unwrap_or(&[]),
        )?;
        let value_end = usize::try_from(vs)
            .ok()
            .and_then(|vs| value_pos.checked_add(vs))
            .filter(|&e| e <= values_bytes)
            .ok_or(Error::truncated("wide column value payload"))?;
        let value = s7_start.saturating_add(value_pos)..s7_start.saturating_add(value_end);
        prev = Some(name.clone());
        columns.push((name, value));
        name_pos = name_end;
        value_pos = value_end;
    }
    // The declared sections must be consumed exactly; the values run to the end of the input,
    // so this also refuses trailing bytes [R :428-434].
    if !s4.is_empty() || !s5.is_empty() || name_pos != names_bytes || value_pos != values_bytes {
        return Err(Error::corruption(
            "wide column sections",
            Malformed::CountMismatch,
        ));
    }
    Ok((columns, types))
}

/// The columns of an entity as ranges of it, and the indexes of its blob columns.
pub(crate) fn parse(input: &[u8]) -> Result<(Vec<ColumnRanges>, Vec<usize>), Error> {
    let mut rest = input;
    let (version, num_columns) = read_header(&mut rest)?;
    if version < VERSION2 {
        return Ok((parse_v1(input, rest, num_columns)?, Vec::new()));
    }
    let (columns, types) = parse_v2(input, rest, num_columns)?;
    let blobs = types
        .iter()
        .enumerate()
        .filter(|&(_, &is_blob)| is_blob)
        .map(|(i, _)| i)
        .collect();
    Ok((columns, blobs))
}

/// Decodes the blob index a blob column's value holds.
fn decode_blob_column(value: &[u8]) -> Result<BlobIndex<'_>, Error> {
    if value.is_empty() {
        return Err(Error::truncated("blob index in wide column"));
    }
    BlobIndex::decode_from(value).map_err(at("blob index in wide column"))
}

/// `Deserialize` [R db/wide/wide_column_serialization.cc:439-502]: the columns of a version 1
/// or 2 entity. A blob column's value is its serialized blob index; with `blob_columns` given,
/// each blob column's index and decoded blob index are appended to it. Without, a blob column
/// is corruption: the caller promised a resolved entity.
pub fn deserialize<'a>(
    input: &'a [u8],
    blob_columns: Option<&mut Vec<(usize, BlobIndex<'a>)>>,
) -> Result<WideColumns<'a>, Error> {
    let (ranges, blobs) = parse(input)?;
    let columns: WideColumns<'a> = ranges
        .into_iter()
        .map(|(name, value)| WideColumn {
            name: input.get(name).unwrap_or(&[]),
            value: input.get(value).unwrap_or(&[]),
        })
        .collect();
    match blob_columns {
        None if !blobs.is_empty() => Err(Error::corruption(
            "wide column blob reference in a resolved entity",
            Malformed::UnknownTag(ValueType::BlobIndex.as_u8()),
        )),
        None => Ok(columns),
        Some(out) => {
            for i in blobs {
                let value = columns.get(i).map_or(&[][..], |c| c.value);
                out.push((i, decode_blob_column(value)?));
            }
            Ok(columns)
        }
    }
}

/// `DeserializeSimple`: `deserialize` of an entity with no blob columns.
pub fn deserialize_simple(input: &[u8]) -> Result<WideColumns<'_>, Error> {
    deserialize(input, None)
}

/// `GetVersion` [R db/wide/wide_column_serialization.cc:641-651].
pub fn get_version(input: &[u8]) -> Result<u32, Error> {
    let mut rest = input;
    get_varint32(&mut rest).map_err(at("wide column version"))
}

/// A version-2 entity's column types, the bytes after them, and its skip info (the byte sizes
/// of the name-size, value-size and name sections).
struct V2Types<'a> {
    types: &'a [u8],
    rest: &'a [u8],
    skip: (usize, usize, usize),
}

/// The column types of a version-2 entity with columns, or `None` for version 1 or no columns.
fn v2_types(input: &[u8]) -> Result<Option<V2Types<'_>>, Error> {
    let mut rest = input;
    let version = get_varint32(&mut rest).map_err(at("wide column version"))?;
    if version < VERSION2 {
        return Ok(None);
    }
    let mut rest = input;
    let (_, num_columns) = read_header(&mut rest)?;
    if num_columns == 0 {
        return Ok(None);
    }
    let skip = read_skip_info(&mut rest).map_err(at("wide column skip info"))?;
    let n = usize::try_from(num_columns).map_err(|_| Error::truncated("wide column types"))?;
    let types = take(&mut rest, n, "wide column types")?;
    for &t in types {
        check_column_type(t)?;
    }
    Ok(Some(V2Types { types, rest, skip }))
}

/// `HasBlobColumns` [R db/wide/wide_column_serialization.cc:519-565]: whether a version-2
/// entity has a blob column, reading only its header and types.
pub fn has_blob_columns(input: &[u8]) -> Result<bool, Error> {
    Ok(v2_types(input)?.is_some_and(|v| v.types.contains(&ValueType::BlobIndex.as_u8())))
}

/// `ForEachBlobFileNumber` [R db/wide/wide_column_serialization.cc:567-639]: calls `f` with each
/// blob column's blob index, reading only the value sizes and the blob values.
pub fn for_each_blob_file_number(
    input: &[u8],
    mut f: impl FnMut(&BlobIndex<'_>) -> Result<(), Error>,
) -> Result<(), Error> {
    let Some(V2Types {
        types,
        mut rest,
        skip: (name_sizes, value_sizes, names),
    }) = v2_types(input)?
    else {
        return Ok(());
    };
    if !types.contains(&ValueType::BlobIndex.as_u8()) {
        return Ok(());
    }
    take(&mut rest, name_sizes, "wide column name sizes")?;
    let mut sizes = take(&mut rest, value_sizes, "wide column value sizes")?;
    take(&mut rest, names, "wide column names")?;
    let mut value_pos = 0usize;
    for &t in types {
        let vs = get_varint32(&mut sizes).map_err(at("wide column value size"))?;
        let end = usize::try_from(vs)
            .ok()
            .and_then(|vs| value_pos.checked_add(vs));
        if t == ValueType::BlobIndex.as_u8() {
            let value = end
                .and_then(|end| rest.get(value_pos..end))
                .ok_or(Error::truncated("wide column blob index"))?;
            f(&decode_blob_column(value)?)?;
        }
        value_pos = end.ok_or(Error::truncated("wide column value size"))?;
    }
    Ok(())
}

/// `GetValueOfDefaultColumn` [R db/wide/wide_column_serialization.cc:653-765]: the default
/// column's value (empty when there is none) and whether it is a serialized blob index to
/// resolve with [`resolve_default_column_blob_reference`]. A version-2 entity is read without
/// parsing its other columns.
pub fn get_value_of_default_column(input: &[u8]) -> Result<(&[u8], bool), Error> {
    let mut rest = input;
    let (version, num_columns) = read_header(&mut rest)?;
    if num_columns == 0 {
        return Ok((&[], false));
    }
    if version >= VERSION2 {
        let (name_sizes, value_sizes, names) = read_skip_info(&mut rest)?;
        let n = usize::try_from(num_columns).map_err(|_| Error::truncated("wide column types"))?;
        let types = take(&mut rest, n, "wide column types")?;
        // Only column 0 can be the default column; its type is checked so a bad byte is
        // corruption rather than taken for an inline value.
        let column0_is_blob = check_column_type(types.first().copied().unwrap_or(0))?;
        let mut first_name = take(&mut rest, name_sizes, "wide column name sizes")?;
        let first_name_size = get_varint32(&mut first_name).map_err(at("wide column name size"))?;
        let mut first_value = take(&mut rest, value_sizes, "wide column value sizes")?;
        let first_value_size =
            get_varint32(&mut first_value).map_err(at("wide column value size"))?;
        if first_name_size != 0 {
            return Ok((&[], false));
        }
        take(&mut rest, names, "wide column names")?;
        let size = usize::try_from(first_value_size)
            .map_err(|_| Error::truncated("wide column value payload"))?;
        let value = rest
            .get(..size)
            .ok_or(Error::truncated("wide column value payload"))?;
        return Ok((value, column0_is_blob));
    }
    let columns = deserialize_simple(input)?;
    Ok(match columns.first() {
        Some(c) if c.name == DEFAULT_WIDE_COLUMN_NAME => (c.value, false),
        _ => (&[], false),
    })
}

/// `ResolveDefaultColumnBlobReference` [R db/wide/wide_column_serialization.cc:865-894]: the
/// value a blob-backed default column refers to.
pub fn resolve_default_column_blob_reference<'a>(
    blob_index: &'a [u8],
    user_key: &[u8],
    blob_fetcher: Option<&dyn BlobFetcher>,
) -> Result<Cow<'a, [u8]>, Error> {
    if blob_index.is_empty() {
        return Err(Error::truncated("blob index in wide column default value"));
    }
    let index = BlobIndex::decode_from(blob_index)
        .map_err(at("blob index in wide column default value"))?;
    if let Some(value) = index.value() {
        return Ok(Cow::Borrowed(value));
    }
    let fetcher = blob_fetcher.ok_or(Error::corruption(
        "blob-backed default column",
        Malformed::Unresolvable,
    ))?;
    Ok(Cow::Owned(fetcher.fetch_blob(user_key, &index)?))
}

/// `ResolveEntityBlobColumns` [R db/wide/wide_column_serialization.cc:767-783]: the entity with
/// every blob column fetched, re-serialized in version 1; `None` when it has no blob columns.
pub fn resolve_entity_blob_columns(
    entity: &[u8],
    user_key: &[u8],
    blob_fetcher: Option<&dyn BlobFetcher>,
) -> Result<Option<Vec<u8>>, Error> {
    let mut blob_columns = Vec::new();
    let mut columns = deserialize(entity, Some(&mut blob_columns))?;
    if blob_columns.is_empty() {
        return Ok(None);
    }
    // Fetched values, held until the columns that borrow them are serialized.
    let mut fetched: Vec<(usize, Vec<u8>)> = Vec::new();
    for (i, index) in &blob_columns {
        if let Some(value) = index.value() {
            if let Some(c) = columns.get_mut(*i) {
                c.value = value;
            }
            continue;
        }
        let fetcher = blob_fetcher.ok_or(Error::corruption(
            "blob column in entity",
            Malformed::Unresolvable,
        ))?;
        fetched.push((*i, fetcher.fetch_blob(user_key, index)?));
    }
    for (i, value) in &fetched {
        if let Some(c) = columns.get_mut(*i) {
            c.value = value;
        }
    }
    let mut out = Vec::new();
    serialize(&columns, &mut out)?;
    Ok(Some(out))
}

/// `ResolveEntityForMerge` [R db/wide/wide_column_serialization.cc:896-922]: the entity itself
/// when it has no blob columns, otherwise its resolved version-1 form.
pub fn resolve_entity_for_merge<'a>(
    entity: &'a [u8],
    user_key: &[u8],
    blob_fetcher: Option<&dyn BlobFetcher>,
) -> Result<Cow<'a, [u8]>, Error> {
    if !has_blob_columns(entity)? {
        return Ok(Cow::Borrowed(entity));
    }
    Ok(
        match resolve_entity_blob_columns(entity, user_key, blob_fetcher)? {
            Some(resolved) => Cow::Owned(resolved),
            None => Cow::Borrowed(entity),
        },
    )
}
