//! PROPPATCH, LOCK and UNLOCK (§4.17.4 piece 13, its second part; A-627): the property update and the lock verbs as
//! `mod_dav` reads them, mapped onto the lock table's guards (A-602's [`LockTable::lock`] with stealing and
//! [`LockTable::unlock`] with breaking), and their answers as `mod_dav` and `mod_dav_svn` write them.
//!
//! A PROPPATCH's `D:propertyupdate` holds `D:set` and `D:remove`, each with a `D:prop` naming properties, in order; a
//! set property's text is its value, base64 when `V:encoding="base64"`, and `V:absent` makes a set a removal (the
//! `webdav-protocol` notes' extension for `svn_ra_change_rev_prop2`).
//!
//! A LOCK, read in httpd 2.4.66's `mod_dav.c` `dav_method_lock` (3178-3358), `util_lock.c` `dav_lock_parse_lockinfo`
//! (170-251) and `util.c` `dav_get_timeout` (542-574), then `mod_dav_svn`'s `lock.c` `open_lockdb` (318-355) and
//! `append_locks` (656-868) at `d434dc14`, in their order ([`lock_request`]):
//! - the body's XML (400, 413 past a bound); `Depth` 0 or `infinity`, `infinity` when absent (400 otherwise); the
//!   parent a directory (409); `X-SVN-Options`' `lock-steal` and `X-SVN-Version-Name`'s revision;
//! - with no body, a refresh, which `mod_dav` answers only for an `If:` header naming the lock's token and a stock
//!   client never sends: refused 400, as `mod_dav` refuses one naming none;
//! - `D:lockinfo` (400 for another root): its children compared by local name alone, as `mod_dav` compares
//!   `child->name`; the first `locktype` naming `write`, the first `lockscope` naming `exclusive` or `shared`, and the
//!   first `owner` are taken, and anything else is 412; `Timeout`'s first word `mod_dav` understands, its seconds read
//!   as `atol` reads them, after the clock's whole second, and never before it (`DAVMinTimeout`, 0 by default);
//! - no principal 501, the path holding nothing 405 (a stock client is "an svn client", which may not lock a
//!   nonexistent path), a type other than write and a scope other than exclusive 400; then the guard.
//!
//! LOCK's answer (`dav_method_lock` 3337-3353, `dav_lock_get_activelock` 42-163, `append_locks` 846-860): 200 with the
//! lock's `activelock` in `D:lockdiscovery`, which gives back the request's depth, its owner element as `mod_dav` kept
//! it (`apr_xml_quote_elem`, then `apr_xml_to_text` with `APR_XML_X2T_FULL_NS_LANG`: every namespace the body declared
//! on it), the seconds left and the token; the headers `X-SVN-Creation-Date`, `X-SVN-Lock-Owner` and `Lock-Token`. A
//! refusal of the guard is answered as `append_locks` answers it: no principal 501 with no body; a lock error
//! (`SVN_ERR_IS_LOCK_ERROR`, no such lock, an expired lock) with FSFS's message, any other error as "Failed to create new
//! lock." (`dav_svn__sanitize_error`), each in `D:error` with its status by `dav_svn__convert_err` (404 for no such
//! path or revision, 423 for a lock held or another principal's, else 500).
//!
//! An UNLOCK (`dav_method_unlock` 3363-3460, `util.c` `dav_validate_resource_state` 899-1420, `lock.c` `get_locks`
//! 432-515 and `remove_lock` 876-978): `Lock-Token` present and bracketed (400 otherwise); the path's unexpired lock
//! (400 when there is none, `svn_fs_get_lock` hiding an expired one) with that token (400 otherwise); an anonymous
//! request that does not break the lock 403 (`get_locks` names the lock's owner as its user then); then the guard.
//! Its answer: 204; a principal's lock another holds 423 with FSFS's message; no principal 501 and any other refusal
//! with "Failed to remove a lock.", each in `D:error` as `remove_lock_svn_output` writes it for an svn client.
//!
//! A PROPFIND's `DAV:lockdiscovery` ([`lockdiscovery`], `props.c` `dav_insert_coreprop` 355-451 over `get_locks` and
//! `svn_lock_to_dav_lock` 62-123): the lock at depth 0, its comment in `D:owner` quoted unless it came as a DAV owner
//! element, and the two `X-SVN-*` headers, which `svn unlock` and `svn info` read.
//!
//! Every client is read as an svn client (`is_svn_client`): the design serves stock Subversion clients alone, so a
//! lock's comment is its owner element's text (`unescape_xml`) and never a DAV owner element. Stricter than `mod_dav`,
//! each stated, since a stock client never sends what is refused: an owner holding an element or an attribute other
//! than `xml:lang` (400); a `Timeout` whose expiry `apr_time_t` cannot hold (400), where `mod_dav`'s arithmetic
//! overflows; an `X-SVN-Version-Name` that is no revision (400), where `atol` reads garbage as 0. A request's header
//! values are taken as httpd hands them on, their surrounding whitespace stripped. An answer whose header value would
//! hold a control character is refused ([`DavRefusal::Unwritable`]), as httpd refuses one (500). A refusal answered
//! with httpd's own status page there is answered with no body here, since no client reads that page.
//!
//! No I/O, no clock, no randomness, no panic.

use std::collections::BTreeMap;

use super::request::PropertyName;
use super::response::svn_time;
use super::xml::{self, Document, Element, quote};
use super::{Bounds, DAV, DavRefusal, SVN_DAV};
use crate::index::NodeKind;
use crate::lock::{
  AprTime, HeadView, Lock, LockRefusal, LockRequest, LockTable, LockTarget, UnlockRequest,
};

/// Microseconds in a second, for a `Second-N` timeout.
/// Format: `apr_time_t` counts microseconds (`APR_USEC_PER_SEC`).
const MICROSECONDS_PER_SECOND: i64 = 1_000_000;

/// The radix `atol` reads a timeout's seconds in.
/// Format: `atol` is `strtol(text, NULL, 10)`.
const DECIMAL: u32 = 10;

/// The XML declaration and `DEBUG_CR` an answer begins with.
/// Format: `mod_dav.h` `DAV_XML_HEADER`, then `DEBUG_CR`, a line feed (httpd 2.4.66).
const XML_HEADER: &str = "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n";

/// The `Content-Type` of every XML answer.
/// Format: `mod_dav.h` `DAV_XML_CONTENT_TYPE`.
pub(crate) const DAV_XML_CONTENT_TYPE: &str = "text/xml; charset=\"utf-8\"";

/// The namespace the `xml` prefix is bound to, which `xml:lang` is in.
/// Format: Namespaces in XML 1.0 §3.
const XML_NAMESPACE: &str = "http://www.w3.org/XML/1998/namespace";

/// The word of `X-SVN-Options` that steals a lock (`svn lock --force`).
/// Format: `svn_dav.h` `SVN_DAV_OPTION_LOCK_STEAL`, found anywhere in the header (`ap_strstr_c`).
const LOCK_STEAL: &str = "lock-steal";

/// The word of `X-SVN-Options` that breaks a lock (`svn unlock --force`).
/// Format: `svn_dav.h` `SVN_DAV_OPTION_LOCK_BREAK`, found anywhere in the header (`ap_strstr_c`).
const LOCK_BREAK: &str = "lock-break";

/// The `mod_dav` error id `mod_dav_svn` gives an anonymous unlock's refusal.
/// Format: `mod_dav.h` `DAV_ERR_LOCK_SAVE_LOCK`, 405.
const DAV_ERR_LOCK_SAVE_LOCK: u32 = 405;

/// HTTP 200, OK.
/// Format: RFC 9110 §15.3.1.
const HTTP_OK: u16 = 200;

/// HTTP 204, No Content: an UNLOCK done.
/// Format: RFC 9110 §15.3.5.
const HTTP_NO_CONTENT: u16 = 204;

/// HTTP 400, Bad Request.
/// Format: RFC 9110 §15.5.1.
const HTTP_BAD_REQUEST: u16 = 400;

/// HTTP 403, Forbidden.
/// Format: RFC 9110 §15.5.4.
const HTTP_FORBIDDEN: u16 = 403;

/// HTTP 404, Not Found.
/// Format: RFC 9110 §15.5.5.
const HTTP_NOT_FOUND: u16 = 404;

/// HTTP 405, Method Not Allowed.
/// Format: RFC 9110 §15.5.6.
const HTTP_METHOD_NOT_ALLOWED: u16 = 405;

/// HTTP 409, Conflict.
/// Format: RFC 9110 §15.5.10.
const HTTP_CONFLICT: u16 = 409;

/// HTTP 412, Precondition Failed.
/// Format: RFC 9110 §15.5.13.
const HTTP_PRECONDITION_FAILED: u16 = 412;

/// HTTP 423, Locked.
/// Format: RFC 4918 §11.3.
const HTTP_LOCKED: u16 = 423;

/// HTTP 500, Internal Server Error.
/// Format: RFC 9110 §15.6.1.
const HTTP_INTERNAL_SERVER_ERROR: u16 = 500;

/// HTTP 501, Not Implemented.
/// Format: RFC 9110 §15.6.2.
const HTTP_NOT_IMPLEMENTED: u16 = 501;

/// `SVN_ERR_FS_NOT_FOUND` and `SVN_ERR_FS_NO_SUCH_REVISION`, which `dav_svn__convert_err` answers 404.
/// Format: `svn_error_codes.h`, the filesystem category's start (160,000) plus 13 and plus 6.
const NOT_FOUND_CODES: [u32; 2] = [160_013, 160_006];

/// `SVN_ERR_FS_LOCK_OWNER_MISMATCH` and `SVN_ERR_FS_PATH_ALREADY_LOCKED`, which `dav_svn__convert_err` answers 423.
/// Format: `svn_error_codes.h`, the filesystem category's start (160,000) plus 39 and plus 35.
const LOCKED_CODES: [u32; 2] = [160_039, 160_035];

/// `SVN_ERR_UNSUPPORTED_FEATURE`, which `dav_svn__convert_err` answers 501.
/// Format: `svn_error_codes.h`, the miscellaneous category's start (200,000) plus 7.
const SVN_ERR_UNSUPPORTED_FEATURE: u32 = 200_007;

/// `SVN_ERR_FS_PROP_BASEVALUE_MISMATCH`, which `dav_svn__convert_err` answers 412.
/// Format: `svn_error_codes.h`, the filesystem category's start (160,000) plus 49.
const SVN_ERR_FS_PROP_BASEVALUE_MISMATCH: u32 = 160_049;

/// What `dav_svn__sanitize_error` puts in place of a lock request's error it does not name.
/// Format: `lock.c` `append_locks` 841.
const LOCK_FAILED: &str = "Failed to create new lock.";

/// What `remove_lock` writes for an unlock's error it does not name.
/// Format: `lock.c` `remove_lock` 966.
const UNLOCK_FAILED: &str = "Failed to remove a lock.";

/// What `remove_lock` answers an anonymous unlock with.
/// Format: `lock.c` `remove_lock` 941.
const ANONYMOUS_UNLOCK: &str = "Anonymous lock removal is not allowed.";

/// One change of a PROPPATCH.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PropChange {
  /// A property set to a value: the text, and whether it is base64.
  Set {
    /// The property.
    name: PropertyName,
    /// Its value as written.
    value: String,
    /// `V:encoding="base64"`.
    base64: bool,
  },
  /// A property removed: under `D:remove`, or set with `V:absent`.
  Remove {
    /// The property.
    name: PropertyName,
  },
}

/// Reads a PROPPATCH body: each change in order.
pub fn proppatch(body: &[u8], bounds: &Bounds) -> Result<Vec<PropChange>, DavRefusal> {
  let root = xml::parse(body, bounds)?;
  if !root.is(DAV, "propertyupdate") {
    return Err(DavRefusal::UnexpectedRoot {
      expected: "propertyupdate",
      namespace: root.namespace,
      name: root.name,
    });
  }
  let mut changes = Vec::new();
  for action in root.elements() {
    let set = action.is(DAV, "set");
    if !set && !action.is(DAV, "remove") {
      continue;
    }
    for prop in action.elements().filter(|prop| prop.is(DAV, "prop")) {
      changes.extend(prop.elements().map(|named| change(named, set)));
    }
  }
  Ok(changes)
}

/// One named property of a `D:set` or a `D:remove`.
fn change(named: &Element, set: bool) -> PropChange {
  let name = PropertyName {
    namespace: named.namespace.clone(),
    name: named.name.clone(),
  };
  let flag = |attribute: &str| {
    named
      .attributes
      .iter()
      .find(|found| found.namespace == SVN_DAV && found.name == attribute)
      .map(|found| found.value.as_str())
  };
  if !set || flag("absent").is_some() {
    return PropChange::Remove { name };
  }
  PropChange::Set {
    name,
    value: named.text(),
    base64: flag("encoding") == Some("base64"),
  }
}

/// The scope a `D:lockscope` names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LockScope {
  /// `D:exclusive`, the one scope Subversion makes.
  Exclusive,
  /// `D:shared`, which `mod_dav_svn` refuses.
  Shared,
}

/// A LOCK's `D:owner`: its text, and the element `mod_dav` keeps to give back.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Owner {
  /// Its text, references resolved: the lock's comment, as `mod_dav_svn` reads it back for an svn client
  /// (`unescape_xml`).
  pub text: String,
  /// The element as `apr_xml_to_text` writes it whole after `apr_xml_quote_elem`, which LOCK's answer gives back.
  pub element: String,
}

/// A LOCK's `D:lockinfo`, as `dav_lock_parse_lockinfo` reads it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LockInfo {
  /// A `D:locktype` named `D:write`, the one type `mod_dav` knows.
  pub write: bool,
  /// The scope a `D:lockscope` named, if it named one `mod_dav` knows.
  pub scope: Option<LockScope>,
  /// The `D:owner`, if the body has one.
  pub owner: Option<Owner>,
}

/// Reads a LOCK body's `D:lockinfo` as `dav_lock_parse_lockinfo` reads it (module documentation, "A LOCK").
pub fn lockinfo(body: &[u8], bounds: &Bounds) -> Result<LockInfo, MethodRefusal> {
  let document =
    xml::parse_document(body, bounds).map_err(|refusal| MethodRefusal::Body(refusal.into()))?;
  read_lockinfo(&document)
}

/// The `D:lockinfo` of a body read whole.
fn read_lockinfo(document: &Document) -> Result<LockInfo, MethodRefusal> {
  let root = &document.root;
  if !root.is(DAV, "lockinfo") {
    return Err(MethodRefusal::Body(DavRefusal::UnexpectedRoot {
      expected: "lockinfo",
      namespace: root.namespace.clone(),
      name: root.name.clone(),
    }));
  }
  let mut info = LockInfo::default();
  for child in root.elements() {
    if !take_lockinfo_child(&mut info, child, document)? {
      return Err(MethodRefusal::UnknownElement {
        name: child.name.clone(),
      });
    }
  }
  Ok(info)
}

/// Takes one child of `D:lockinfo` as `dav_lock_parse_lockinfo` takes it, by its local name: the first `locktype`
/// naming `write`, the first `lockscope` naming `exclusive` or `shared`, the first `owner`. Whether it was taken.
fn take_lockinfo_child(
  info: &mut LockInfo,
  child: &Element,
  document: &Document,
) -> Result<bool, MethodRefusal> {
  let first = child.elements().next().map(|inner| inner.name.as_str());
  if child.name == "locktype" && !info.write && first == Some("write") {
    info.write = true;
    return Ok(true);
  }
  if child.name == "lockscope" && info.scope.is_none() {
    info.scope = match first {
      Some("exclusive") => Some(LockScope::Exclusive),
      Some("shared") => Some(LockScope::Shared),
      _ => None,
    };
    if info.scope.is_some() {
      return Ok(true);
    }
  }
  if child.name == "owner" && info.owner.is_none() {
    info.owner = Some(owner(child, document)?);
    return Ok(true);
  }
  Ok(false)
}

/// A `D:owner`: its text and the element as `apr_xml_to_text` writes it with `APR_XML_X2T_FULL_NS_LANG`
/// (`apr_xml.c` `write_elem`): its name with its namespace's index, its `xml:lang` (its own or its parent's), every
/// namespace the body declared from the last, then its quoted text, or `/>` for none. Refused holding an element or
/// an attribute other than `xml:lang`.
fn owner(element: &Element, document: &Document) -> Result<Owner, MethodRefusal> {
  let attributed = element
    .attributes
    .iter()
    .any(|attribute| !(attribute.namespace == XML_NAMESPACE && attribute.name == "lang"));
  if attributed || element.elements().next().is_some() {
    return Err(MethodRefusal::OwnerMarkup);
  }
  let text = element.text();
  let quoted_namespace = quote(&element.namespace, true);
  let name = document
    .namespaces
    .iter()
    .position(|known| *known == quoted_namespace)
    .filter(|_| !element.namespace.is_empty())
    .map_or_else(
      || element.name.clone(),
      |index| format!("ns{index}:{}", element.name),
    );
  let mut written = format!("<{name}");
  if let Some(lang) = lang(element).or_else(|| lang(&document.root)) {
    written.push_str(&format!(" xml:lang=\"{}\"", quote(lang, true)));
  }
  for (index, namespace) in document.namespaces.iter().enumerate().rev() {
    written.push_str(&format!(" xmlns:ns{index}=\"{namespace}\""));
  }
  if text.is_empty() {
    written.push_str("/>");
  } else {
    written.push_str(&format!(">{}</{name}>", quote(&text, false)));
  }
  Ok(Owner {
    text,
    element: written,
  })
}

/// An element's own `xml:lang`.
fn lang(element: &Element) -> Option<&str> {
  element
    .attributes
    .iter()
    .find(|attribute| attribute.namespace == XML_NAMESPACE && attribute.name == "lang")
    .map(|attribute| attribute.value.as_str())
}

/// When a new lock expires by its `Timeout` header (`dav_get_timeout`, then `dav_method_lock`'s `DAVMinTimeout`
/// bound, 0 by default): the first word that begins `Infinite` is none and the first that begins `Second-` is the
/// clock's whole second plus its seconds as `atol` reads them, never before that second; a word it does not
/// understand is passed over, and none understood is none. An expiry of second 0 is `DAV_TIMEOUT_INFINITE`, so none.
/// Refused when the expiry is past what `apr_time_t` holds.
pub fn timeout(header: Option<&str>, now: AprTime) -> Result<Option<AprTime>, MethodRefusal> {
  let Some(header) = header else {
    return Ok(None);
  };
  for word in header.split(is_apr_space).filter(|word| !word.is_empty()) {
    if word.starts_with("Infinite") {
      return Ok(None);
    }
    if let Some(seconds) = word.strip_prefix("Second-") {
      let refused = || MethodRefusal::Timeout {
        value: header.to_owned(),
      };
      let now_seconds = now
        .0
        .checked_div(MICROSECONDS_PER_SECOND)
        .ok_or_else(refused)?;
      let at = now_seconds.checked_add(atol(seconds)).ok_or_else(refused)?;
      if at == 0 {
        return Ok(None);
      }
      let micros = at
        .max(now_seconds)
        .checked_mul(MICROSECONDS_PER_SECOND)
        .ok_or_else(refused)?;
      return Ok(Some(AprTime(micros)));
    }
  }
  Ok(None)
}

/// Whether `character` is whitespace as `apr_isspace` and `ap_getword_white` read it.
fn is_apr_space(character: char) -> bool {
  matches!(character, ' ' | '\t' | '\n' | '\u{b}' | '\u{c}' | '\r')
}

/// `text` read as `atol` reads it (`strtol` in base 10): whitespace, a sign, then the digits up to the first other
/// character; nothing read is 0, and a value past the type's range is its end.
fn atol(text: &str) -> i64 {
  let text = text.trim_start_matches(is_apr_space);
  let (negative, digits) = match text.strip_prefix('-') {
    Some(rest) => (true, rest),
    None => (false, text.strip_prefix('+').unwrap_or(text)),
  };
  digits
    .chars()
    .map_while(|character| character.to_digit(DECIMAL))
    .fold(0_i64, |value, digit| {
      let value = value.saturating_mul(i64::from(DECIMAL));
      if negative {
        value.saturating_sub(i64::from(digit))
      } else {
        value.saturating_add(i64::from(digit))
      }
    })
}

/// The token of a `Lock-Token` header, `<opaquelocktoken:…>`, its angle brackets taken off.
pub fn lock_token(header: &str) -> Option<&str> {
  header.trim().strip_prefix('<')?.strip_suffix('>')
}

/// Why a LOCK or an UNLOCK is refused before the lock table judges it, each with the status `mod_dav` or `mod_dav_svn`
/// answers ([`MethodRefusal::http_status`]). Each is answered with no body ([`MethodRefusal::answer`]): `mod_dav`
/// answers each with httpd's own status page, which no client reads.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MethodRefusal {
  /// A body that is not the XML a LOCK takes, or whose root is not `D:lockinfo` ("The request body contains an
  /// unexpected XML root element.").
  Body(DavRefusal),
  /// A `D:lockinfo` child `dav_lock_parse_lockinfo` does not take (412, "The server cannot satisfy the LOCK request due
  /// to an unknown XML element").
  UnknownElement {
    /// Its local name.
    name: String,
  },
  /// A `D:owner` holding an element or an attribute other than `xml:lang` (400; stricter than `mod_dav`).
  OwnerMarkup,
  /// A `Depth` other than `0` or `infinity` (400, "Depth must be 0 or \"infinity\" for LOCK.").
  Depth {
    /// The header's value.
    value: String,
  },
  /// A `Timeout` whose expiry `apr_time_t` cannot hold (400; stricter than `mod_dav`).
  Timeout {
    /// The header's value.
    value: String,
  },
  /// An `X-SVN-Version-Name` that is not a revision (400; stricter than `mod_dav_svn`).
  VersionName {
    /// The header's value.
    value: String,
  },
  /// A LOCK with no body: a refresh, which needs an `If:` header naming the lock's token (400, as `mod_dav` refuses
  /// one naming none).
  Refresh,
  /// The path's parent is not a directory (409, "The parent resource of %s does not exist or is not a collection.").
  ParentMissing {
    /// The path.
    path: String,
  },
  /// A lock request with no principal (501, "Anonymous lock creation is not allowed.").
  Anonymous,
  /// A lock request for a path that holds nothing (405, "Subversion clients may not lock nonexistent paths.").
  Nonexistent {
    /// The path.
    path: String,
  },
  /// A lock type other than write (400, "Only 'write' locks are supported.").
  NotWrite,
  /// A lock scope other than exclusive (400, "Only exclusive locks are supported.").
  NotExclusive,
  /// An UNLOCK with no `Lock-Token` (400).
  NoLockToken,
  /// An UNLOCK whose `Lock-Token` is not bracketed (400).
  MalformedLockToken {
    /// The header's value.
    header: String,
  },
  /// An UNLOCK of a path with no unexpired lock (400, "… this resource has no outstanding locks.").
  NoOutstandingLock {
    /// The path.
    path: String,
  },
  /// An UNLOCK whose token is not the lock's (400, "… did not specify one of this resource's locktoken(s).").
  TokenMismatch {
    /// The path.
    path: String,
  },
  /// An anonymous UNLOCK that does not break the lock (403, "User \"[none]\" submitted a locktoken created by user
  /// \"%s\".").
  AnonymousUnlock {
    /// The lock's owner.
    owner: String,
  },
}

impl MethodRefusal {
  /// The HTTP status the request is answered with.
  pub fn http_status(&self) -> u16 {
    match self {
      Self::Body(refusal) => refusal.http_status(),
      Self::UnknownElement { .. } => HTTP_PRECONDITION_FAILED,
      Self::ParentMissing { .. } => HTTP_CONFLICT,
      Self::Anonymous => HTTP_NOT_IMPLEMENTED,
      Self::Nonexistent { .. } => HTTP_METHOD_NOT_ALLOWED,
      Self::AnonymousUnlock { .. } => HTTP_FORBIDDEN,
      _ => HTTP_BAD_REQUEST,
    }
  }

  /// The answer: the status, with no header and no body.
  pub fn answer(&self) -> HttpAnswer {
    HttpAnswer {
      status: self.http_status(),
      headers: Vec::new(),
      body: Vec::new(),
    }
  }
}

/// An answer to a LOCK or an UNLOCK: its status, its headers in the order `mod_dav` and `mod_dav_svn` set them, and its
/// body.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HttpAnswer {
  /// The status.
  pub status: u16,
  /// The headers, each a name and its value.
  pub headers: Vec<(&'static str, String)>,
  /// The body; empty for none.
  pub body: Vec<u8>,
}

/// A LOCK as the server received it, its URL resolved to a repository path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LockHttp<'a> {
  /// The repository path the URL names, canonical (`/trunk/f.txt`).
  pub path: &'a str,
  /// The authenticated principal (`r->user`); none for an anonymous request.
  pub user: Option<&'a str>,
  /// The body; empty for none.
  pub body: &'a [u8],
  /// `Depth`.
  pub depth: Option<&'a str>,
  /// `Timeout`.
  pub timeout: Option<&'a str>,
  /// `X-SVN-Options`.
  pub options: Option<&'a str>,
  /// `X-SVN-Version-Name`, the revision of the client's working file.
  pub version_name: Option<&'a str>,
}

/// What a LOCK asks of the lock table, and what its answer gives back of the request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LockPlan {
  /// The lock table's request ([`LockTable::lock`]).
  pub request: LockRequest,
  /// Whether the request's `Depth` was 0; its answer names `infinity` otherwise.
  pub depth_zero: bool,
  /// The request's `D:owner` as `mod_dav` kept it, which its answer gives back.
  pub owner_element: Option<String>,
}

/// Reads a LOCK into the lock table's request, refused as `mod_dav` and `mod_dav_svn` refuse one before the table
/// judges it (module documentation, "A LOCK"), in their order.
pub fn lock_request(
  request: &LockHttp<'_>,
  head: &impl HeadView,
  now: AprTime,
  bounds: &Bounds,
) -> Result<LockPlan, MethodRefusal> {
  let document = if request.body.is_empty() {
    None
  } else {
    Some(
      xml::parse_document(request.body, bounds)
        .map_err(|refusal| MethodRefusal::Body(refusal.into()))?,
    )
  };
  let depth_zero = lock_depth(request.depth)?;
  parent_is_directory(request.path, head)?;
  let current = version_name(request.version_name)?;
  let info = read_lockinfo(&document.ok_or(MethodRefusal::Refresh)?)?;
  let expires = timeout(request.timeout, now)?;
  let user = request.user.ok_or(MethodRefusal::Anonymous)?;
  if head.node(request.path).is_none() {
    return Err(MethodRefusal::Nonexistent {
      path: request.path.to_owned(),
    });
  }
  if !info.write {
    return Err(MethodRefusal::NotWrite);
  }
  if info.scope != Some(LockScope::Exclusive) {
    return Err(MethodRefusal::NotExclusive);
  }
  let target = LockTarget {
    current,
    token: None,
  };
  let lock_request = LockRequest {
    user: Some(user.to_owned()),
    comment: info
      .owner
      .as_ref()
      .map(|owner| owner.text.clone().into_bytes()),
    is_dav_comment: false,
    expires,
    steal: request
      .options
      .is_some_and(|options| options.contains(LOCK_STEAL)),
    targets: BTreeMap::from([(request.path.to_owned(), target)]),
  };
  Ok(LockPlan {
    request: lock_request,
    depth_zero,
    owner_element: info.owner.map(|owner| owner.element),
  })
}

/// A LOCK's `Depth` (`dav_get_depth` with infinity for none): whether it is 0, refused unless 0 or `infinity`.
fn lock_depth(header: Option<&str>) -> Result<bool, MethodRefusal> {
  match header.map(str::trim) {
    None => Ok(false),
    Some(value) if value.eq_ignore_ascii_case("infinity") => Ok(false),
    Some("0") => Ok(true),
    Some(value) => Err(MethodRefusal::Depth {
      value: value.to_owned(),
    }),
  }
}

/// The path's parent, refused unless it is a directory at the head; the root has none to check.
fn parent_is_directory(path: &str, head: &impl HeadView) -> Result<(), MethodRefusal> {
  let Some((parent, _)) = path.rsplit_once('/').filter(|_| path != "/") else {
    return Ok(());
  };
  let parent = if parent.is_empty() { "/" } else { parent };
  match head.node(parent) {
    Some((NodeKind::Directory, _)) => Ok(()),
    _ => Err(MethodRefusal::ParentMissing {
      path: path.to_owned(),
    }),
  }
}

/// `X-SVN-Version-Name`'s revision: none for no header or a negative number, refused unless decimal.
fn version_name(header: Option<&str>) -> Result<Option<u64>, MethodRefusal> {
  let Some(value) = header.map(str::trim) else {
    return Ok(None);
  };
  let number = value
    .parse::<i64>()
    .map_err(|_| MethodRefusal::VersionName {
      value: value.to_owned(),
    })?;
  Ok(u64::try_from(number).ok())
}

/// LOCK's answer to the lock table's decision for its path: the new lock's discovery, or the refusal as `append_locks`
/// answers it (module documentation, "LOCK's answer"). `filesystem` names the repository's filesystem in FSFS's
/// messages, where FSFS writes its directory's path (`svn_fs__identifier`).
pub fn lock_answer(
  plan: &LockPlan,
  decided: Result<&Lock, &LockRefusal>,
  now: AprTime,
  filesystem: &str,
) -> Result<HttpAnswer, DavRefusal> {
  let lock = match decided {
    Ok(lock) => lock,
    Err(refusal) => return Ok(lock_refused(refusal, filesystem)),
  };
  let token = escape_cdata(lock.token.as_str());
  let activelock = activelock(
    plan.depth_zero,
    plan.owner_element.as_deref(),
    lock.expires,
    &token,
    now,
  );
  let body = format!(
    "{XML_HEADER}<D:prop xmlns:D=\"DAV:\">\n<D:lockdiscovery>\n{activelock}\n</D:lockdiscovery>\n</D:prop>"
  );
  Ok(HttpAnswer {
    status: HTTP_OK,
    headers: vec![
      ("X-SVN-Creation-Date", svn_time(lock.created)?),
      (
        "X-SVN-Lock-Owner",
        header_value("X-SVN-Lock-Owner", &lock.owner)?,
      ),
      ("Lock-Token", format!("<{token}>")),
      ("Content-Type", String::from(DAV_XML_CONTENT_TYPE)),
    ],
    body: body.into_bytes(),
  })
}

/// A PROPFIND's `DAV:lockdiscovery` for a path, with the headers `get_locks` sets when it finds a lock.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Discovery {
  /// The property's element, as a multistatus's `D:prop` holds it ([`super::response::Propstat::props`]).
  pub property: String,
  /// `X-SVN-Creation-Date` and `X-SVN-Lock-Owner`, when there is a lock.
  pub headers: Vec<(&'static str, String)>,
}

/// The `DAV:lockdiscovery` of the path whose unexpired lock is `lock` ([`LockTable::get_lock`]), at `now` (module
/// documentation, "A PROPFIND's `DAV:lockdiscovery`").
pub fn lockdiscovery(lock: Option<&Lock>, now: AprTime) -> Result<Discovery, DavRefusal> {
  let Some(lock) = lock else {
    return Ok(Discovery {
      property: String::from("<D:lockdiscovery/>\n"),
      headers: Vec::new(),
    });
  };
  let owner = lock.comment.as_deref().map(|comment| {
    if lock.is_dav_comment {
      comment.to_owned()
    } else {
      format!(
        "<D:owner xmlns:D=\"DAV:\">{}</D:owner>",
        quote(comment, true)
      )
    }
  });
  let token = escape_cdata(lock.token.as_str());
  let activelock = activelock(true, owner.as_deref(), lock.expires, &token, now);
  Ok(Discovery {
    property: format!("<D:lockdiscovery>{activelock}</D:lockdiscovery>\n"),
    headers: vec![
      ("X-SVN-Creation-Date", svn_time(lock.created)?),
      (
        "X-SVN-Lock-Owner",
        header_value("X-SVN-Lock-Owner", &lock.owner)?,
      ),
    ],
  })
}

/// An UNLOCK as the server received it, its URL resolved to a repository path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UnlockHttp<'a> {
  /// The repository path the URL names.
  pub path: &'a str,
  /// The authenticated principal; none for an anonymous request.
  pub user: Option<&'a str>,
  /// `Lock-Token`.
  pub lock_token: Option<&'a str>,
  /// `X-SVN-Options`.
  pub options: Option<&'a str>,
}

/// Reads an UNLOCK into the lock table's request, refused as `dav_method_unlock` and `dav_validate_request` refuse one
/// before `mod_dav_svn` asks the table (module documentation, "An UNLOCK"), in their order.
pub fn unlock_request(
  request: &UnlockHttp<'_>,
  table: &LockTable,
  now: AprTime,
) -> Result<UnlockRequest, MethodRefusal> {
  let header = request.lock_token.ok_or(MethodRefusal::NoLockToken)?;
  let token = lock_token(header).ok_or_else(|| MethodRefusal::MalformedLockToken {
    header: header.to_owned(),
  })?;
  let break_lock = request
    .options
    .is_some_and(|options| options.contains(LOCK_BREAK));
  let Some(lock) = table.get_lock(request.path, now) else {
    return Err(MethodRefusal::NoOutstandingLock {
      path: request.path.to_owned(),
    });
  };
  if lock.token.as_str() != token {
    return Err(MethodRefusal::TokenMismatch {
      path: request.path.to_owned(),
    });
  }
  if request.user.is_none() && !break_lock {
    return Err(MethodRefusal::AnonymousUnlock {
      owner: lock.owner.clone(),
    });
  }
  Ok(UnlockRequest {
    user: request.user.map(str::to_owned),
    break_lock,
    targets: BTreeMap::from([(request.path.to_owned(), Some(token.to_owned()))]),
  })
}

/// UNLOCK's answer to the lock table's decision for its path (module documentation, "An UNLOCK"), as
/// `remove_lock_svn_output` writes it for an svn client; `filesystem` as [`lock_answer`] takes it.
pub fn unlock_answer(
  request: &UnlockRequest,
  decided: Result<(), &LockRefusal>,
  filesystem: &str,
) -> HttpAnswer {
  match decided {
    Ok(()) => HttpAnswer {
      status: HTTP_NO_CONTENT,
      headers: Vec::new(),
      body: Vec::new(),
    },
    Err(refusal) => unlock_refused(request, refusal, filesystem),
  }
}

/// A refusal of the lock table's unlock guard, answered as `remove_lock` and `remove_lock_svn_output` answer it: no
/// principal 501 with `mod_dav`'s error id, another principal's lock with FSFS's message and its status, any other
/// with "Failed to remove a lock." and its status.
fn unlock_refused(request: &UnlockRequest, refusal: &LockRefusal, filesystem: &str) -> HttpAnswer {
  let code = refusal.subversion_code();
  match refusal {
    LockRefusal::NoUser { .. } => error_answer(
      HTTP_NOT_IMPLEMENTED,
      false,
      DAV_ERR_LOCK_SAVE_LOCK,
      ANONYMOUS_UNLOCK,
    ),
    LockRefusal::OwnerMismatch { owner, .. } => {
      let user = request.user.as_deref().unwrap_or_default();
      let message = format!(
        "User '{user}' is trying to use a lock owned by '{owner}' in filesystem '{filesystem}'"
      );
      error_answer(convert_status(code, HTTP_INTERNAL_SERVER_ERROR), true, code, &message)
    }
    _ => error_answer(convert_status(code, HTTP_INTERNAL_SERVER_ERROR), false, code, UNLOCK_FAILED),
  }
}

/// A refusal of the lock table's lock guard, answered as `append_locks` answers it.
fn lock_refused(refusal: &LockRefusal, filesystem: &str) -> HttpAnswer {
  if let LockRefusal::NoUser { .. } = refusal {
    return HttpAnswer {
      status: HTTP_NOT_IMPLEMENTED,
      headers: Vec::new(),
      body: Vec::new(),
    };
  }
  let code = refusal.subversion_code();
  let message =
    lock_error_message(refusal, filesystem).unwrap_or_else(|| String::from(LOCK_FAILED));
  error_answer(convert_status(code, HTTP_INTERNAL_SERVER_ERROR), true, code, &message)
}

/// FSFS's message for a refusal `append_locks` passes on with its own text (`SVN_ERR_IS_LOCK_ERROR`, no such lock):
/// `lock.c` `check_lock` 720-815 and `verify_lock` 611-626, `fs-loader.c` `svn_fs_lock_many` 1841-1868, the messages of
/// `svn_fs_util.h` 133-176. None for the others, which `dav_svn__sanitize_error` gives its own text, and for an expired
/// lock, whose message names a token the refusal does not carry and which the lock guard never gives.
fn lock_error_message(refusal: &LockRefusal, filesystem: &str) -> Option<String> {
  match refusal {
    LockRefusal::AlreadyLocked { path, owner } => Some(format!(
      "Path '{path}' is already locked by user '{owner}' in filesystem '{filesystem}'"
    )),
    LockRefusal::NotFound { path } => Some(format!("Path '{path}' doesn't exist in HEAD revision")),
    LockRefusal::OutOfDate { path } => {
      Some(format!("Lock failed: newer version of '{path}' exists"))
    }
    LockRefusal::NoSuchRevision { revision } => Some(format!("No such revision {revision}")),
    LockRefusal::NotFile { path } => Some(format!(
      "'{path}' is not a file in filesystem '{filesystem}'"
    )),
    LockRefusal::NoSuchLock { path } => Some(format!(
      "No lock on path '{path}' in filesystem '{filesystem}'"
    )),
    LockRefusal::TokenNotPresented { path } => Some(format!(
      "Cannot verify lock on path '{path}'; no matching lock-token available"
    )),
    LockRefusal::MalformedToken { token } => Some(malformed_token_message(token)),
    _ => None,
  }
}

/// `svn_fs_lock_many`'s message for a supplied token it refuses: its scheme, else its first byte that is not
/// printable ASCII.
fn malformed_token_message(token: &str) -> String {
  if !token.starts_with(crate::lock::TOKEN_SCHEME) {
    return format!("Lock token URI '{token}' has bad scheme; expected 'opaquelocktoken'");
  }
  let at = token
    .bytes()
    .position(|byte| !byte.is_ascii() || byte.is_ascii_control())
    .unwrap_or_default();
  format!("Lock token '{token}' is not ASCII or is a control character at byte {at}")
}

/// The status `dav_svn__convert_err` gives a Subversion error (`util.c` 140-157): its own for the codes it maps, else
/// `status`, the one its caller suggests (the lock verbs' 500, a log-report's 400).
pub(crate) fn convert_status(code: u32, status: u16) -> u16 {
  if NOT_FOUND_CODES.contains(&code) {
    HTTP_NOT_FOUND
  } else if code == SVN_ERR_UNSUPPORTED_FEATURE {
    HTTP_NOT_IMPLEMENTED
  } else if LOCKED_CODES.contains(&code) {
    HTTP_LOCKED
  } else if code == SVN_ERR_FS_PROP_BASEVALUE_MISMATCH {
    HTTP_PRECONDITION_FAILED
  } else {
    status
  }
}

/// An answer with a `D:error` body as `dav_error_response_tag` and `dav_svn__error_response_tag` write one: the
/// Subversion error's own element when `tagged`, then its code and message.
pub(crate) fn error_answer(status: u16, tagged: bool, code: u32, message: &str) -> HttpAnswer {
  let element = if tagged {
    " xmlns:C=\"svn:\">\n<C:error/>\n"
  } else {
    ">\n"
  };
  let body = format!(
    "{XML_HEADER}<D:error xmlns:D=\"DAV:\" xmlns:m=\"http://apache.org/dav/xmlns\"{element}\
     <m:human-readable errcode=\"{code}\">\n{}\n</m:human-readable>\n</D:error>\n",
    quote(message, false)
  );
  HttpAnswer {
    status,
    headers: vec![("Content-Type", String::from(DAV_XML_CONTENT_TYPE))],
    body: body.into_bytes(),
  }
}

/// One `D:activelock` as `dav_lock_get_activelock` writes it: a write lock, exclusive, its depth, its owner element as
/// kept, the seconds left, and its token.
fn activelock(
  depth_zero: bool,
  owner: Option<&str>,
  expires: Option<AprTime>,
  token: &str,
  now: AprTime,
) -> String {
  let depth = if depth_zero { "0" } else { "infinity" };
  format!(
    "<D:activelock>\n<D:locktype><D:write/></D:locktype>\n<D:lockscope><D:exclusive/></D:lockscope>\n\
     <D:depth>{depth}</D:depth>\n{}<D:timeout>{}</D:timeout>\n<D:locktoken>\n<D:href>{token}</D:href>\n\
     </D:locktoken>\n</D:activelock>\n",
    owner.unwrap_or_default(),
    timeout_text(expires, now)
  )
}

/// A lock's `D:timeout`: `Infinite` for none (or second 0, `DAV_TIMEOUT_INFINITE`), else the whole seconds left, 0 once
/// it has passed.
fn timeout_text(expires: Option<AprTime>, now: AprTime) -> String {
  let at = expires.and_then(|expires| expires.0.checked_div(MICROSECONDS_PER_SECOND));
  match at {
    None | Some(0) => String::from("Infinite"),
    Some(at) => {
      let now_seconds = now
        .0
        .checked_div(MICROSECONDS_PER_SECOND)
        .unwrap_or_default();
      format!("Second-{}", at.saturating_sub(now_seconds).max(0))
    }
  }
}

/// `text` as `svn_xml_escape_cdata_cstring` escapes it: `&`, `<` and `>`, and a carriage return as a reference.
fn escape_cdata(text: &str) -> String {
  quote(text, false).replace('\r', "&#13;")
}

/// A header's value, refused holding a control character other than tab, as httpd refuses one.
fn header_value(name: &'static str, value: &str) -> Result<String, DavRefusal> {
  if value
    .bytes()
    .any(|byte| byte.is_ascii_control() && byte != b'\t')
  {
    return Err(DavRefusal::Unwritable {
      what: name,
      value: value.to_owned(),
    });
  }
  Ok(value.to_owned())
}

#[cfg(test)]
mod tests;
