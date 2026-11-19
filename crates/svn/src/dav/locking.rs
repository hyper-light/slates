//! PROPPATCH, LOCK and UNLOCK (§4.17.4 piece 13, its second part; A-627): the property update and the lock request as
//! `mod_dav` reads them (`mod_dav.c` `dav_method_proppatch`, `dav_method_lock`, `util_lock.c`), and the two headers a
//! lock carries, `Timeout` (`dav_get_timeout`) and `Lock-Token`, read into what the A-602 lock table takes.
//!
//! A PROPPATCH's `D:propertyupdate` holds `D:set` and `D:remove`, each with a `D:prop` naming properties, in order; a
//! set property's text is its value, base64 when `V:encoding="base64"`, and `V:absent` makes a set a removal (the
//! `webdav-protocol` notes' extension for `svn_ra_change_rev_prop2`). A LOCK's `D:lockinfo` names an exclusive write
//! lock and its owner, whose text is the lock's comment. `Timeout: Second-N` is an expiry N seconds after the owner's
//! clock reading; `Infinite`, or none, is none.
//!
//! No I/O, no clock, no randomness, no panic.

use super::request::PropertyName;
use super::xml::{self, Element};
use super::{Bounds, DAV, DavRefusal, SVN_DAV};
use crate::lock::AprTime;

/// Microseconds in a second, for a `Second-N` timeout.
/// Format: `apr_time_t` counts microseconds.
const MICROSECONDS_PER_SECOND: i64 = 1_000_000;

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

/// A LOCK's `D:lockinfo`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LockInfo {
  /// `D:exclusive` scope; Subversion makes no other.
  pub exclusive: bool,
  /// `D:write` type.
  pub write: bool,
  /// `D:owner`'s text, the lock's comment.
  pub owner: Option<String>,
}

/// Reads a LOCK body; refused unless it is a `D:lockinfo`.
pub fn lockinfo(body: &[u8], bounds: &Bounds) -> Result<LockInfo, DavRefusal> {
  let root = xml::parse(body, bounds)?;
  if !root.is(DAV, "lockinfo") {
    return Err(DavRefusal::UnexpectedRoot {
      expected: "lockinfo",
      namespace: root.namespace,
      name: root.name,
    });
  }
  let holds = |parent: &str, child: &str| {
    root
      .elements()
      .filter(|element| element.is(DAV, parent))
      .any(|element| element.elements().any(|inner| inner.is(DAV, child)))
  };
  Ok(LockInfo {
    exclusive: holds("lockscope", "exclusive"),
    write: holds("locktype", "write"),
    owner: root
      .elements()
      .find(|element| element.is(DAV, "owner"))
      .map(Element::text),
  })
}

/// When a lock expires by its `Timeout` header (`dav_get_timeout`): the first value read, `Second-N` N seconds after
/// `now` and `Infinite` none; no header, or none of its values read, is none.
pub fn timeout(header: Option<&str>, now: AprTime) -> Option<AprTime> {
  let header = header?;
  for value in header.split(',').map(str::trim) {
    if value == "Infinite" {
      return None;
    }
    if let Some(seconds) = value
      .strip_prefix("Second-")
      .and_then(|digits| digits.parse::<i64>().ok())
    {
      return seconds
        .checked_mul(MICROSECONDS_PER_SECOND)
        .and_then(|micros| now.0.checked_add(micros))
        .map(AprTime);
    }
  }
  None
}

/// The token of a `Lock-Token` header, `<opaquelocktoken:…>`, its angle brackets taken off.
pub fn lock_token(header: &str) -> Option<&str> {
  header.trim().strip_prefix('<')?.strip_suffix('>')
}

#[cfg(test)]
mod tests;
