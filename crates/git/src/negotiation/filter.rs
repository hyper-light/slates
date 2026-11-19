//! `filter <spec>` (A-615 part 2; §4.17.2 "A fetch" item 3, "Filters"): the specs git reads and what each sends
//! [B: `rev-list-options` "--filter"; C: `list-objects-filter-options.c` `gently_parse_list_objects_filter` and
//! `parse_combine_filter`, `list-objects-filter.c`, git v2.50.1].
//!
//! - `blob:none`: no blobs.
//! - `blob:limit=<n>`: blobs smaller than `n` bytes (git's test is `object_length < max_bytes`); `n` may end with `k`,
//!   `m` or `g`, 2^10, 2^20 and 2^30, as the document asks receivers to accept.
//! - `tree:<depth>`: trees and blobs whose least depth below a commit's root tree is under `depth`, the root tree at
//!   depth 0 and each entry one deeper than its tree (`filter_trees_depth`); `tree:0` sends no tree and no blob.
//! - `object:type=<type>`: objects of that type alone (`filter_object_type`), commits and tags included.
//! - `combine:<spec>+<spec>...`: what every spec sends. Each sub-spec is percent-decoded, and refused, as git refuses it,
//!   when its encoded form holds a space, a control byte or one of ``~`!@#$^&*()[]{}\;'",<>?``.
//! - `sparse:oid=<blob-ish>`: read, and refused as not served until the walk takes it: its blob's patterns are read and
//!   asked as git reads and asks them ([`super::sparse`], A-640 part 4), and the walk by path, with the tree frames
//!   git's filter keeps, and the blob's name resolved are owed. `sparse:path=`, which git dropped, is refused as git
//!   refuses it.
//!
//! Commits and tags are sent under every filter but `object:type`; the objects a request names, and what their tags
//! name, are sent under every filter, as git sends the objects a client named (`--filter-provided-objects` off). A
//! number is canonical decimal: git's `git_parse_ulong` also reads `0x`, octal and a sign, which no client writes.
//! A `combine:` within a `combine:` is refused: git reads it, but its own client writes every filter into one
//! `combine:`, and refusing it keeps the parse free of recursion.

use crate::object_name::Kind;

/// Format: the spec that sends no blobs.
const BLOB_NONE: &[u8] = b"blob:none";

/// Format: what a size limit's spec begins with.
const BLOB_LIMIT: &[u8] = b"blob:limit=";

/// Format: what a tree depth's spec begins with.
const TREE: &[u8] = b"tree:";

/// Format: what a type's spec begins with.
const OBJECT_TYPE: &[u8] = b"object:type=";

/// Format: what a combination's spec begins with.
const COMBINE: &[u8] = b"combine:";

/// Format: the byte between a combination's sub-specs.
const PLUS: u8 = b'+';

/// Format: what a sparse filter's spec begins with.
const SPARSE_OID: &[u8] = b"sparse:oid=";

/// Format: what the sparse filter git dropped began with.
const SPARSE_PATH: &[u8] = b"sparse:path=";

/// Format: the bytes a combination's encoded sub-spec may not hold besides space, the controls and the bytes past
/// ASCII, which git's `has_reserved_character` refuses as a signed `char` at or below space, on the x86-64 hosts most
/// servers run (`list-objects-filter-options.c`'s `RESERVED_NON_WS`).
const RESERVED: &[u8] = b"~`!@#$^&*()[]{}\\;'\",<>?";

/// Format: the byte a percent-encoded byte begins with.
const PERCENT: u8 = b'%';

/// Format: the radix of a percent-encoded byte's two digits.
const HEX_RADIX: u32 = 16;

/// Format: the digits of a decimal number.
const DECIMAL_DIGITS: core::ops::RangeInclusive<u8> = b'0'..=b'9';

/// Format: the base of a decimal number.
const DECIMAL: u64 = 10;

/// Format: each size unit and its factor (`config.c`'s `get_unit_factor`): 2^10, 2^20 and 2^30, either case.
const UNITS: [(u8, u64); 6] = [
  (b'k', 1 << 10),
  (b'K', 1 << 10),
  (b'm', 1 << 20),
  (b'M', 1 << 20),
  (b'g', 1 << 30),
  (b'G', 1 << 30),
];

/// A filter.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Filter {
  /// `blob:none`.
  BlobNone,
  /// `blob:limit=<n>`: blobs smaller than `n` bytes.
  BlobLimit(u64),
  /// `tree:<depth>`: trees and blobs less deep than `depth`.
  TreeDepth(u64),
  /// `object:type=<type>`.
  ObjectType(Kind),
  /// `sparse:oid=<blob-ish>`, its value as sent; read, never served.
  SparseOid(Vec<u8>),
  /// `combine:`: what every filter sends.
  Combine(Vec<Filter>),
}

/// Why a spec is not a filter this negotiation serves.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
  /// A spec git does not read ("invalid filter-spec").
  Unknown,
  /// A size or depth that is not canonical decimal with an optional unit, or past 64 bits.
  Number,
  /// `object:type=` naming no type.
  Type,
  /// `combine:` with nothing after it, or an empty sub-spec.
  Empty,
  /// A combination's encoded sub-spec holding a space, a control or a reserved byte.
  Reserved,
  /// A `combine:` within a `combine:`.
  NestedCombine,
  /// `sparse:path=`, which git dropped ("sparse:path filters support has been dropped").
  SparsePath,
  /// `sparse:oid=`, read and not served.
  SparseOid,
}

impl Filter {
  /// The filter `spec` writes.
  pub fn parse(spec: &[u8]) -> Result<Filter, Refusal> {
    match spec.strip_prefix(COMBINE) {
      Some(rest) => combine(rest),
      None => single(spec),
    }
  }

  /// Whether the filter decides each object by its own row, its kind and size, so a pack can apply it to a run of
  /// versions' fragments: no depth, no sparse patterns.
  pub fn is_per_object(&self) -> bool {
    match self {
      Filter::TreeDepth(_) | Filter::SparseOid(_) => false,
      Filter::Combine(filters) => filters.iter().all(Filter::is_per_object),
      _ => true,
    }
  }

  /// Whether the filter sends an object of `kind` and `size`, met `depth` below its commit's root tree (trees and blobs;
  /// commits and tags take any depth).
  pub fn admits(&self, kind: Kind, size: u64, depth: u64) -> bool {
    match self {
      Filter::BlobNone => kind != Kind::Blob,
      Filter::BlobLimit(limit) => kind != Kind::Blob || size < *limit,
      Filter::TreeDepth(limit) => matches!(kind, Kind::Commit | Kind::Tag) || depth < *limit,
      Filter::ObjectType(wanted) => kind == *wanted,
      Filter::SparseOid(_) => true,
      Filter::Combine(filters) => filters
        .iter()
        .all(|filter| filter.admits(kind, size, depth)),
    }
  }

  /// Whether a tree or blob met `depth` below its commit's root tree can be sent at all: not at or past a depth limit,
  /// and not under a filter that sends only commits or tags. A walk opens no tree whose entries none could be.
  pub fn reaches(&self, depth: u64) -> bool {
    match self {
      Filter::TreeDepth(limit) => depth < *limit,
      Filter::ObjectType(wanted) => matches!(wanted, Kind::Tree | Kind::Blob),
      Filter::Combine(filters) => filters.iter().all(|filter| filter.reaches(depth)),
      _ => true,
    }
  }
}

/// Refused when `filter` is one this negotiation reads but does not serve: `sparse:oid`.
pub(super) fn check_served(filter: &Filter) -> Result<(), Refusal> {
  match filter {
    Filter::SparseOid(_) => Err(Refusal::SparseOid),
    Filter::Combine(filters) => filters.iter().try_for_each(check_served),
    _ => Ok(()),
  }
}

/// The filter a spec other than `combine:` writes.
fn single(spec: &[u8]) -> Result<Filter, Refusal> {
  if spec == BLOB_NONE {
    return Ok(Filter::BlobNone);
  }
  if let Some(value) = spec.strip_prefix(BLOB_LIMIT) {
    return number(value).map(Filter::BlobLimit);
  }
  if let Some(value) = spec.strip_prefix(TREE) {
    return number(value).map(Filter::TreeDepth);
  }
  if let Some(value) = spec.strip_prefix(OBJECT_TYPE) {
    return Kind::from_name(value)
      .map(Filter::ObjectType)
      .map_err(|_| Refusal::Type);
  }
  if let Some(value) = spec.strip_prefix(SPARSE_OID) {
    return Ok(Filter::SparseOid(value.to_vec()));
  }
  if spec.starts_with(SPARSE_PATH) {
    return Err(Refusal::SparsePath);
  }
  Err(Refusal::Unknown)
}

/// The combination `rest`, what follows `combine:`, writes.
fn combine(rest: &[u8]) -> Result<Filter, Refusal> {
  if rest.is_empty() {
    return Err(Refusal::Empty);
  }
  let mut filters = Vec::new();
  for encoded in rest.split(|byte| *byte == PLUS) {
    if encoded.is_empty() {
      return Err(Refusal::Empty);
    }
    if encoded
      .iter()
      .any(|byte| *byte <= b' ' || !byte.is_ascii() || RESERVED.contains(byte))
    {
      return Err(Refusal::Reserved);
    }
    let decoded = percent_decode(encoded);
    if decoded.starts_with(COMBINE) {
      return Err(Refusal::NestedCombine);
    }
    filters.push(single(&decoded)?);
  }
  Ok(Filter::Combine(filters))
}

/// `encoded` with each `%` and two hexadecimal digits made the byte they name; any other `%` kept, as git's
/// `url_percent_decode` keeps it.
fn percent_decode(encoded: &[u8]) -> Vec<u8> {
  let mut decoded = Vec::with_capacity(encoded.len());
  let mut at = 0usize;
  while let Some(byte) = encoded.get(at).copied() {
    let pair = encoded.get(at.saturating_add(1)..at.saturating_add(3));
    match (byte, pair.and_then(hex_pair)) {
      (PERCENT, Some(value)) => {
        decoded.push(value);
        at = at.saturating_add(3);
      }
      _ => {
        decoded.push(byte);
        at = at.saturating_add(1);
      }
    }
  }
  decoded
}

/// The byte two hexadecimal digits name.
fn hex_pair(pair: &[u8]) -> Option<u8> {
  let [high, low] = pair else {
    return None;
  };
  let high = char::from(*high).to_digit(HEX_RADIX)?;
  let low = char::from(*low).to_digit(HEX_RADIX)?;
  u8::try_from(high.checked_mul(HEX_RADIX)?.checked_add(low)?).ok()
}

/// The value of `text`: canonical decimal digits, then a unit or nothing, within 64 bits.
fn number(text: &[u8]) -> Result<u64, Refusal> {
  let (digits, factor) = match text.split_last() {
    Some((last, digits)) if !last.is_ascii_digit() => {
      let factor = UNITS
        .iter()
        .find(|(unit, _)| unit == last)
        .map(|(_, factor)| *factor)
        .ok_or(Refusal::Number)?;
      (digits, factor)
    }
    _ => (text, 1),
  };
  let canonical = !digits.is_empty()
    && digits.iter().all(|byte| DECIMAL_DIGITS.contains(byte))
    && (digits.len() == 1 || digits.first() != Some(&b'0'));
  if !canonical {
    return Err(Refusal::Number);
  }
  digits
    .iter()
    .try_fold(0u64, |value, digit| {
      value
        .checked_mul(DECIMAL)?
        .checked_add(u64::from(digit.saturating_sub(b'0')))
    })
    .and_then(|value| value.checked_mul(factor))
    .ok_or(Refusal::Number)
}

#[cfg(test)]
#[path = "tests/filter.rs"]
mod tests;
