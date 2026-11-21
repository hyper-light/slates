//! DeltaV v1's commit resources and bodies, against `mod_dav_svn` 1.14.x and ra_serf 1.14's own.
#![allow(clippy::unwrap_used)]

use super::{V1Resource, v1_resource, working_baseline_uri, working_uri};
use crate::dav::Bounds;
use crate::dav::request::{CheckoutRequest, checkout};
use crate::dav::response::created;

/// Shape: bounds past every body here.
const BOUNDS: Bounds = Bounds {
  body_bytes: 1 << 16,
  depth: 32,
  name_bytes: 256,
  attributes: 32,
  elements: 1024,
};

/// Shape: the activity id ra_serf makes (`svn_uuid_generate`).
const ACTIVITY: &str = "4c1d2b8a-0f6e-4b52-9c7a-3d2e1f0a9b8c";

/// A-766 part 2 (`repos.c` `parse_activity_uri`, `parse_working_baseline_uri`, `parse_wrk_uri`): do read each v1
/// commit path; expect the activity, the working baseline with its revision, the working resource with its path (the
/// root's `/`), and none for another special resource, a revision that is no number, or a path outside `!svn`.
#[test]
fn deltav_v1_commit_paths_are_read_as_mod_dav_svn_reads_them() {
  fn read(path: &str) -> Option<V1Resource<'_>> {
    let segments: Vec<&str> = path.split('/').filter(|name| !name.is_empty()).collect();
    v1_resource(&segments)
  }
  assert_eq!(
    (
      read(&format!("/!svn/act/{ACTIVITY}")),
      read(&format!("/!svn/wbl/{ACTIVITY}/6")),
      read(&format!("/!svn/wrk/{ACTIVITY}/trunk/a.txt")),
      read(&format!("/!svn/wrk/{ACTIVITY}")),
      read("/!svn/wbl/x/six"),
      read("/!svn/ver/6/trunk"),
      read("/trunk/a.txt"),
    ),
    (
      Some(V1Resource::Activity(ACTIVITY)),
      Some(V1Resource::WorkingBaseline {
        activity: ACTIVITY,
        revision: 6
      }),
      Some(V1Resource::Working {
        activity: ACTIVITY,
        path: String::from("/trunk/a.txt")
      }),
      Some(V1Resource::Working {
        activity: ACTIVITY,
        path: String::from("/")
      }),
      None,
      None,
      None,
    )
  );
}

/// A-766 part 2 (`dav_svn__create_working_resource`): do make the working baseline's and a working resource's URLs
/// below `/repo`; expect `mod_dav_svn`'s paths, a space in a path URI-encoded.
#[test]
fn deltav_v1_working_resources_are_named_as_mod_dav_svn_names_them() {
  assert_eq!(
    (
      working_baseline_uri("/repo", ACTIVITY, 6),
      working_uri("/repo/", ACTIVITY, "/trunk/a b.txt"),
      working_uri("/repo", ACTIVITY, "/"),
    ),
    (
      format!("/repo/!svn/wbl/{ACTIVITY}/6"),
      format!("/repo/!svn/wrk/{ACTIVITY}/trunk/a%20b.txt"),
      format!("/repo/!svn/wrk/{ACTIVITY}/"),
    )
  );
}

/// A-766 part 2 (ra_serf `create_checkout_body`, `mod_dav.c` `dav_method_checkout`): do read the CHECKOUT body ra_serf
/// sends, an empty body, one asking for a new activity, and an activity set naming none; expect the activity's href and
/// `apply-to-version`, nothing asked, `new`, and the last refused.
#[test]
fn deltav_v1_a_checkouts_body_is_read_as_mod_dav_reads_it() {
  let sent = format!(
    "<?xml version=\"1.0\" encoding=\"utf-8\"?><D:checkout xmlns:D=\"DAV:\"><D:activity-set><D:href>/repo/!svn/act/\
     {ACTIVITY}</D:href></D:activity-set><D:apply-to-version/></D:checkout>"
  );
  let new = b"<D:checkout xmlns:D=\"DAV:\"><D:activity-set><D:new/></D:activity-set></D:checkout>";
  let none = b"<D:checkout xmlns:D=\"DAV:\"><D:activity-set/></D:checkout>";
  assert_eq!(
    (
      checkout(sent.as_bytes(), &BOUNDS),
      checkout(b"", &BOUNDS),
      checkout(new, &BOUNDS).map(|asked| asked.new_activity),
      checkout(none, &BOUNDS).is_err(),
    ),
    (
      Ok(CheckoutRequest {
        activities: vec![format!("/repo/!svn/act/{ACTIVITY}")],
        new_activity: false,
        apply_to_version: true,
      }),
      Ok(CheckoutRequest::default()),
      Ok(true),
      true,
    )
  );
}

/// A-766 part 2 (`mod_dav.c` `dav_created`, `dav_error_response`; `mod_dav.h` `DAV_RESPONSE_BODY_1`..`5`): do answer a
/// CHECKOUT's working resource whose path holds a `&`; expect `201`, the absolute `Location`, `Cache-Control:
/// no-cache`, and `mod_dav`'s HTML body byte for byte, the path HTML-escaped.
#[test]
fn deltav_v1_a_created_resource_is_answered_as_mod_dav_answers_it() {
  let answer = created(
    "Checked-out resource",
    "/repo/!svn/wrk/id/a&b",
    "http://host/repo/!svn/wrk/id/a&b",
  );
  let expected = "<!DOCTYPE HTML PUBLIC \"-//W3C//DTD HTML 4.01//EN\" \"http://www.w3.org/TR/html4/strict.dtd\">\n\
<html>\n<head>\n<title>201 Created</title>\n</head><body>\n<h1>Created</h1>\n\
<p>Checked-out resource /repo/!svn/wrk/id/a&amp;b has been created.</p>\n</body></html>\n";
  assert_eq!(
    (
      answer.status,
      answer.headers,
      String::from_utf8(answer.body).unwrap()
    ),
    (
      201,
      vec![
        ("Location", String::from("http://host/repo/!svn/wrk/id/a&b")),
        ("Cache-Control", String::from("no-cache")),
        (
          "Content-Type",
          String::from("text/html; charset=ISO-8859-1")
        ),
      ],
      expected.to_owned()
    )
  );
}
