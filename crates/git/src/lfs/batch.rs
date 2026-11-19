//! The batch API and the basic transfer adapter (A-610; git-lfs v3.8.1 `docs/api/batch.md` and `basic-transfers.md`
//! [B]; the client's `tq/api.go` and `tq/transfer.go` [C]).
//!
//! **A request** ([`read_request`]) names an `operation`, `download` or `upload`; optionally the transfer adapters the
//! client has (`transfers`; "If omitted, the `basic` transfer adapter MUST be assumed"), the ref the objects belong to
//! (`ref`, `{"name": ...}`; the client writes `"ref": null` without one, since its struct has no `omitempty`) and the
//! hash (`hash_algo`, SHA-256 when absent, and the client's empty string read so too); and the `objects`, each an `oid`
//! and a `size`. A verify callback's body ([`read_verify`]) is one such object.
//!
//! **An answer** ([`write_response`]) gives each object, in the order asked, the basic adapter's actions (a `download`;
//! an `upload` and optionally a `verify`), nothing when an upload names an object the server holds ("the server should
//! omit the `actions` property completely"), or an error with one of LFS's codes. An action is an `href`, optional
//! headers and an expiry, `expires_in` seconds (within ±2147483647) or `expires_at`. The answer names the `basic`
//! adapter and `sha256`.
//!
//! **Read as the client writes, and stricter where a server must be.** Unknown fields are skipped, as Go's decoder
//! skips them. A known field repeated is refused (Go keeps the last), so an operation or an object cannot read one way
//! to one reader and another way to the next. An oid is 64 lower-case hexadecimal digits, as git-lfs reads and writes
//! it. A size is an integer from zero to Go's `int64` limit, the client's `Size int64`: a negative one, a fraction, an
//! exponent and anything larger are each refused by kind. A hash other than SHA-256 is refused: no other names an
//! object the server could store, and the server answers it as a validation error.

use super::json::{self, Cursor, Integer, JsonStr, NumberRefusal, Shape};
use super::{Oid, RefProblem, SHA256, Timestamp, is_header_name, is_header_value, read_ref};
use crate::out::Out;
use crate::{Admission, PastAdmission, to_u64};

/// Format: the largest size git-lfs reads, Go's `int64` maximum (`tq/transfer.go`: `Size int64`).
pub const MAX_SIZE: u64 = i64::MAX.unsigned_abs();

/// Format: the bound on `expires_in` either way (batch.md: "Maximum of 2147483647, minimum of -2147483647").
const EXPIRES_IN_BOUND: i64 = 2_147_483_647;

/// Format: the one transfer adapter a server answers with (basic-transfers.md: "All Git LFS clients and servers SHOULD
/// support it").
pub const BASIC: &str = "basic";

/// The operation a batch request asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Operation {
  /// `download`: the client fetches objects.
  Download,
  /// `upload`: the client sends objects.
  Upload,
}

/// A field of a batch request or a verify callback, as a refusal names it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Field {
  /// The body itself, which must be an object.
  Body,
  /// `operation`.
  Operation,
  /// `transfers`.
  Transfers,
  /// An element of `transfers`.
  Transfer,
  /// `ref`.
  Ref,
  /// `ref`'s `name`.
  RefName,
  /// `objects`.
  Objects,
  /// An element of `objects`, or a verify callback's body.
  Object,
  /// An object's `oid`.
  Oid,
  /// An object's `size`.
  Size,
  /// `hash_algo`.
  HashAlgo,
}

/// Why a body is not a batch request or a verify callback. Each names where it stood, from the body's first byte.
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
    /// Where the object that lacks it begins.
    at: u64,
  },
  /// A field given twice in one object.
  Repeated {
    /// The field.
    field: Field,
    /// Where the second value begins.
    at: u64,
  },
  /// An `operation` other than `download` and `upload`.
  Operation {
    /// Where its value begins.
    at: u64,
  },
  /// An `oid` that is not 64 lower-case hexadecimal digits.
  Oid {
    /// Where its value begins.
    at: u64,
  },
  /// A `size` below zero.
  NegativeSize {
    /// Where its value begins.
    at: u64,
  },
  /// A `size` with a fraction or an exponent, or past Go's `int64` ([`MAX_SIZE`]).
  Size {
    /// Where its value begins.
    at: u64,
    /// Which.
    kind: NumberRefusal,
  },
  /// A `hash_algo` other than `sha256`.
  HashAlgo {
    /// Where its value begins.
    at: u64,
  },
  /// The lists the request holds would take more than the admission had left; nothing was built.
  PastAdmission(PastAdmission),
}

/// An object a request names.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Object {
  /// Its id.
  pub oid: Oid,
  /// Its size in bytes, at most [`MAX_SIZE`].
  pub size: u64,
}

/// A batch request, its strings borrowed from the body.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Request<'a> {
  /// What the client asks to do.
  pub operation: Operation,
  /// The transfer adapters the client has, as it named them; empty when it named none, which means `basic`.
  pub transfers: Vec<JsonStr<'a>>,
  /// The ref the objects belong to, when the client named one.
  pub ref_name: Option<JsonStr<'a>>,
  /// The objects, in the order asked.
  pub objects: Vec<Object>,
}

/// A refusal of the value at `at` as not `field`'s type.
fn type_of(field: Field) -> impl Fn(Shape) -> Refusal {
  move |shape| Refusal::Type {
    field,
    at: shape.at,
  }
}

/// The batch request's fields.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Key {
  /// `operation`.
  Operation,
  /// `transfers`.
  Transfers,
  /// `ref`.
  Ref,
  /// `objects`.
  Objects,
  /// `hash_algo`.
  HashAlgo,
}

/// Format: each field of a batch request by its key [B: batch.md "Requests"].
const REQUEST_KEYS: [(&str, Key); 5] = [
  ("operation", Key::Operation),
  ("transfers", Key::Transfers),
  ("ref", Key::Ref),
  ("objects", Key::Objects),
  ("hash_algo", Key::HashAlgo),
];

/// The fields a request has given, so a second of one is refused.
#[derive(Default)]
struct Seen {
  /// The fields seen, by [`Key`].
  keys: [bool; REQUEST_KEYS.len()],
}

impl Seen {
  /// `key`'s place among [`REQUEST_KEYS`].
  fn slot(key: Key) -> usize {
    REQUEST_KEYS
      .iter()
      .position(|(_, known)| *known == key)
      .unwrap_or(0)
  }

  /// Whether `key` was given.
  fn has(&self, key: Key) -> bool {
    self.keys.get(Self::slot(key)).copied().unwrap_or(false)
  }

  /// Marks `key` given, refusing it as `field` at `at` when it was already.
  fn mark(&mut self, key: Key, field: Field, at: u64) -> Result<(), Refusal> {
    if self.has(key) {
      return Err(Refusal::Repeated { field, at });
    }
    if let Some(given) = self.keys.get_mut(Self::slot(key)) {
      *given = true;
    }
    Ok(())
  }
}

/// Reads a batch request: validates the body as JSON, charging its nesting to `admission`, then reads the request's
/// fields, charging the exact size of its lists before building them.
pub fn read_request<'a>(body: &'a [u8], admission: &mut Admission) -> Result<Request<'a>, Refusal> {
  json::validate(body, admission).map_err(Refusal::Json)?;
  let mut cursor = Cursor::new(body);
  let start = cursor.position();
  cursor.begin_object().map_err(type_of(Field::Body))?;
  let mut seen = Seen::default();
  let mut request = Request {
    operation: Operation::Download,
    transfers: Vec::new(),
    ref_name: None,
    objects: Vec::new(),
  };
  while let Some(name) = cursor.next_key().map_err(type_of(Field::Body))? {
    let at = cursor.position();
    let Some((_, key)) = REQUEST_KEYS.iter().find(|(text, _)| name.is(text)) else {
      cursor.skip_value().map_err(type_of(Field::Body))?;
      continue;
    };
    read_field(*key, &mut cursor, &mut request, &mut seen, at, admission)?;
  }
  for (key, field) in [
    (Key::Operation, Field::Operation),
    (Key::Objects, Field::Objects),
  ] {
    if !seen.has(key) {
      return Err(Refusal::Missing { field, at: start });
    }
  }
  Ok(request)
}

/// Reads the value of the request's field `key`, at `at`, into `request`.
fn read_field<'a>(
  key: Key,
  cursor: &mut Cursor<'a>,
  request: &mut Request<'a>,
  seen: &mut Seen,
  at: u64,
  admission: &mut Admission,
) -> Result<(), Refusal> {
  match key {
    Key::Operation => {
      seen.mark(key, Field::Operation, at)?;
      request.operation = read_operation(cursor)?;
    }
    Key::Transfers => {
      seen.mark(key, Field::Transfers, at)?;
      request.transfers = read_transfers(cursor, admission)?;
    }
    Key::Ref => {
      seen.mark(key, Field::Ref, at)?;
      request.ref_name = read_ref(cursor).map_err(ref_refusal)?;
    }
    Key::Objects => {
      seen.mark(key, Field::Objects, at)?;
      request.objects = read_objects(cursor, admission)?;
    }
    Key::HashAlgo => {
      seen.mark(key, Field::HashAlgo, at)?;
      read_hash_algo(cursor)?;
    }
  }
  Ok(())
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

/// Reads `operation`'s value.
fn read_operation(cursor: &mut Cursor<'_>) -> Result<Operation, Refusal> {
  let at = cursor.position();
  let text = cursor.string().map_err(type_of(Field::Operation))?;
  if text.is("download") {
    return Ok(Operation::Download);
  }
  if text.is("upload") {
    return Ok(Operation::Upload);
  }
  Err(Refusal::Operation { at })
}

/// Reads `transfers`' value: an array of strings, or `null` for none. Its length is charged before it is built.
fn read_transfers<'a>(
  cursor: &mut Cursor<'a>,
  admission: &mut Admission,
) -> Result<Vec<JsonStr<'a>>, Refusal> {
  if cursor.null() {
    return Ok(Vec::new());
  }
  let count = cursor.count_elements().map_err(type_of(Field::Transfers))?;
  let mut transfers = reserve::<JsonStr<'a>>(count, admission)?;
  cursor.begin_array().map_err(type_of(Field::Transfers))?;
  while cursor.next_element().map_err(type_of(Field::Transfers))? {
    transfers.push(cursor.string().map_err(type_of(Field::Transfer))?);
  }
  Ok(transfers)
}

/// A list with room for `count` items, charged to `admission` before it is made.
fn reserve<T>(count: usize, admission: &mut Admission) -> Result<Vec<T>, Refusal> {
  admission
    .charge_items::<T>(count)
    .map_err(Refusal::PastAdmission)?;
  let mut list = Vec::new();
  list.try_reserve_exact(count).map_err(|_| {
    Refusal::PastAdmission(PastAdmission {
      asked: to_u64(count.saturating_mul(core::mem::size_of::<T>())),
      left: admission.left(),
    })
  })?;
  Ok(list)
}

/// Reads `objects`' value: an array of objects. Its length is charged before it is built.
fn read_objects(
  cursor: &mut Cursor<'_>,
  admission: &mut Admission,
) -> Result<Vec<Object>, Refusal> {
  let count = cursor.count_elements().map_err(type_of(Field::Objects))?;
  let mut objects = reserve::<Object>(count, admission)?;
  cursor.begin_array().map_err(type_of(Field::Objects))?;
  while cursor.next_element().map_err(type_of(Field::Objects))? {
    objects.push(read_object(cursor)?);
  }
  Ok(objects)
}

/// Reads one object: `oid` and `size` required, anything else skipped.
fn read_object(cursor: &mut Cursor<'_>) -> Result<Object, Refusal> {
  let start = cursor.position();
  cursor.begin_object().map_err(type_of(Field::Object))?;
  let (mut oid, mut size) = (None, None);
  while let Some(name) = cursor.next_key().map_err(type_of(Field::Object))? {
    let at = cursor.position();
    if name.is("oid") {
      if oid.is_some() {
        return Err(Refusal::Repeated {
          field: Field::Oid,
          at,
        });
      }
      oid = Some(read_oid(cursor)?);
    } else if name.is("size") {
      if size.is_some() {
        return Err(Refusal::Repeated {
          field: Field::Size,
          at,
        });
      }
      size = Some(read_size(cursor)?);
    } else {
      cursor.skip_value().map_err(type_of(Field::Object))?;
    }
  }
  let missing = |field| Refusal::Missing { field, at: start };
  Ok(Object {
    oid: oid.ok_or(missing(Field::Oid))?,
    size: size.ok_or(missing(Field::Size))?,
  })
}

/// Reads an `oid`'s value: 64 lower-case hexadecimal digits, escaped or not.
fn read_oid(cursor: &mut Cursor<'_>) -> Result<Oid, Refusal> {
  let at = cursor.position();
  let text = cursor.string().map_err(type_of(Field::Oid))?;
  oid_of(text).ok_or(Refusal::Oid { at })
}

/// The oid a JSON string writes, its escapes resolved.
pub(crate) fn oid_of(text: JsonStr<'_>) -> Option<Oid> {
  if let Some(plain) = text.as_str() {
    return Oid::from_hex(plain.as_bytes());
  }
  let mut digits = [0u8; super::OID_HEX_LEN];
  let mut written = 0usize;
  for named in text.chars() {
    let slot = digits.get_mut(written)?;
    *slot = u8::try_from(u32::from(named)).ok()?;
    written = written.saturating_add(1);
  }
  Oid::from_hex(digits.get(..written)?)
}

/// Reads a `size`'s value: an integer from zero to [`MAX_SIZE`].
fn read_size(cursor: &mut Cursor<'_>) -> Result<u64, Refusal> {
  let at = cursor.position();
  let number = cursor.number().map_err(type_of(Field::Size))?;
  match number.integer() {
    Ok(Integer::NonNegative(size)) if size <= MAX_SIZE => Ok(size),
    Ok(Integer::NonNegative(_)) => Err(Refusal::Size {
      at,
      kind: NumberRefusal::TooLarge,
    }),
    Ok(Integer::Negative(_)) => Err(Refusal::NegativeSize { at }),
    Err(kind) => Err(Refusal::Size { at, kind }),
  }
}

/// Reads `hash_algo`'s value: `sha256`, or `null` or the empty string for the default, which is SHA-256.
fn read_hash_algo(cursor: &mut Cursor<'_>) -> Result<(), Refusal> {
  if cursor.null() {
    return Ok(());
  }
  let at = cursor.position();
  let text = cursor.string().map_err(type_of(Field::HashAlgo))?;
  if text.is(SHA256) || text.is("") {
    return Ok(());
  }
  Err(Refusal::HashAlgo { at })
}

/// Reads a verify callback's body: the object just uploaded, `{"oid": ..., "size": ...}` [B: basic-transfers.md
/// "Verification"].
pub fn read_verify(body: &[u8], admission: &mut Admission) -> Result<Object, Refusal> {
  json::validate(body, admission).map_err(Refusal::Json)?;
  read_object(&mut Cursor::new(body))
}

/// Seconds after the client's own clock at which an action expires, within ±2147483647 (batch.md).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ExpiresIn(i64);

impl ExpiresIn {
  /// `seconds`, or `None` past the bound either way.
  pub fn from_seconds(seconds: i64) -> Option<ExpiresIn> {
    (-EXPIRES_IN_BOUND..=EXPIRES_IN_BOUND)
      .contains(&seconds)
      .then_some(ExpiresIn(seconds))
  }

  /// The seconds.
  pub fn seconds(&self) -> i64 {
    self.0
  }
}

/// What the basic adapter does for one transfer: a URL, the headers to send with it, and when it stops being good.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Action<'a> {
  /// Where the client sends the request.
  pub href: &'a str,
  /// Headers the client adds, as names and values.
  pub header: &'a [(&'a str, &'a str)],
  /// Seconds after the client's clock when it expires; preferred by the client over `expires_at`.
  pub expires_in: Option<ExpiresIn>,
  /// When it expires.
  pub expires_at: Option<Timestamp>,
}

/// A per-object error's code: LFS's, which "should match HTTP status codes where possible" [B: batch.md].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorCode {
  /// 404: "The object does not exist on the server."
  NotFound,
  /// 409: "The specified hash algorithm disagrees with the server's acceptable options."
  HashAlgo,
  /// 410: "The object was removed by the owner."
  Removed,
  /// 422: "Validation error."
  Validation,
}

/// Format: each per-object error code's number [B: batch.md "LFS object error codes"].
const ERROR_CODES: [(ErrorCode, u64); 4] = [
  (ErrorCode::NotFound, 404),
  (ErrorCode::HashAlgo, 409),
  (ErrorCode::Removed, 410),
  (ErrorCode::Validation, 422),
];

impl ErrorCode {
  /// The code's number.
  pub fn number(self) -> u64 {
    ERROR_CODES
      .iter()
      .find(|(code, _)| *code == self)
      .map_or(0, |(_, number)| *number)
  }
}

/// An object that cannot be transferred, and why.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ObjectError<'a> {
  /// The code.
  pub code: ErrorCode,
  /// What went wrong, in words.
  pub message: &'a str,
}

/// What the answer gives one object.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome<'a> {
  /// The server holds the object an upload names: no actions, so the client skips it.
  Held,
  /// Download it with the basic adapter's `GET`.
  Download(Action<'a>),
  /// Upload it with a `PUT`, then confirm it with the verify callback when there is one.
  Upload {
    /// The upload.
    upload: Action<'a>,
    /// The callback after it.
    verify: Option<Action<'a>>,
  },
  /// It cannot be transferred.
  Error(ObjectError<'a>),
}

/// One object's answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Answer<'a> {
  /// Its id.
  pub oid: Oid,
  /// Its size, at most [`MAX_SIZE`].
  pub size: u64,
  /// Whether the actions' requests are authenticated already, so the client asks for no credentials.
  pub authenticated: bool,
  /// What to do.
  pub outcome: Outcome<'a>,
}

/// Why answers cannot be written. Nothing is written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WriteRefusal {
  /// An answer's size past [`MAX_SIZE`], which no client reads.
  Size {
    /// The answer's place.
    answer: u64,
  },
  /// An action's header name that is not an HTTP token.
  HeaderName {
    /// The answer's place.
    answer: u64,
  },
  /// An action's header value with a control character other than a tab, which could end the header's line.
  HeaderValue {
    /// The answer's place.
    answer: u64,
  },
}

/// The actions of an outcome.
fn actions<'o, 'a>(outcome: &'o Outcome<'a>) -> [Option<&'o Action<'a>>; 2] {
  match outcome {
    Outcome::Download(action) => [Some(action), None],
    Outcome::Upload { upload, verify } => [Some(upload), verify.as_ref()],
    Outcome::Held | Outcome::Error(_) => [None, None],
  }
}

/// Checks every answer before any byte is written.
fn check_answers(answers: &[Answer<'_>]) -> Result<(), WriteRefusal> {
  for (place, answer) in answers.iter().enumerate() {
    let answer_at = to_u64(place);
    if answer.size > MAX_SIZE {
      return Err(WriteRefusal::Size { answer: answer_at });
    }
    for action in actions(&answer.outcome).into_iter().flatten() {
      if !action.header.iter().all(|(name, _)| is_header_name(name)) {
        return Err(WriteRefusal::HeaderName { answer: answer_at });
      }
      if !action
        .header
        .iter()
        .all(|(_, value)| is_header_value(value))
      {
        return Err(WriteRefusal::HeaderValue { answer: answer_at });
      }
    }
  }
  Ok(())
}

/// Writes a batch answer, status 200: `{"transfer":"basic","objects":[...],"hash_algo":"sha256"}`, the answers in the
/// order given, every one checked before the first byte is written.
pub fn write_response(answers: &[Answer<'_>], out: &mut impl Out) -> Result<(), WriteRefusal> {
  check_answers(answers)?;
  out.put(b"{\"transfer\":");
  json::put_str(out, BASIC);
  out.put(b",\"objects\":[");
  for (place, answer) in answers.iter().enumerate() {
    if place > 0 {
      out.put(b",");
    }
    put_answer(answer, out);
  }
  out.put(b"],\"hash_algo\":");
  json::put_str(out, SHA256);
  out.put(b"}");
  Ok(())
}

/// Writes one answer.
fn put_answer(answer: &Answer<'_>, out: &mut impl Out) {
  out.put(b"{\"oid\":\"");
  answer.oid.put_hex(out);
  out.put(b"\",\"size\":");
  json::put_u64(out, answer.size);
  if answer.authenticated {
    out.put(b",\"authenticated\":true");
  }
  match &answer.outcome {
    Outcome::Held => {}
    Outcome::Download(action) => {
      out.put(b",\"actions\":{\"download\":");
      put_action(action, out);
      out.put(b"}");
    }
    Outcome::Upload { upload, verify } => {
      out.put(b",\"actions\":{\"upload\":");
      put_action(upload, out);
      if let Some(verify) = verify {
        out.put(b",\"verify\":");
        put_action(verify, out);
      }
      out.put(b"}");
    }
    Outcome::Error(error) => {
      out.put(b",\"error\":{\"code\":");
      json::put_u64(out, error.code.number());
      out.put(b",\"message\":");
      json::put_str(out, error.message);
      out.put(b"}");
    }
  }
  out.put(b"}");
}

/// Writes one action.
fn put_action(action: &Action<'_>, out: &mut impl Out) {
  out.put(b"{\"href\":");
  json::put_str(out, action.href);
  if !action.header.is_empty() {
    out.put(b",\"header\":{");
    for (place, (name, value)) in action.header.iter().enumerate() {
      if place > 0 {
        out.put(b",");
      }
      json::put_str(out, name);
      out.put(b":");
      json::put_str(out, value);
    }
    out.put(b"}");
  }
  if let Some(expires_in) = action.expires_in {
    out.put(b",\"expires_in\":");
    json::put_i64(out, expires_in.seconds());
  }
  if let Some(expires_at) = action.expires_at {
    out.put(b",\"expires_at\":\"");
    expires_at.put_rfc3339(out);
    out.put(b"\"");
  }
  out.put(b"}");
}

#[cfg(test)]
#[path = "../tests/lfs_batch.rs"]
mod tests;
