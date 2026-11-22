//! A revision property change over HTTP (`mod_dav_svn` `deadprops.c`; main 01a20182's ruling of 2026-11-21 23:24 PST,
//! the HTTPv2 `svn propset --revprop` red): a PROPPATCH of a revision resource, HTTPv2's `!svn/rev/N` or DeltaV v1's
//! baseline `!svn/bln/N` (`svn_ra_serf__change_rev_prop`, `commit.c` 2431), each property it names one
//! `svn_repos_fs_change_rev_prop4` judged as `svnserve` judges ra_svn's (`crate::serve::RevisionPropertyChange`).
//!
//! **The body** (`deadprops.c` `db_store` 504 and `decode_property_value` 461, as `commit.c` `write_prop_xml` 572
//! writes it). A `D:propertyupdate` holds `D:set` and `D:remove`, each a `D:prop` naming properties in Subversion's
//! namespaces (`S:` its own, `C:` the custom one). A set property's text is its new value, base64 where its
//! `V:encoding` says so; one marked `V:old-value-absent` has none: a deletion that names what it expects. Its first
//! child, when that is a `V:old-value`, is the value the client expects the property to hold, read the same way (marked
//! absent: it expects none). A `D:remove` property is a deletion naming no expectation. `mod_dav_svn` refuses
//! `old-value-absent` on a value with no `V:old-value` (`500`); so is it refused here, the body read as malformed.
//!
//! **The answer** a change that succeeds whole: `mod_dav`'s `207` with each property's `200` (`crate::dav::commit`'s,
//! as a node's PROPPATCH is answered).

use super::commit::{base64_decode, property_name, proppatched};
use super::locking::HttpAnswer;
use super::xml::{Element, parse};
use super::{Bounds, DAV, DavRefusal};
use crate::index::Revision;
use crate::serve::RevisionPropertyChange;

/// Format: the attribute naming a value's encoding (`V:encoding`, `SVN_DAV__ENCODING`); its namespace is not compared,
/// as `mod_dav_svn` compares `attr->name` alone.
const ENCODING: &str = "encoding";
/// Format: the attribute marking a value absent (`V:old-value-absent`, `SVN_DAV__OLD_VALUE__ABSENT`).
const OLD_VALUE_ABSENT: &str = "old-value-absent";
/// Format: the element holding the value the client expects (`V:old-value`, `SVN_DAV__OLD_VALUE`); its namespace not
/// compared, as `db_store` compares `first_child->name` alone.
const OLD_VALUE: &str = "old-value";

/// The changes a PROPPATCH of revision `revision`'s resource asks (module documentation), in order.
///
/// # Errors
/// [`DavRefusal::Xml`] for a body that does not parse within `bounds`; [`DavRefusal::Malformed`] for one that is no
/// `D:propertyupdate`, names a property outside Subversion's namespaces, holds a value whose encoding is not base64's
/// or whose base64 does not decode, or marks a value absent with no expectation beside it.
pub fn revision_property_changes(
  revision: Revision,
  body: &[u8],
  bounds: &Bounds,
) -> Result<Vec<RevisionPropertyChange>, DavRefusal> {
  let malformed = || DavRefusal::Malformed {
    element: "propertyupdate",
    text: String::new(),
  };
  let root = parse(body, bounds).map_err(DavRefusal::Xml)?;
  if !root.is(DAV, "propertyupdate") {
    return Err(malformed());
  }
  let mut changes = Vec::new();
  for action in root.elements() {
    let set = action.is(DAV, "set");
    if !set && !action.is(DAV, "remove") {
      continue;
    }
    for prop in action.elements().filter(|element| element.is(DAV, "prop")) {
      for property in prop.elements() {
        let name = property_name(property).ok_or_else(malformed)?;
        let change = if set {
          let value = decoded(property).ok_or_else(malformed)?;
          let expected = match property.elements().next() {
            Some(old) if old.name == OLD_VALUE => Some(decoded(old).ok_or_else(malformed)?),
            _ if value.is_none() => return Err(malformed()),
            _ => None,
          };
          RevisionPropertyChange {
            revision,
            name,
            value,
            expected,
          }
        } else {
          RevisionPropertyChange {
            revision,
            name,
            value: None,
            expected: None,
          }
        };
        changes.push(change);
      }
    }
  }
  Ok(changes)
}

/// An element's value (`decode_property_value`): none where it is marked absent, its text decoded from base64 where its
/// encoding says so, its text as it is where it names none; refused (the outer none) for another encoding or base64
/// that does not decode.
fn decoded(element: &Element) -> Option<Option<Vec<u8>>> {
  if element.attribute(OLD_VALUE_ABSENT).is_some() {
    return Some(None);
  }
  let text = element.text();
  match element.attribute(ENCODING) {
    Some("base64") => base64_decode(&text).map(Some),
    Some(_) => None,
    None => Some(Some(text.into_bytes())),
  }
}

/// The answer to a PROPPATCH of a revision resource whose `changes` all succeeded: `mod_dav`'s `207`, each property's
/// `200`.
pub fn revision_proppatched(changes: &[RevisionPropertyChange]) -> HttpAnswer {
  let names: Vec<(Vec<u8>, Option<Vec<u8>>)> = changes
    .iter()
    .map(|change| (change.name.clone(), change.value.clone()))
    .collect();
  proppatched(&names)
}
