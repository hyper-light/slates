//! PROPPATCH, LOCK and UNLOCK's tests (A-627): the bodies stock svn 1.15.0 sent for `svn propset --revprop` and `svn
//! lock -m` (recorded on the Mac Studio, 2026-11-19), every cut of each, the property-update extension's removals and
//! encodings, and the two headers as `mod_dav` reads them.

use super::*;
use crate::dav::SVN_PROPS;

/// Bounds no captured body comes near.
const ROOMY: Bounds = Bounds {
  body_bytes: 1 << 16,
  depth: 16,
  name_bytes: 128,
  attributes: 16,
  elements: 1_000,
};

/// `svn propset --revprop -r 5 svn:log "a new message" URL`'s PROPPATCH.
const PROPPATCH: &[u8] = include_bytes!("../fixtures/proppatch-revprop.xml");

/// `svn lock -m "editing it" URL`'s LOCK.
const LOCK: &[u8] = include_bytes!("../fixtures/lock.xml");

/// The name of a property.
fn named(namespace: &str, name: &str) -> PropertyName {
  PropertyName {
    namespace: namespace.to_owned(),
    name: name.to_owned(),
  }
}

/// A-627 (`mod_dav.c` `dav_method_proppatch`): do decode `svn propset --revprop`'s PROPPATCH; expect `svn:log` in
/// Subversion's property namespace set to the new message.
#[test]
fn the_captured_proppatch_sets_the_log_message() {
  let expected = vec![PropChange::Set {
    name: named(SVN_PROPS, "log"),
    value: "a new message".to_owned(),
    base64: false,
  }];
  assert_eq!(proppatch(PROPPATCH, &ROOMY), Ok(expected));
}

/// A-627 (`webdav-protocol`, PROPPATCH's extension): do decode a removal, a set with `V:absent`, and a set in base64;
/// expect two removals and a set marked base64.
#[test]
fn a_removal_and_an_absent_set_are_removals_and_base64_is_marked() {
  let body = b"<D:propertyupdate xmlns:D=\"DAV:\" xmlns:V=\"http://subversion.tigris.org/xmlns/dav/\" \
    xmlns:C=\"http://subversion.tigris.org/xmlns/custom/\"><D:remove><D:prop><C:old/></D:prop></D:remove>\
    <D:set><D:prop><C:gone V:absent=\"1\"/><C:bin V:encoding=\"base64\">/w==</C:bin></D:prop></D:set>\
    </D:propertyupdate>";
  let custom = "http://subversion.tigris.org/xmlns/custom/";
  let expected = vec![
    PropChange::Remove {
      name: named(custom, "old"),
    },
    PropChange::Remove {
      name: named(custom, "gone"),
    },
    PropChange::Set {
      name: named(custom, "bin"),
      value: "/w==".to_owned(),
      base64: true,
    },
  ];
  assert_eq!(proppatch(body, &ROOMY), Ok(expected));
}

/// A-627 (`mod_dav.c` `dav_method_lock`): do decode `svn lock -m`'s LOCK; expect an exclusive write lock whose owner
/// text is the comment.
#[test]
fn the_captured_lock_is_an_exclusive_write_lock_with_its_comment() {
  let expected = LockInfo {
    exclusive: true,
    write: true,
    owner: Some("editing it".to_owned()),
  };
  assert_eq!(lockinfo(LOCK, &ROOMY), Ok(expected));
}

/// A-627 (a body cut anywhere): do decode every proper prefix of both captured bodies; expect each refused.
#[test]
fn every_cut_of_both_captured_bodies_is_refused() {
  let patches = (1..PROPPATCH.len())
    .filter(|&cut| proppatch(&PROPPATCH[..cut], &ROOMY).is_ok())
    .count();
  let locks = (1..LOCK.len())
    .filter(|&cut| lockinfo(&LOCK[..cut], &ROOMY).is_ok())
    .count();
  assert_eq!((patches, locks), (0, 0));
}

/// A-627 (`mod_dav.c` `dav_get_timeout`; the `Lock-Token` header): do read a minute, `Infinite` first, an unreadable
/// value then five seconds, no header, and two lock tokens; expect a minute after the clock, none, five seconds, none,
/// the bracketed token and nothing for an unbracketed one.
#[test]
fn timeouts_and_lock_tokens_read_as_mod_dav_reads_them() {
  let now = AprTime(1_700_000_000_000_000);
  let read = [
    timeout(Some("Second-60"), now),
    timeout(Some("Infinite, Second-4100000000"), now),
    timeout(Some("Second-x, Second-5"), now),
    timeout(None, now),
  ];
  let expected = [
    Some(AprTime(1_700_000_060_000_000)),
    None,
    Some(AprTime(1_700_000_005_000_000)),
    None,
  ];
  assert_eq!(read, expected);
  assert_eq!(
    (
      lock_token("<opaquelocktoken:abc>"),
      lock_token("opaquelocktoken:abc")
    ),
    (Some("opaquelocktoken:abc"), None)
  );
}
