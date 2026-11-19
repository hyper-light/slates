//! The locks API (A-610; git-lfs v3.8.1 `docs/api/locking.md` and `locking/schemas/` [B]; the client's
//! `locking/api.go` [C]).
//!
//! **Requests.** Creating a lock ([`read_create`], `POST /locks`) names its `path` and optionally a `ref`. Listing them
//! (`GET /locks`) sends `path`, `id`, `cursor`, `limit` and `refspec` as a query ([`read_list_query`]), which the
//! client writes with Go's `url.Values.Encode` (keys sorted, values form-escaped: `+` for a space, `%XX` for the rest),
//! leaving out a cursor and a refspec that are empty and a limit that is not above zero. Listing for a push's
//! verification ([`read_verify`], `POST /locks/verify`) sends `ref`, `cursor` and `limit` in a body. Releasing one
//! ([`read_unlock`], `POST /locks/{id}/unlock`, the id in the path: [`super::route`]) sends `force` and a `ref`.
//!
//! **Answers.** A created or released lock is answered `{"lock": ...}` ([`write_lock`]); a path already locked,
//! `{"lock": ..., "message": ...}` with the existing lock ([`write_conflict`]); a list, `{"locks": [...],
//! "next_cursor": ...}` ([`write_list`]); a verification, `{"ours": [...], "theirs": [...], "next_cursor": ...}`
//! ([`write_verify`]). A lock is its `id`, `path`, `locked_at` (RFC 3339, upper case, to the second) and `owner`'s
//! `name`. Who may release whose lock, and which locks are ours, is the server's to decide; this module only carries
//! it.
//!
//! **Read as the client writes, and stricter where a server must be.** Unknown fields and query keys are skipped; a
//! known one given twice is refused. `null` reads as absent where the client's Go struct reads it so (a cursor, a
//! limit, `force`). A limit is an integer from zero to Go's `int64` limit, the client's `Limit int`; a negative one, a
//! fraction and anything larger are refused by kind.

use super::json::{self, Cursor, Integer, JsonStr, NumberRefusal, Shape};
use super::{ErrorBody, RefProblem, Timestamp, read_ref};
use crate::out::Out;
use crate::{Admission, to_u64};

/// Format: the largest limit git-lfs sends, Go's `int64` maximum (`locking/api.go`: `Limit int`, 64 bits on every
/// platform git-lfs ships for).
pub const MAX_LIMIT: u64 = i64::MAX.unsigned_abs();

/// Format: the base of a limit's digits in a query.
const DECIMAL_RADIX: u32 = 10;

/// Format: the base of a `%XX` escape's digits (RFC 3986 §2.1).
const HEX_RADIX: u32 = 16;

/// Format: the bits one hexadecimal digit carries.
const HEX_DIGIT_BITS: u32 = 4;

/// A field of a locks request, as a refusal names it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Field {
  /// The body itself, which must be an object.
  Body,
  /// `path`.
  Path,
  /// `ref`.
  Ref,
  /// `ref`'s `name`.
  RefName,
  /// `cursor`.
  Cursor,
  /// `limit`.
  Limit,
  /// `force`.
  Force,
}

/// Why a body is not a locks request. Each names where it stood, from the body's first byte.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
  /// Not one JSON value.
  Json(json::Refusal),
  /// A field holding a value of another type than its own.
  Type {
    /// The field.
    field: Field,
    /// Where the value begins.
    at: u64,
  },
  /// A required field absent.
  Missing {
    /// The field.
    field: Field,
    /// Where the body begins.
    at: u64,
  },
  /// A field given twice.
  Repeated {
    /// The field.
    field: Field,
    /// Where the second value begins.
    at: u64,
  },
  /// A `limit` below zero.
  NegativeLimit {
    /// Where its value begins.
    at: u64,
  },
  /// A `limit` with a fraction or an exponent, or past Go's `int64` ([`MAX_LIMIT`]).
  Limit {
    /// Where its value begins.
    at: u64,
    /// Which.
    kind: NumberRefusal,
  },
}

/// A refusal of the value at `at` as not `field`'s type.
fn type_of(field: Field) -> impl Fn(Shape) -> Refusal {
  move |shape| Refusal::Type {
    field,
    at: shape.at,
  }
}

/// The refusal a problem with the `ref` property is.
fn ref_refusal(problem: RefProblem) -> Refusal {
  match problem {
    RefProblem::Type { at } => Refusal::Type {
      field: Field::Ref,
      at,
    },
    RefProblem::NameType { at } => Refusal::Type {
      field: Field::RefName,
      at,
    },
    RefProblem::NameRepeated { at } => Refusal::Repeated {
      field: Field::RefName,
      at,
    },
  }
}

/// Refuses `field` at `at` when it was `seen` already.
fn once(seen: bool, field: Field, at: u64) -> Result<(), Refusal> {
  if seen {
    return Err(Refusal::Repeated { field, at });
  }
  Ok(())
}

/// Validates `body` and enters its object: the cursor, and where the body begins.
fn open<'a>(body: &'a [u8], admission: &mut Admission) -> Result<(Cursor<'a>, u64), Refusal> {
  json::validate(body, admission).map_err(Refusal::Json)?;
  let mut cursor = Cursor::new(body);
  let start = cursor.position();
  cursor.begin_object().map_err(type_of(Field::Body))?;
  Ok((cursor, start))
}

/// A request to create a lock.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CreateRequest<'a> {
  /// The path to lock, relative to the repository's root.
  pub path: JsonStr<'a>,
  /// The ref the lock belongs to, when the client named one.
  pub ref_name: Option<JsonStr<'a>>,
}

/// Reads a request to create a lock: `{"path": ..., "ref": {"name": ...}}`, the path required.
pub fn read_create<'a>(
  body: &'a [u8],
  admission: &mut Admission,
) -> Result<CreateRequest<'a>, Refusal> {
  let (mut cursor, start) = open(body, admission)?;
  let (mut path, mut ref_name, mut ref_seen) = (None, None, false);
  while let Some(key) = cursor.next_key().map_err(type_of(Field::Body))? {
    let at = cursor.position();
    if key.is("path") {
      once(path.is_some(), Field::Path, at)?;
      path = Some(cursor.string().map_err(type_of(Field::Path))?);
    } else if key.is("ref") {
      once(ref_seen, Field::Ref, at)?;
      ref_seen = true;
      ref_name = read_ref(&mut cursor).map_err(ref_refusal)?;
    } else {
      cursor.skip_value().map_err(type_of(Field::Body))?;
    }
  }
  Ok(CreateRequest {
    path: path.ok_or(Refusal::Missing {
      field: Field::Path,
      at: start,
    })?,
    ref_name,
  })
}

/// A request to list locks for a push's verification.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VerifyRequest<'a> {
  /// The ref the push updates, when the client named one.
  pub ref_name: Option<JsonStr<'a>>,
  /// Where the previous page ended: its `next_cursor`.
  pub cursor: Option<JsonStr<'a>>,
  /// The most locks the client asks for in one answer.
  pub limit: Option<u64>,
}

/// Reads a `limit`'s value: an integer from zero to [`MAX_LIMIT`], or `null` for none.
fn read_limit(cursor: &mut Cursor<'_>) -> Result<Option<u64>, Refusal> {
  if cursor.null() {
    return Ok(None);
  }
  let at = cursor.position();
  let number = cursor.number().map_err(type_of(Field::Limit))?;
  match number.integer() {
    Ok(Integer::NonNegative(limit)) if limit <= MAX_LIMIT => Ok(Some(limit)),
    Ok(Integer::NonNegative(_)) => Err(Refusal::Limit {
      at,
      kind: NumberRefusal::TooLarge,
    }),
    Ok(Integer::Negative(_)) => Err(Refusal::NegativeLimit { at }),
    Err(kind) => Err(Refusal::Limit { at, kind }),
  }
}

/// Reads a string field's value, or `null` for none.
fn read_optional_string<'a>(
  cursor: &mut Cursor<'a>,
  field: Field,
) -> Result<Option<JsonStr<'a>>, Refusal> {
  if cursor.null() {
    return Ok(None);
  }
  cursor.string().map(Some).map_err(type_of(field))
}

/// Reads a request to list locks for verification: `{"ref": {"name": ...}, "cursor": ..., "limit": ...}`, each
/// optional.
pub fn read_verify<'a>(
  body: &'a [u8],
  admission: &mut Admission,
) -> Result<VerifyRequest<'a>, Refusal> {
  let (mut cursor, _) = open(body, admission)?;
  let mut request = VerifyRequest {
    ref_name: None,
    cursor: None,
    limit: None,
  };
  let mut seen = [false; 3];
  while let Some(key) = cursor.next_key().map_err(type_of(Field::Body))? {
    let at = cursor.position();
    let field = if key.is("ref") {
      Field::Ref
    } else if key.is("cursor") {
      Field::Cursor
    } else if key.is("limit") {
      Field::Limit
    } else {
      cursor.skip_value().map_err(type_of(Field::Body))?;
      continue;
    };
    let slot = match field {
      Field::Ref => seen.get_mut(0),
      Field::Cursor => seen.get_mut(1),
      _ => seen.get_mut(2),
    };
    if let Some(given) = slot {
      once(*given, field, at)?;
      *given = true;
    }
    match field {
      Field::Ref => request.ref_name = read_ref(&mut cursor).map_err(ref_refusal)?,
      Field::Cursor => request.cursor = read_optional_string(&mut cursor, Field::Cursor)?,
      _ => request.limit = read_limit(&mut cursor)?,
    }
  }
  Ok(request)
}

/// A request to release a lock, whose id the path names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UnlockRequest<'a> {
  /// Whether the user releases another user's lock.
  pub force: bool,
  /// The ref the lock belongs to, when the client named one.
  pub ref_name: Option<JsonStr<'a>>,
}

/// Reads a request to release a lock: `{"force": ..., "ref": {"name": ...}}`, each optional; `force` absent or `null`
/// is false.
pub fn read_unlock<'a>(
  body: &'a [u8],
  admission: &mut Admission,
) -> Result<UnlockRequest<'a>, Refusal> {
  let (mut cursor, _) = open(body, admission)?;
  let mut request = UnlockRequest {
    force: false,
    ref_name: None,
  };
  let (mut force_seen, mut ref_seen) = (false, false);
  while let Some(key) = cursor.next_key().map_err(type_of(Field::Body))? {
    let at = cursor.position();
    if key.is("force") {
      once(force_seen, Field::Force, at)?;
      force_seen = true;
      request.force = !cursor.null() && cursor.boolean().map_err(type_of(Field::Force))?;
    } else if key.is("ref") {
      once(ref_seen, Field::Ref, at)?;
      ref_seen = true;
      request.ref_name = read_ref(&mut cursor).map_err(ref_refusal)?;
    } else {
      cursor.skip_value().map_err(type_of(Field::Body))?;
    }
  }
  Ok(request)
}

/// A value of a URL query, borrowed form-encoded: `+` for a space, `%XX` for a byte, as Go's `url.QueryEscape` writes
/// it. Its escapes are well formed and the bytes they make are UTF-8.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct QueryStr<'a> {
  /// The value as the query carries it.
  raw: &'a [u8],
}

impl<'a> QueryStr<'a> {
  /// The value's bytes, decoded.
  pub fn bytes(&self) -> impl Iterator<Item = u8> + use<'a> {
    decoded(self.raw).map(|(_, byte)| byte)
  }

  /// Whether the value is `text`.
  pub fn is(&self, text: &str) -> bool {
    self.bytes().eq(text.bytes())
  }

  /// The value as text, owned, its exact length charged to `admission` before it is made.
  pub fn owned(&self, admission: &mut Admission) -> Result<String, crate::PastAdmission> {
    let len = self.bytes().count();
    admission.charge(to_u64(len))?;
    let mut bytes = Vec::new();
    bytes
      .try_reserve_exact(len)
      .map_err(|_| crate::PastAdmission {
        asked: to_u64(len),
        left: admission.left(),
      })?;
    bytes.extend(self.bytes());
    Ok(String::from_utf8(bytes).unwrap_or_default())
  }
}

/// The bytes form-encoded `raw` makes, each with where its encoding begins in `raw`; an escape that is not `%` and two
/// hexadecimal digits yields nothing more.
fn decoded(raw: &[u8]) -> impl Iterator<Item = (usize, u8)> + '_ {
  let mut at = 0usize;
  core::iter::from_fn(move || {
    let start = at;
    let byte = *raw.get(at)?;
    match byte {
      b'+' => {
        at = at.saturating_add(1);
        Some((start, b' '))
      }
      b'%' => {
        let high = char::from(*raw.get(at.saturating_add(1))?).to_digit(HEX_RADIX)?;
        let low = char::from(*raw.get(at.saturating_add(2))?).to_digit(HEX_RADIX)?;
        at = at.saturating_add(3);
        u8::try_from(high.checked_shl(HEX_DIGIT_BITS)? | low)
          .ok()
          .map(|value| (start, value))
      }
      _ => {
        at = at.saturating_add(1);
        Some((start, byte))
      }
    }
  })
}

/// Why a query is not a lock listing's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QueryRefusal {
  /// A `%` without two hexadecimal digits after it.
  Escape {
    /// Where it is, from the query's first byte.
    at: u64,
  },
  /// A key or value whose bytes are not UTF-8.
  NotUtf8 {
    /// Where its encoding begins.
    at: u64,
  },
  /// A key given twice.
  Repeated {
    /// Where the second pair begins.
    at: u64,
  },
  /// A `limit` that is not a decimal integer from zero to [`MAX_LIMIT`].
  Limit {
    /// Where its value begins.
    at: u64,
  },
}

/// A lock listing's query: each filter and the page's place and size, form-encoded values borrowed in place.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ListQuery<'a> {
  /// Only the lock on this path.
  pub path: Option<QueryStr<'a>>,
  /// Only the lock with this id.
  pub id: Option<QueryStr<'a>>,
  /// Where the previous page ended: its `next_cursor`.
  pub cursor: Option<QueryStr<'a>>,
  /// The most locks the client asks for in one answer.
  pub limit: Option<u64>,
  /// The ref whose locks are asked for.
  pub refspec: Option<QueryStr<'a>>,
}

/// Checks the form-encoded `raw`, which begins at `offset` in its query: escapes whole, bytes UTF-8.
fn check_component(raw: &[u8], offset: usize) -> Result<(), QueryRefusal> {
  let at = |place: usize| to_u64(offset.saturating_add(place));
  let mut sequence = [0u8; 4];
  let (mut held, mut needed, mut began) = (0usize, 0usize, 0usize);
  let mut seen = 0usize;
  for (place, byte) in decoded(raw) {
    seen = place.saturating_add(if raw.get(place) == Some(&b'%') { 3 } else { 1 });
    if held == 0 {
      began = place;
      needed = match byte.leading_ones() {
        0 => 1,
        2..=4 => usize::try_from(byte.leading_ones()).unwrap_or(1),
        _ => return Err(QueryRefusal::NotUtf8 { at: at(place) }),
      };
    }
    if let Some(slot) = sequence.get_mut(held) {
      *slot = byte;
    }
    held = held.saturating_add(1);
    if held == needed {
      if core::str::from_utf8(sequence.get(..held).unwrap_or_default()).is_err() {
        return Err(QueryRefusal::NotUtf8 { at: at(began) });
      }
      held = 0;
    }
  }
  if seen < raw.len() {
    return Err(QueryRefusal::Escape { at: at(seen) });
  }
  if held != 0 {
    return Err(QueryRefusal::NotUtf8 { at: at(began) });
  }
  Ok(())
}

/// Format: the keys a lock listing's query carries, in [`ListQuery`]'s order [B: locking.md "List Locks"].
const LIST_KEYS: [&str; 5] = ["path", "id", "cursor", "limit", "refspec"];

/// Reads a lock listing's query, the part of the URL after `?`: pairs split at `&`, each key and value split at its
/// first `=` (a pair without one is a key with an empty value), empty pairs skipped as Go's `url.ParseQuery` skips
/// them, unknown keys skipped, a known key twice refused.
pub fn read_list_query(query: &[u8]) -> Result<ListQuery<'_>, QueryRefusal> {
  let mut list = ListQuery::default();
  let mut seen = [false; LIST_KEYS.len()];
  let mut offset = 0usize;
  for pair in query.split(|byte| *byte == b'&') {
    let start = offset;
    offset = offset.saturating_add(pair.len()).saturating_add(1);
    if pair.is_empty() {
      continue;
    }
    let (key, value, value_at) = match pair.iter().position(|byte| *byte == b'=') {
      Some(equals) => (
        pair.get(..equals).unwrap_or_default(),
        pair.get(equals.saturating_add(1)..).unwrap_or_default(),
        start.saturating_add(equals).saturating_add(1),
      ),
      None => (pair, &[][..], start.saturating_add(pair.len())),
    };
    check_component(key, start)?;
    check_component(value, value_at)?;
    let key = QueryStr { raw: key };
    let Some(index) = LIST_KEYS.iter().position(|known| key.is(known)) else {
      continue;
    };
    if let Some(given) = seen.get_mut(index) {
      if *given {
        return Err(QueryRefusal::Repeated { at: to_u64(start) });
      }
      *given = true;
    }
    store(&mut list, index, QueryStr { raw: value }, value_at)?;
  }
  Ok(list)
}

/// Stores the value of the known key at `index` of [`LIST_KEYS`] into `list`.
fn store<'a>(
  list: &mut ListQuery<'a>,
  index: usize,
  value: QueryStr<'a>,
  at: usize,
) -> Result<(), QueryRefusal> {
  match index {
    0 => list.path = Some(value),
    1 => list.id = Some(value),
    2 => list.cursor = Some(value),
    3 => list.limit = Some(query_limit(value).ok_or(QueryRefusal::Limit { at: to_u64(at) })?),
    _ => list.refspec = Some(value),
  }
  Ok(())
}

/// A query's `limit`: one or more decimal digits making at most [`MAX_LIMIT`].
fn query_limit(value: QueryStr<'_>) -> Option<u64> {
  let mut limit: u64 = 0;
  let mut digits = 0usize;
  for byte in value.bytes() {
    let digit = char::from(byte).to_digit(DECIMAL_RADIX)?;
    limit = limit
      .checked_mul(u64::from(DECIMAL_RADIX))?
      .checked_add(u64::from(digit))?;
    digits = digits.saturating_add(1);
  }
  (digits > 0 && limit <= MAX_LIMIT).then_some(limit)
}

/// A lock, as an answer gives it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Lock<'a> {
  /// Its id: a server issues ids of RFC 3986's unreserved characters, since the client writes it into a path unescaped
  /// ([`super::Route::Unlock`]).
  pub id: &'a str,
  /// The path it locks.
  pub path: &'a str,
  /// When it was taken.
  pub locked_at: Timestamp,
  /// Who took it, by name.
  pub owner: Option<&'a str>,
}

/// Writes one lock: `{"id":...,"path":...,"locked_at":...,"owner":{"name":...}}`.
fn put_lock(lock: &Lock<'_>, out: &mut impl Out) {
  out.put(b"{\"id\":");
  json::put_str(out, lock.id);
  out.put(b",\"path\":");
  json::put_str(out, lock.path);
  out.put(b",\"locked_at\":\"");
  lock.locked_at.put_rfc3339(out);
  out.put(b"\"");
  if let Some(owner) = lock.owner {
    out.put(b",\"owner\":{\"name\":");
    json::put_str(out, owner);
    out.put(b"}");
  }
  out.put(b"}");
}

/// Writes locks as an array.
fn put_locks(locks: &[Lock<'_>], out: &mut impl Out) {
  out.put(b"[");
  for (place, lock) in locks.iter().enumerate() {
    if place > 0 {
      out.put(b",");
    }
    put_lock(lock, out);
  }
  out.put(b"]");
}

/// Writes the answer to a lock created (201) or released (200): `{"lock": ...}`.
pub fn write_lock(lock: &Lock<'_>, out: &mut impl Out) {
  out.put(b"{\"lock\":");
  put_lock(lock, out);
  out.put(b"}");
}

/// Writes the answer to a lock refused because the path is locked already (409): the existing lock, and why.
pub fn write_conflict(lock: &Lock<'_>, error: &ErrorBody<'_>, out: &mut impl Out) {
  out.put(b"{\"lock\":");
  put_lock(lock, out);
  error.put_fields(out, false);
  out.put(b"}");
}

/// Writes a page of a lock listing: `{"locks": [...], "next_cursor": ...}`, the cursor only when more locks match.
pub fn write_list(locks: &[Lock<'_>], next_cursor: Option<&str>, out: &mut impl Out) {
  out.put(b"{\"locks\":");
  put_locks(locks, out);
  put_next_cursor(next_cursor, out);
  out.put(b"}");
}

/// Writes a page of a verification: `{"ours": [...], "theirs": [...], "next_cursor": ...}`, the cursor only when more
/// locks match.
pub fn write_verify(
  ours: &[Lock<'_>],
  theirs: &[Lock<'_>],
  next_cursor: Option<&str>,
  out: &mut impl Out,
) {
  out.put(b"{\"ours\":");
  put_locks(ours, out);
  out.put(b",\"theirs\":");
  put_locks(theirs, out);
  put_next_cursor(next_cursor, out);
  out.put(b"}");
}

/// Writes `,"next_cursor": ...` when there is one.
fn put_next_cursor(next_cursor: Option<&str>, out: &mut impl Out) {
  if let Some(cursor) = next_cursor {
    out.put(b",\"next_cursor\":");
    json::put_str(out, cursor);
  }
}

#[cfg(test)]
#[path = "../tests/lfs_locks.rs"]
mod tests;
