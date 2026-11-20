//! Subversion's DAV site, its read side's resources (§4.17.4 item 5 "HTTP" and piece 13's third part, part 1; A-681):
//! one request's method and its URL below a repository's root answered from the index (A-553), the green
//! ([`Repository`]) and the lock table (A-602) as `mod_dav` and `mod_dav_svn` answer it, judged by the principal's
//! compiled read rules (A-606, [`may_read`]). It does no I/O: the edge's site (`slates-server`'s `vcs_http::svn`)
//! proves the principal, finds the repository, reads the body and lends what is served here.
//!
//! What stock svn 1.15.0 asks of these resources was recorded on the Mac Studio (2026-11-19, against a recorded
//! responder; `docs/wip/research/vcs-servers-subversion.md` §3): a session opens with three OPTIONS (the activity
//! collection asked for, a chunked probe, then asked again); `svn info` PROPFINDs six properties of the revision root's
//! resource at depth 0 and `DAV:lockdiscovery` of the public URL; `svn cat` and `svn export` PROPFIND `allprop` and
//! `resourcetype` and GET the revision root's resource; `svn ls` PROPFINDs at depth 0 and 1 where the server lists no
//! `list` capability, and sends a `list-report` where it does.
//!
//! **Resources** (`repos.c` `parse_uri`, `parse_baseline_coll_uri`, `prep_regular`). A path below the root not under
//! the special URI ([`SPECIAL`]) is the node at that path in the youngest revision (a public URL). `!svn/rvr/REV/PATH`
//! and `!svn/bc/REV/PATH` are the node at `PATH` in `REV` (a revision root's resource and a baseline collection's,
//! which `mod_dav_svn` treats alike). `!svn` itself and every other special resource it names (`me`, `rev`, `vcc`,
//! `bln`, `ver`, `his`, `wrk`, `act`, `wbl`, `txn`, `txr`, `vtxn`, `vtxr`) is one this part does not serve; any other
//! special path, or a revision that is not a number, is `mod_dav_svn`'s malformed URI: `500` (error 190001). A revision
//! past the youngest is `404`, "No such revision N" (160006), as `dav_svn__convert_err` maps it. Each of these is
//! answered whatever the method.
//!
//! **Methods.**
//! - OPTIONS: `200`, the `DAV` lines and, when the body asks for the activity collection, the HTTPv2 headers
//!   (`version.c` `get_option`, A-609's writer), listing only what this site serves: of `mod_dav_svn`'s capabilities
//!   (each names a report or a commit's form) `log-revprops` alone, and `SVN-Repository-MergeInfo: no`, since a
//!   `mergeinfo-report` is not served; so a stock client lists a directory by PROPFIND and asks no report it would be
//!   refused.
//! - PROPFIND (`mod_dav.c` `dav_method_propfind`, `props.c` `dav_get_allprops` and `dav_get_props`): `207`, one response
//!   for the resource and, at depth 1 on a directory, one for each child the principal may read, in name order; a
//!   collection's href ends in a slash, as `mod_dav_svn`'s walker writes it. `allprop` is the node's own properties
//!   (`deadprops.c`: `svn:` names as `S:`, others as `C:`, a value XML cannot carry in base64), then `resourcetype`, then
//!   `mod_dav_svn`'s live properties in `liveprops.c`'s order, then `supportedlock` for a file and `lockdiscovery`.
//!   A named property is found live, then among the node's own, else listed in a `404 Not Found` propstat under a `gN`
//!   prefix (`dav_xmlns_add_uri`). A missing `Depth` is infinity, refused `403` on a directory with `mod_dav`'s default.
//! - GET and HEAD (`repos.c` `set_headers`, `deliver`): a file's text, streamed by the caller from the green a piece at
//!   a time, with `Cache-Control` (a week for a resource whose URL names its revision, none for a public URL),
//!   `Last-Modified` for a public URL, its `ETag` and `Content-Type` (its valid `svn:mime-type`, else `text/plain`).
//! - REPORT (`mod_dav.c` `dav_method_report`, `version.c` `deliver_report`; part 1's second slice): its body read as
//!   A-609 reads it, then the resource's node, `404` without one; a `log-report` answered `200`, `text/xml`, from
//!   A-613's log walk with `mod_dav_svn`'s receivers (`crate::reads::DavLog`), which the caller streams a slice a reply
//!   run ([`Report`]): a walk refused before its first byte is answered with its error, `dav_svn__convert_err`'s status
//!   and the error in `D:error`, and one refused after it ends the connection. Every other report is `501` in
//!   `mod_dav_svn`'s words for one it does not know ("The requested report is unknown"), until it is served.
//! - Every other method: `501`, `SVN_ERR_UNSUPPORTED_FEATURE` (200007).
//!
//! **Read access** (`mod_authz_svn`'s questions with HTTP, §4.17.4 item 10). A request whose node's path the
//! principal's rules do not let it read is `403`; a directory's children it may not read are left out of a depth-1
//! PROPFIND, as `mod_dav_svn`'s walker leaves them out. With no rules (no authorization file) every path is readable.
//!
//! **Departures, each stated.** An error `mod_dav` answers with Apache's own error page (a missing node, a read
//! refused, a body refused) is answered with its status and no body. `Content-Length` and `Accept-Ranges` are not
//! sent: the edge frames every reply in chunks and serves no range. A GET naming a delta base (`X-SVN-VR-Base`) is
//! answered the full text, which the protocol allows. A directory's GET (its HTML listing) is `501` until the site's
//! browsing is served. `mod_dav`'s `getcontenttype` and `getcontentlanguage` from a subrequest (Apache's MIME
//! configuration) are not added to an `allprop`, and the multistatus does not echo the request's namespaces. A
//! repository's root URL path is written URL-encoded wherever `mod_dav_svn` writes its configured root raw.
//!
//! No I/O, no clock, no randomness, no panic.

#[cfg(test)]
mod tests;

use super::drive::uri_encode;
use super::locking::{
  DAV_XML_CONTENT_TYPE, HttpAnswer, convert_status, error_answer, lockdiscovery,
};
use super::request::{self, PropertyName, PropfindRequest, Report as Asked, svn_time};
use super::response::{self, Body, BulkUpdates, Capability, Httpv2, live_prop};
use super::update::{UpdateWalk, Walked};
use super::xml::{base64_lines, is_xml_safe, quote};
use super::{Bounds, CUSTOM_PROPS, DAV, DavRefusal, SVN_DAV, SVN_PROPS};
use crate::authz::{PrincipalRules, may_read};
use crate::index::{Index, Node, NodeKind, NodeRelation, PropertyList, Revision, Text};
use crate::lock::{AprTime, LockTable};
use crate::reads::{DavLog, DavLogProgress, Repository};
use crate::report::drive::history as copies;

/// The special URI's name: where `mod_dav_svn` keeps every resource that is no node's public URL.
/// Format: `SVNSpecialURI`'s default, `!svn` (`mod_dav_svn.c`).
pub const SPECIAL: &str = "!svn";

/// HTTPv2's `me` resource beneath the special URI, the target of a stock client's update-report
/// (`svn_ra_serf__report_resource`).
/// Format: `mod_dav_svn`'s `me` (`repos.c`).
const ME: &str = "me";

/// The special resources `mod_dav_svn` knows, of which this part serves `rvr` and `bc`.
/// Format: `repos.c` `special_subdirs`.
const SPECIAL_KINDS: [&str; 15] = [
  "ver", "his", "wrk", "act", "vcc", "bc", "bln", "wbl", "me", "rev", "rvr", "txn", "txr", "vtxn",
  "vtxr",
];

/// The special resources that name a node in a revision: a revision root's and a baseline collection's.
/// Format: `repos.c` `parse_revroot_uri`, which parses as `parse_baseline_coll_uri` does.
const REVISION_ROOTS: [&str; 2] = ["rvr", "bc"];

/// The `Cache-Control` of a resource whose URL names its revision: one week.
/// Format: `repos.c` `set_headers`, "Cache resource for one week (specified in seconds)".
const CACHE_A_WEEK: &str = "max-age=604800";

/// The `Cache-Control` of a public URL's resource, which moves with the youngest revision.
/// Format: `repos.c` `set_headers`.
const CACHE_NONE: &str = "max-age=0";

/// A directory's media type without an XSLT index.
/// Format: `liveprops.c` `DAV_PROPID_getcontenttype`, `repos.c` `set_headers`.
const DIRECTORY_TYPE: &str = "text/html; charset=UTF-8";

/// A file's media type without a valid `svn:mime-type`.
/// Format: `liveprops.c` and `repos.c` `set_headers`: "use text/plain".
const PLAIN_TYPE: &str = "text/plain";

/// The property holding a file's media type.
/// Format: `SVN_PROP_MIME_TYPE` (`svn_props.h`).
const MIME_TYPE: &[u8] = b"svn:mime-type";

/// The revision properties a node's live properties read.
/// Format: `SVN_PROP_REVISION_AUTHOR` and `SVN_PROP_REVISION_DATE` (`svn_props.h`).
const AUTHOR: &[u8] = b"svn:author";

/// Format: `SVN_PROP_REVISION_DATE` (`svn_props.h`).
const DATE: &[u8] = b"svn:date";

/// The prefix of a property in Subversion's own namespace.
/// Format: `SVN_PROP_PREFIX` (`svn_props.h`).
const SVN_PREFIX: &str = "svn:";

/// The VCC's name in its URL.
/// Format: `DAV_SVN__DEFAULT_VCC_NAME` (`dav_svn.h`).
const DEFAULT_VCC: &str = "default";

/// `SVN_ERR_FS_NO_SUCH_REVISION`.
/// Format: the filesystem category's start, 160,000, plus 6 (`svn_error_codes.h`).
const SVN_ERR_FS_NO_SUCH_REVISION: u32 = 160_006;

/// `SVN_ERR_FS_CORRUPT`: the green lacks what the index names.
/// Format: the filesystem category's start plus 4 (`svn_error_codes.h`).
const SVN_ERR_FS_CORRUPT: u32 = 160_004;

/// `SVN_ERR_UNSUPPORTED_FEATURE`.
/// Format: the miscellaneous category's start, 200,000, plus 7 (`svn_error_codes.h`).
const SVN_ERR_UNSUPPORTED_FEATURE: u32 = 200_007;

/// Format: `200 OK` (RFC 9110 §15.3.1).
const HTTP_OK: u16 = 200;
/// Format: `207 Multi-Status` (RFC 4918 §11.1).
const HTTP_MULTI_STATUS: u16 = 207;
/// Format: `400 Bad Request` (RFC 9110 §15.5.1).
const HTTP_BAD_REQUEST: u16 = 400;
/// Format: `403 Forbidden` (RFC 9110 §15.5.4).
const HTTP_FORBIDDEN: u16 = 403;
/// Format: `404 Not Found` (RFC 9110 §15.5.5).
const HTTP_NOT_FOUND: u16 = 404;
/// Format: `500 Internal Server Error` (RFC 9110 §15.6.1).
const HTTP_INTERNAL_SERVER_ERROR: u16 = 500;
/// Format: `501 Not Implemented` (RFC 9110 §15.6.2).
const HTTP_NOT_IMPLEMENTED: u16 = 501;

/// The days of the week as RFC 1123 names them, Sunday first.
/// Format: `apr_day_snames`.
const DAY_NAMES: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];

/// The months as RFC 1123 names them.
/// Format: `apr_month_snames`.
const MONTH_NAMES: [&str; 12] = [
  "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// The weekday of the epoch's first day, 1970-01-01, a Thursday, counted from Sunday.
/// Format: the Gregorian calendar.
const EPOCH_WEEKDAY: u64 = 4;

/// Microseconds in a second.
/// Format: `apr_time_t` counts microseconds.
const MICROSECONDS_PER_SECOND: u64 = 1_000_000;

/// The capabilities this site serves, which its OPTIONS answer lists: `log-revprops`, a `log-report`'s revision
/// properties named or all of them, which a client asks for only where it is listed.
/// Format: `version.c` `get_vsn_options`' names (`svn_dav.h`), those whose report or form is served here.
const SERVED_CAPABILITIES: [Capability; 1] = [Capability::LogRevprops];

/// The status a `log-report` refused before its first byte is answered with, where `dav_svn__convert_err` maps the
/// error to none of its own.
/// Format: `reports/log.c` `dav_svn__log_report`, `dav_svn__convert_err(serr, HTTP_BAD_REQUEST, ...)`.
const LOG_REFUSED_STATUS: u16 = HTTP_BAD_REQUEST;

/// The repository as one request is answered from it, lent by the edge's site for the request's step.
pub struct Served<'a, R: ?Sized> {
  /// The index.
  pub index: &'a Index,
  /// The green: texts, property lists and revision properties.
  pub repository: &'a R,
  /// The repository root's URL path, decoded (`/repo`).
  pub root: &'a str,
  /// The repository's UUID.
  pub uuid: &'a str,
  /// Its locks.
  pub locks: &'a LockTable,
  /// The principal's compiled read rules; none where the repository names no authorization file.
  pub rules: Option<&'a PrincipalRules>,
  /// The instant a lock's expiry is judged at.
  pub now: AprTime,
}

/// A request as the site reads it.
#[derive(Clone, Copy, Debug)]
pub struct Request<'a> {
  /// The method.
  pub method: &'a str,
  /// The URL's path below the repository's root, decoded: empty or `/` for the root, `/trunk/a.txt`, or
  /// `/!svn/rvr/5/trunk`.
  pub path: &'a str,
  /// The `Depth` header's value, if one was sent.
  pub depth: Option<&'a str>,
  /// The body, whole.
  pub body: &'a [u8],
}

/// An answer's headers, each a name and its value, in order.
pub type Headers = Vec<(&'static str, String)>;

/// The answer to a request.
#[derive(Debug)]
pub enum Answer {
  /// Its status, headers and body, whole.
  Whole(HttpAnswer),
  /// `200` with `headers` and a file's text, which the caller streams from the green a piece at a time
  /// ([`Repository::text`]); none for a file with no text, whose body is empty.
  Text {
    /// The headers.
    headers: Headers,
    /// The text.
    text: Option<Text>,
  },
  /// A REPORT's answer, which the caller streams a slice at a time ([`Report::resume`]), its status decided by its
  /// first slices.
  Report(Report),
}

/// A REPORT's answer still to write, the repository lent again for each slice (§4.17.4 item 11: long reads are sliced
/// under the shard's budget): a `log-report`'s walk.
#[derive(Debug)]
pub struct Report {
  /// The walk.
  walk: Walk,
}

/// The walk a REPORT's answer is: a `log-report`'s or an `update-report`'s.
#[derive(Debug)]
enum Walk {
  /// A `log-report`'s.
  Log(DavLog),
  /// An `update-report`'s ([`super::update`]).
  Update(Box<UpdateWalk>),
}

/// How far a REPORT's answer is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReportProgress {
  /// More follows: resume it again.
  More,
  /// The answer is whole.
  Done,
  /// Refused before any of it was written: this is the answer instead (`dav_method_report`'s error for a report that
  /// sent nothing).
  Refused(HttpAnswer),
  /// Refused after some of it was written: the connection ends there (`dav_method_report` aborts it).
  Broken,
}

impl Report {
  /// The answer's headers: its content type (`dav_method_report`'s `DAV_XML_CONTENT_TYPE`).
  pub fn headers(&self) -> Headers {
    vec![("Content-Type", String::from(DAV_XML_CONTENT_TYPE))]
  }

  /// Writes the answer's next slice into `out` from `served`, until what it wrote and the steps it took reach `room`,
  /// at least one step, or the answer ends.
  pub fn resume<R: Repository + ?Sized>(
    &mut self,
    served: &Served<'_, R>,
    out: &mut Vec<u8>,
    room: usize,
  ) -> ReportProgress {
    let log = match &mut self.walk {
      Walk::Log(log) => log,
      Walk::Update(walk) => {
        let walked = Walked {
          index: served.index,
          repository: served.repository,
          locks: served.locks,
          now: served.now,
          rules: served.rules,
        };
        return walk.resume(&walked, out, room);
      }
    };
    let progress = log.resume(served.index, served.repository, served.rules, out, room);
    match progress {
      DavLogProgress::More => ReportProgress::More,
      DavLogProgress::Done => ReportProgress::Done,
      DavLogProgress::Refused { .. } if log.begun() => ReportProgress::Broken,
      DavLogProgress::Refused { code, message } => {
        let status = convert_status(code, LOG_REFUSED_STATUS);
        ReportProgress::Refused(error_answer(status, true, code, &message))
      }
    }
  }
}

/// A node's place: its path and the revision it is read in, whether its URL is public (moving with the youngest), and
/// the URL's path, which a multistatus names it by.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Place {
  /// The URL's path, decoded, from the server's root (`/repo/!svn/rvr/5/trunk`).
  url: String,
  /// The repository path, canonical (`/`, `/trunk/a.txt`).
  path: String,
  /// The revision.
  revision: Revision,
  /// Whether the URL is a public one.
  public: bool,
}

/// What a request's URL names.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Resource {
  /// A node's place.
  Node(Place),
  /// HTTPv2's `me` resource (`!svn/me`), which a stock client sends its update-report to.
  Me,
  /// A special resource this part does not serve.
  Unserved,
}

/// The depth a PROPFIND reaches.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Depth {
  /// The resource alone.
  Zero,
  /// The resource and its children.
  One,
  /// Every descendant, which `mod_dav` refuses on a collection.
  Infinity,
}

/// Answers `request` from `served`, every body within `room` bytes, a request body read within `bounds` (the module
/// doc).
pub fn answer<R: Repository + ?Sized>(
  served: &Served<'_, R>,
  request: &Request<'_>,
  bounds: &Bounds,
  room: usize,
) -> Answer {
  let resource = match resolve(served, request.path) {
    Ok(resource) => resource,
    Err(refused) => return Answer::Whole(refused),
  };
  match request.method {
    "OPTIONS" => Answer::Whole(options(served, request, bounds, room)),
    "PROPFIND" => Answer::Whole(propfind(served, &resource, request, bounds, room)),
    "GET" | "HEAD" => get(served, &resource),
    "REPORT" => report(served, &resource, request, bounds, room),
    method => Answer::Whole(unserved_method(method)),
  }
}

/// What `path` names, read as `parse_uri` reads it; refused for a path with a `.` or `..` name (`400`), a malformed
/// special URI (`500`), a revision past the youngest (`404`), and a node the principal may not read (`403`).
fn resolve<R: ?Sized>(served: &Served<'_, R>, path: &str) -> Result<Resource, HttpAnswer> {
  let segments: Vec<&str> = path.split('/').filter(|name| !name.is_empty()).collect();
  if segments.iter().any(|name| *name == "." || *name == "..") {
    return Err(bare(HTTP_BAD_REQUEST));
  }
  let place = match segments.split_first() {
    Some((&first, [me])) if first == SPECIAL && *me == ME => return Ok(Resource::Me),
    Some((&first, rest)) if first == SPECIAL => special(rest)?,
    _ => Some(Place {
      url: String::new(),
      path: joined(&segments),
      revision: served.index.youngest(),
      public: true,
    }),
  };
  let Some(mut place) = place else {
    return Ok(Resource::Unserved);
  };
  place.url = format!("{}{}", served.root.trim_end_matches('/'), joined(&segments));
  if place.revision > served.index.youngest() {
    let message = format!("No such revision {}", place.revision);
    return Err(error_answer(
      HTTP_NOT_FOUND,
      true,
      SVN_ERR_FS_NO_SUCH_REVISION,
      &message,
    ));
  }
  if !may_read(served.rules, &place.path) {
    return Err(bare(HTTP_FORBIDDEN));
  }
  Ok(Resource::Node(place))
}

/// The place a special URI's segments after [`SPECIAL`] name: a revision root's node, none for a special resource this
/// part does not serve, refused as `mod_dav_svn`'s malformed URI otherwise.
fn special(segments: &[&str]) -> Result<Option<Place>, HttpAnswer> {
  let Some((&kind, rest)) = segments.split_first() else {
    return Ok(None);
  };
  if !SPECIAL_KINDS.contains(&kind) {
    return Err(malformed_uri());
  }
  if !REVISION_ROOTS.contains(&kind) {
    return Ok(None);
  }
  let Some((&revision, path)) = rest.split_first() else {
    return Ok(None);
  };
  let revision = decimal(revision).ok_or_else(malformed_uri)?;
  Ok(Some(Place {
    url: String::new(),
    path: joined(path),
    revision,
    public: false,
  }))
}

/// A revision written in decimal digits alone (A-609's stricter reading of `SVN_STR_TO_REV`).
fn decimal(text: &str) -> Option<Revision> {
  if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
    return None;
  }
  text.parse().ok()
}

/// The canonical repository path of `segments`: `/` for none.
fn joined(segments: &[&str]) -> String {
  if segments.is_empty() {
    return String::from("/");
  }
  segments.iter().fold(String::new(), |mut path, name| {
    path.push('/');
    path.push_str(name);
    path
  })
}

/// `mod_dav_svn`'s answer to a special URI it cannot read: `500` with `SVN_ERR_APMOD_MALFORMED_URI` (190001), an error
/// carrying no Subversion tag, so `mod_dav` answers it with Apache's own page (the module doc's departure).
fn malformed_uri() -> HttpAnswer {
  bare(HTTP_INTERNAL_SERVER_ERROR)
}

/// An answer with `status` alone.
fn bare(status: u16) -> HttpAnswer {
  HttpAnswer {
    status,
    headers: Vec::new(),
    body: Vec::new(),
  }
}

/// The answer to a body `refused`: its status alone.
fn refused_body(refused: &DavRefusal) -> HttpAnswer {
  bare(refused.http_status())
}

/// `501` for a method this part does not serve (the module doc).
fn unserved_method(method: &str) -> HttpAnswer {
  let message = if method == "REPORT" {
    String::from("The requested report is unknown")
  } else {
    format!("The method '{method}' is not served here")
  };
  error_answer(
    HTTP_NOT_IMPLEMENTED,
    true,
    SVN_ERR_UNSUPPORTED_FEATURE,
    &message,
  )
}

/// The OPTIONS answer (the module doc): the HTTPv2 facts when the body asks for the activity collection.
fn options<R: ?Sized>(
  served: &Served<'_, R>,
  request: &Request<'_>,
  bounds: &Bounds,
  room: usize,
) -> HttpAnswer {
  let asked = match request::options(request.body, bounds) {
    Ok(asked) => asked.activity_collection_set,
    Err(refused) => return refused_body(&refused),
  };
  let facts = Httpv2 {
    root: uri_encode(served.root).into_owned(),
    special: SPECIAL.to_owned(),
    youngest: Some(served.index.youngest()),
    uuid: Some(served.uuid.to_owned()),
    mergeinfo: Some(false),
    bulk_updates: BulkUpdates::Prefer,
    served: SERVED_CAPABILITIES.to_vec(),
  };
  let mut headers = response::options_headers(&facts, asked);
  headers.push(("Content-Type", String::from(DAV_XML_CONTENT_TYPE)));
  match response::options_body(&facts, asked, room) {
    Ok(body) => HttpAnswer {
      status: HTTP_OK,
      headers,
      body,
    },
    Err(refused) => refused_body(&refused),
  }
}

/// The node at `place`, none where it is absent; refused `500` where the index refuses the question.
fn node_at<'i, R: ?Sized>(
  served: &Served<'i, R>,
  place: &Place,
) -> Result<Option<Node<'i>>, HttpAnswer> {
  served
    .index
    .node(&place.path, place.revision)
    .map_err(|refused| {
      error_answer(
        HTTP_INTERNAL_SERVER_ERROR,
        true,
        refused.subversion_code(),
        "The repository's index refused the request",
      )
    })
}

/// The GET or HEAD answer (the module doc): a file's text and its headers.
fn get<R: Repository + ?Sized>(served: &Served<'_, R>, resource: &Resource) -> Answer {
  let Resource::Node(place) = resource else {
    return Answer::Whole(unserved_method("GET"));
  };
  let node = match node_at(served, place) {
    Ok(Some(node)) => node,
    Ok(None) => return Answer::Whole(bare(HTTP_NOT_FOUND)),
    Err(refused) => return Answer::Whole(refused),
  };
  if node.kind() == NodeKind::Directory {
    return Answer::Whole(error_answer(
      HTTP_NOT_IMPLEMENTED,
      true,
      SVN_ERR_UNSUPPORTED_FEATURE,
      "A directory's listing is not served here",
    ));
  }
  match subject(served, place, &node) {
    Ok(subject) => Answer::Text {
      headers: text_headers(&subject),
      text: node.text(),
    },
    Err(refused) => Answer::Whole(refused),
  }
}

/// The REPORT answer (the module doc): the body read first, then the resource's node; a `log-report` streamed, any
/// other report `501`.
fn report<R: Repository + ?Sized>(
  served: &Served<'_, R>,
  resource: &Resource,
  request: &Request<'_>,
  bounds: &Bounds,
  room: usize,
) -> Answer {
  let asked = match request::report(request.body, bounds) {
    Ok(asked) => asked,
    Err(DavRefusal::UnknownReport { .. }) => return Answer::Whole(unserved_method("REPORT")),
    Err(refused) => return Answer::Whole(refused_body(&refused)),
  };
  if let Asked::Update(update) = &asked {
    let v2 = match resource {
      Resource::Me => true,
      Resource::Node(_) => false,
      Resource::Unserved => return Answer::Whole(unserved_method("REPORT")),
    };
    let youngest = served.index.youngest();
    return match UpdateWalk::new(update, served.root, youngest, served.uuid, v2, room) {
      Ok(walk) => Answer::Report(Report {
        walk: Walk::Update(Box::new(walk)),
      }),
      Err(refused) => Answer::Whole(refused),
    };
  }
  let Resource::Node(place) = resource else {
    return Answer::Whole(unserved_method("REPORT"));
  };
  match node_at(served, place) {
    Ok(Some(_)) => {}
    Ok(None) => return Answer::Whole(bare(HTTP_NOT_FOUND)),
    Err(refused) => return Answer::Whole(refused),
  }
  match asked {
    Asked::Log(log) => match DavLog::new(&log, &place.path) {
      Some(log) => Answer::Report(Report {
        walk: Walk::Log(log),
      }),
      None => Answer::Whole(bare(HTTP_BAD_REQUEST)),
    },
    _ => Answer::Whole(unserved_method("REPORT")),
  }
}

/// A file's GET headers, as `set_headers` sets them (the module doc).
fn text_headers(subject: &Subject<'_>) -> Headers {
  let mut headers = Vec::new();
  let cache = if subject.place.public {
    CACHE_NONE
  } else {
    CACHE_A_WEEK
  };
  headers.push(("Cache-Control", String::from(cache)));
  if subject.place.public
    && let Some(modified) = subject.date.as_deref().and_then(rfc1123)
  {
    headers.push(("Last-Modified", modified));
  }
  headers.push(("ETag", subject.etag()));
  headers.push(("Content-Type", subject.media_type()));
  headers
}

/// What a node's properties are read from: its place, kind, created revision and its author and date, its text, and
/// its own properties in name order.
struct Subject<'p> {
  /// Its place.
  place: &'p Place,
  /// Whether it is a directory.
  collection: bool,
  /// Its created revision.
  created: Revision,
  /// The created revision's author.
  author: Option<Vec<u8>>,
  /// The created revision's date, as stored.
  date: Option<String>,
  /// Its text, for a file that has one.
  text: Option<Text>,
  /// Its own properties, in name order.
  properties: Vec<(Vec<u8>, Vec<u8>)>,
  /// The revision `checked-in` names it in (`dav_svn__get_safe_cr`).
  safe_created: Revision,
}

/// The subject `node` at `place` is; refused `500` where the green lacks a property list the index names.
fn subject<'p, R: Repository + ?Sized>(
  served: &Served<'_, R>,
  place: &'p Place,
  node: &Node<'_>,
) -> Result<Subject<'p>, HttpAnswer> {
  let created = node.created_revision();
  let revision_properties = served
    .repository
    .revision_properties(created)
    .unwrap_or_default();
  let find = |name: &[u8]| {
    revision_properties
      .iter()
      .find(|(found, _)| *found == name)
      .map(|(_, value)| value.to_vec())
  };
  let mut properties = match node.properties() {
    PropertyList::Empty => Vec::new(),
    PropertyList::Held { identity, .. } => served
      .repository
      .properties(&identity)
      .ok_or_else(corrupt)?
      .into_iter()
      .map(|(name, value)| (name.to_vec(), value.to_vec()))
      .collect(),
  };
  properties.sort();
  Ok(Subject {
    place,
    collection: node.kind() == NodeKind::Directory,
    created,
    author: find(AUTHOR),
    date: find(DATE).and_then(|date| String::from_utf8(date).ok()),
    text: node.text(),
    properties,
    safe_created: safe_created(served.index, place, node),
  })
}

/// `500` for a green that lacks what the index names.
fn corrupt() -> HttpAnswer {
  error_answer(
    HTTP_INTERNAL_SERVER_ERROR,
    true,
    SVN_ERR_FS_CORRUPT,
    "The green lacks a property list the index names",
  )
}

/// `dav_svn__get_safe_cr`: the revision of the first location the node's history reports from `place` without
/// crossing a copy (`get_last_history_rev`, `tree.c` `history_prev`), where the same node-revision is at the path;
/// else `place`'s own revision.
fn safe_created(index: &Index, place: &Place, node: &Node<'_>) -> Revision {
  let created = node.created_revision();
  let history = if created == place.revision {
    created
  } else {
    match copies::youngest_copy_root(index, &place.path, place.revision) {
      Ok(Some((copied_in, copy_path)))
        if copied_in > created && at_or_beneath(&place.path, &copy_path) =>
      {
        copied_in
      }
      Ok(_) => created,
      Err(_) => return place.revision,
    }
  };
  match index.relation(
    (place.path.as_str(), place.revision),
    (place.path.as_str(), history),
  ) {
    Ok(NodeRelation::Unchanged) => history,
    _ => place.revision,
  }
}

/// Whether `path` is `ancestor` or beneath it.
fn at_or_beneath(path: &str, ancestor: &str) -> bool {
  path == ancestor
    || ancestor == "/"
    || path
      .strip_prefix(ancestor)
      .is_some_and(|rest| rest.starts_with('/'))
}

impl Subject<'_> {
  /// `dav_svn__getetag`: weak for a directory, the created revision and the path.
  fn etag(&self) -> String {
    let weak = if self.collection { "W/" } else { "" };
    format!(
      "{weak}\"{}/{}\"",
      self.created,
      quote(&self.place.path, true)
    )
  }

  /// The media type a GET gives (`set_headers`): a directory's, else the file's valid `svn:mime-type`, else
  /// `text/plain`.
  fn media_type(&self) -> String {
    self
      .live_media_type()
      .unwrap_or_else(|| String::from(PLAIN_TYPE))
  }

  /// `getcontenttype` (`liveprops.c`): a directory's type, the file's `svn:mime-type`, or `text/plain` without one;
  /// none where its `svn:mime-type` is not valid.
  fn live_media_type(&self) -> Option<String> {
    if self.collection {
      return Some(String::from(DIRECTORY_TYPE));
    }
    match self.own(MIME_TYPE) {
      None => Some(String::from(PLAIN_TYPE)),
      Some(value) => std::str::from_utf8(value)
        .ok()
        .filter(|value| valid_mime_type(value))
        .map(str::to_owned),
    }
  }

  /// The node's own property `name`, if it has one.
  fn own(&self, name: &[u8]) -> Option<&[u8]> {
    self
      .properties
      .iter()
      .find(|(found, _)| found == name)
      .map(|(_, value)| value.as_slice())
  }
}

/// The characters a media type may not hold beside its one slash.
/// Format: `svn_mime_type_validate`'s `tspecials` (RFC 1521).
const MIME_SPECIALS: &[u8] = b"()<>@,;:\\\"/[]?=";

/// `svn_mime_type_validate` (`libsvn_subr/types.c`): the media type, up to the first `;` or space, is not empty, holds
/// its slash, and is ASCII with no control, space or special character beside it; the whole value holds no control
/// character but a tab.
fn valid_mime_type(value: &str) -> bool {
  let bytes = value.as_bytes();
  let media_end = bytes
    .iter()
    .position(|byte| *byte == b';' || *byte == b' ')
    .unwrap_or(bytes.len());
  let media = bytes.get(..media_end).unwrap_or_default();
  let Some(slash) = media.iter().position(|byte| *byte == b'/') else {
    return false;
  };
  let media_ok = media.iter().enumerate().all(|(at, byte)| {
    at == slash
      || (byte.is_ascii()
        && !byte.is_ascii_control()
        && !byte.is_ascii_whitespace()
        && !MIME_SPECIALS.contains(byte))
  });
  media_ok
    && !bytes
      .iter()
      .any(|byte| byte.is_ascii_control() && *byte != b'\t')
}

/// A date as stored (`svn_time_to_cstring`'s form) in RFC 1123's form, `getlastmodified`'s and `Last-Modified`'s
/// (`liveprops.c` `get_last_modified_time`); none for a date in no form `svn_time_from_cstring` reads here.
fn rfc1123(date: &str) -> Option<String> {
  let micros = u64::try_from(svn_time(date)?.0).ok()?;
  let seconds = micros / MICROSECONDS_PER_SECOND;
  let days = seconds / super::time::SECONDS_PER_DAY;
  let (year, month, day) = super::time::civil_from_days(days)?;
  let (hour, minute, second) = super::time::time_of_day(seconds % super::time::SECONDS_PER_DAY);
  let week = u64::try_from(DAY_NAMES.len()).ok()?;
  let weekday = DAY_NAMES.get(usize::try_from(days.saturating_add(EPOCH_WEEKDAY) % week).ok()?)?;
  let month_name = MONTH_NAMES.get(usize::try_from(month.checked_sub(1)?).ok()?)?;
  Some(format!(
    "{weekday}, {day:02} {month_name} {year} {hour:02}:{minute:02}:{second:02} GMT"
  ))
}

/// The PROPFIND answer (the module doc).
fn propfind<R: Repository + ?Sized>(
  served: &Served<'_, R>,
  resource: &Resource,
  request: &Request<'_>,
  bounds: &Bounds,
  room: usize,
) -> HttpAnswer {
  let Resource::Node(place) = resource else {
    return unserved_method("PROPFIND");
  };
  let Some(depth) = depth_of(request.depth) else {
    return bare(HTTP_BAD_REQUEST);
  };
  let asked = match request::propfind(request.body, bounds) {
    Ok(asked) => asked,
    Err(refused) => return refused_body(&refused),
  };
  let node = match node_at(served, place) {
    Ok(Some(node)) => node,
    Ok(None) => return bare(HTTP_NOT_FOUND),
    Err(refused) => return refused,
  };
  let collection = node.kind() == NodeKind::Directory;
  if depth == Depth::Infinity && collection {
    return bare(HTTP_FORBIDDEN);
  }
  match described(served, place, &node, &asked, depth) {
    Ok((responses, headers)) => multistatus(&responses, headers, room),
    Err(refused) => refused,
  }
}

/// The `Depth` header's depth, infinity when absent (`mod_dav`'s `dav_get_depth`); none for another value.
fn depth_of(header: Option<&str>) -> Option<Depth> {
  match header.map(str::trim) {
    None => Some(Depth::Infinity),
    Some("0") => Some(Depth::Zero),
    Some("1") => Some(Depth::One),
    Some(value) if value.eq_ignore_ascii_case("infinity") => Some(Depth::Infinity),
    Some(_) => None,
  }
}

/// One `D:response` of a multistatus: its href, the namespaces declared on it, and its two propstats' properties.
#[derive(Debug, Default)]
struct Described {
  /// The href, URL-encoded.
  href: String,
  /// The live properties' namespaces declared on it, each a prefix and a URI, as they were first used.
  live: Vec<(String, String)>,
  /// The other namespaces declared on it (`dav_xmlns_info`'s: the dead properties' `S`, `C`, `V` and the `gN` of the
  /// properties not found), as they were added.
  namespaces: Vec<(String, String)>,
  /// Whether `namespaces` are declared before `live`, as `dav_get_allprops` gathers them; after, as `dav_get_props`.
  namespaces_first: bool,
  /// The properties found, written.
  found: String,
  /// Whether any was found.
  any_found: bool,
  /// The properties asked for and not found, written.
  missing: String,
}

impl Described {
  /// Declares the live properties' `prefix` for `uri`, once.
  fn declare_live(&mut self, prefix: &str, uri: &str) {
    if !self.live.iter().any(|(declared, _)| declared == prefix) {
      self.live.push((prefix.to_owned(), uri.to_owned()));
    }
  }

  /// Declares `prefix` for `uri` among the other namespaces, once.
  fn declare(&mut self, prefix: &str, uri: &str) {
    if !self
      .namespaces
      .iter()
      .any(|(declared, _)| declared == prefix)
    {
      self.namespaces.push((prefix.to_owned(), uri.to_owned()));
    }
  }

  /// Declares the dead properties' namespaces (`deadprops.c` `db_define_namespaces`).
  fn declare_dead(&mut self) {
    self.declare("S", SVN_PROPS);
    self.declare("C", CUSTOM_PROPS);
    self.declare("V", SVN_DAV);
  }

  /// Every declaration in the order `mod_dav` writes them on the response.
  fn declarations(&self) -> impl Iterator<Item = &(String, String)> {
    let (first, second) = if self.namespaces_first {
      (&self.namespaces, &self.live)
    } else {
      (&self.live, &self.namespaces)
    };
    first.iter().chain(second.iter())
  }

  /// The prefix declared for `uri`, a new `gN` when none is (`dav_xmlns_add_uri`).
  fn prefix_of(&mut self, uri: &str) -> String {
    if let Some((prefix, _)) = self.namespaces.iter().find(|(_, declared)| declared == uri) {
      return prefix.clone();
    }
    let generated = self
      .namespaces
      .iter()
      .filter(|(prefix, _)| prefix.starts_with('g'))
      .count();
    let prefix = format!("g{generated}");
    self.declare(&prefix, uri);
    prefix
  }
}

/// The responses a PROPFIND of `node` at `place` writes at `depth`, and the lock headers its own lock gives.
fn described<R: Repository + ?Sized>(
  served: &Served<'_, R>,
  place: &Place,
  node: &Node<'_>,
  asked: &PropfindRequest,
  depth: Depth,
) -> Result<(Vec<Described>, Headers), HttpAnswer> {
  let target = subject(served, place, node)?;
  let href = href_of(&place.url, target.collection);
  let (first, headers) = one_response(served, &target, asked, href.clone())?;
  let mut responses = vec![first];
  if depth == Depth::One && target.collection {
    for name in children(served, place) {
      let child = Place {
        url: format!("{}/{name}", place.url),
        path: child_path(&place.path, &name),
        revision: place.revision,
        public: place.public,
      };
      let Ok(Some(child_node)) = node_at(served, &child) else {
        continue;
      };
      let child_subject = subject(served, &child, &child_node)?;
      let slash = if child_subject.collection { "/" } else { "" };
      let child_href = format!("{href}{}{slash}", uri_encode(&name));
      responses.push(one_response(served, &child_subject, asked, child_href)?.0);
    }
  }
  Ok((responses, headers))
}

/// The names of the directory at `place` the principal may read, in name order.
fn children<R: Repository + ?Sized>(served: &Served<'_, R>, place: &Place) -> Vec<String> {
  let mut names = served
    .repository
    .entries(&place.path, place.revision)
    .unwrap_or_default();
  names.sort();
  names.retain(|name| may_read(served.rules, &child_path(&place.path, name)));
  names
}

/// The path of `name` in the directory at `parent`.
fn child_path(parent: &str, name: &str) -> String {
  if parent == "/" {
    format!("/{name}")
  } else {
    format!("{parent}/{name}")
  }
}

/// The href of the URL path `url`: URL-encoded, a collection's ending in a slash, as `mod_dav_svn`'s walker writes it.
fn href_of(url: &str, collection: bool) -> String {
  let mut href = uri_encode(url).into_owned();
  if collection && !href.ends_with('/') {
    href.push('/');
  }
  href
}

/// One response for `subject` at `href`, with the lock headers its lock gives.
fn one_response<R: Repository + ?Sized>(
  served: &Served<'_, R>,
  subject: &Subject<'_>,
  asked: &PropfindRequest,
  href: String,
) -> Result<(Described, Headers), HttpAnswer> {
  let mut described = Described {
    href,
    ..Described::default()
  };
  let discovery = lockdiscovery(
    served.locks.get_lock(&subject.place.path, served.now),
    served.now,
  )
  .map_err(|refused| refused_body(&refused))?;
  let lock = (discovery.property, discovery.headers);
  match asked {
    PropfindRequest::AllProp => all_properties(served, subject, &lock.0, false, &mut described),
    PropfindRequest::PropName => all_properties(served, subject, &lock.0, true, &mut described),
    PropfindRequest::Prop(names) => {
      for name in names {
        named_property(served, subject, &lock.0, name, &mut described);
      }
    }
  }
  Ok((described, lock.1))
}

/// Where a live property is written: `mod_dav`'s own (`D:`), or a provider's (`lp1` for `DAV:`, `lp2` for
/// `mod_dav_svn`'s namespace).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Group {
  /// `mod_dav`'s core properties, `D:`.
  Core,
  /// A provider's property in `DAV:`, `lp1`.
  Dav,
  /// A provider's property in `mod_dav_svn`'s namespace, `lp2`.
  Svn,
}

/// The live properties `allprop` writes, each its group and name: `resourcetype` (`mod_dav`'s standard provider), then
/// `mod_dav_svn`'s in `liveprops.c`'s order, then `mod_dav`'s own `supportedlock` and `lockdiscovery`.
/// Format: `liveprops.c` `props[]`, `props.c` `dav_get_allprops`.
const ALL_LIVE: [(Group, &str); 19] = [
  (Group::Dav, "resourcetype"),
  (Group::Dav, "getcontentlength"),
  (Group::Dav, "getcontenttype"),
  (Group::Dav, "getetag"),
  (Group::Dav, "creationdate"),
  (Group::Dav, "getlastmodified"),
  (Group::Dav, "baseline-collection"),
  (Group::Dav, "checked-in"),
  (Group::Dav, "version-controlled-configuration"),
  (Group::Dav, "version-name"),
  (Group::Dav, "creator-displayname"),
  (Group::Dav, "auto-version"),
  (Group::Svn, "baseline-relative-path"),
  (Group::Svn, "md5-checksum"),
  (Group::Svn, "repository-uuid"),
  (Group::Svn, "deadprop-count"),
  (Group::Svn, "sha1-checksum"),
  (Group::Core, "supportedlock"),
  (Group::Core, "lockdiscovery"),
];

/// The prefix and namespace URI a provider's group is declared with.
fn declared_as(group: Group) -> Option<(&'static str, &'static str)> {
  match group {
    Group::Core => None,
    Group::Dav => Some(("lp1", DAV)),
    Group::Svn => Some(("lp2", SVN_DAV)),
  }
}

/// `allprop`, or `propname` when `names_only` (`dav_get_allprops`): the node's own properties, then every live one.
fn all_properties<R: ?Sized>(
  served: &Served<'_, R>,
  subject: &Subject<'_>,
  lock: &str,
  names_only: bool,
  described: &mut Described,
) {
  described.namespaces_first = true;
  described.declare_dead();
  described.declare_live("lp1", DAV);
  described.declare_live("lp2", SVN_DAV);
  for (name, value) in &subject.properties {
    described
      .found
      .push_str(&dead_property(name, value, names_only));
    described.any_found = true;
  }
  for (group, name) in ALL_LIVE {
    if let Some(value) = live_value(served, subject, lock, group, name) {
      let written = if names_only { String::new() } else { value };
      described
        .found
        .push_str(&live_element(group, name, &written));
      described.any_found = true;
    }
  }
}

/// A live property's element as its group writes it, an empty value an empty element.
fn live_element(group: Group, name: &str, value: &str) -> String {
  match group {
    Group::Core if value.is_empty() => format!("<D:{name}/>\n"),
    Group::Core => format!("<D:{name}>{value}</D:{name}>\n"),
    Group::Dav => live_prop(true, name, value),
    Group::Svn => live_prop(false, name, value),
  }
}

/// A node's own property as `deadprops.c` writes it: `svn:` names in `S:`, others in `C:`; an empty value an empty
/// element; a value XML cannot carry in base64.
fn dead_property(name: &[u8], value: &[u8], names_only: bool) -> String {
  let name = String::from_utf8_lossy(name);
  let (prefix, local) = match name.strip_prefix(SVN_PREFIX) {
    Some(local) => ("S", local),
    None => ("C", name.as_ref()),
  };
  if names_only || value.is_empty() {
    return format!("<{prefix}:{local}/>\n");
  }
  match std::str::from_utf8(value)
    .ok()
    .filter(|_| is_xml_safe(value))
  {
    Some(text) => format!(
      "<{prefix}:{local}>{}</{prefix}:{local}>\n",
      quote(text, false).replace('\r', "&#13;")
    ),
    None => format!(
      "<{prefix}:{local} V:encoding=\"base64\">{}</{prefix}:{local}>\n",
      base64_lines(value)
    ),
  }
}

/// One property a PROPFIND names (`dav_get_props`): live, else the node's own, else missing.
fn named_property<R: ?Sized>(
  served: &Served<'_, R>,
  subject: &Subject<'_>,
  lock: &str,
  name: &PropertyName,
  described: &mut Described,
) {
  if let Some(group) = live_group(&name.namespace, &name.name)
    && let Some(value) = live_value(served, subject, lock, group, &name.name)
  {
    if let Some((prefix, uri)) = declared_as(group) {
      described.declare_live(prefix, uri);
    }
    described
      .found
      .push_str(&live_element(group, &name.name, &value));
    described.any_found = true;
    return;
  }
  if let Some(value) = own_named(subject, name) {
    described.declare_dead();
    let full = full_name(name).unwrap_or_default();
    described
      .found
      .push_str(&dead_property(full.as_bytes(), value, false));
    described.any_found = true;
    return;
  }
  let element = if name.namespace.is_empty() {
    format!("<{}/>\n", name.name)
  } else {
    let prefix = described.prefix_of(&name.namespace);
    format!("<{prefix}:{}/>\n", name.name)
  };
  described.missing.push_str(&element);
}

/// The group of the live property `name` in `namespace`, if a provider or `mod_dav` defines one.
fn live_group(namespace: &str, name: &str) -> Option<Group> {
  ALL_LIVE
    .iter()
    .find(|(group, defined)| {
      *defined == name && declared_as(*group).map_or(DAV, |(_, uri)| uri) == namespace
    })
    .map(|(group, _)| *group)
}

/// The Subversion name a requested dead property has (`deadprops.c` `get_value`): `svn:` and the name in Subversion's
/// namespace, the name alone in the custom one; none in any other.
fn full_name(name: &PropertyName) -> Option<String> {
  if name.namespace == SVN_PROPS {
    Some(format!("{SVN_PREFIX}{}", name.name))
  } else if name.namespace == CUSTOM_PROPS {
    Some(name.name.clone())
  } else {
    None
  }
}

/// The node's own property a requested dead property names, if it has it.
fn own_named<'s>(subject: &'s Subject<'_>, name: &PropertyName) -> Option<&'s [u8]> {
  subject.own(full_name(name)?.as_bytes())
}

/// The value of the live property `name` of `group` for `subject`, written as `liveprops.c` and `mod_dav` write it;
/// none where it is not defined for the node (`DAV_PROP_INSERT_NOTDEF`, `NOTSUPP`).
fn live_value<R: ?Sized>(
  served: &Served<'_, R>,
  subject: &Subject<'_>,
  lock: &str,
  group: Group,
  name: &str,
) -> Option<String> {
  match group {
    Group::Core => core_value(subject, lock, name),
    Group::Dav => dav_value(served, subject, name),
    Group::Svn => svn_value(served, subject, name),
  }
}

/// `mod_dav`'s own: `supportedlock` (`lock.c` `get_supportedlock`: a file's exclusive write lock) and
/// `lockdiscovery`.
fn core_value(subject: &Subject<'_>, lock: &str, name: &str) -> Option<String> {
  match name {
    "supportedlock" if !subject.collection => Some(String::from(
      "\n<D:lockentry>\n<D:lockscope><D:exclusive/></D:lockscope>\n<D:locktype><D:write/></D:locktype>\n\
       </D:lockentry>\n",
    )),
    "lockdiscovery" => Some(
      lock
        .strip_prefix("<D:lockdiscovery>")
        .and_then(|rest| rest.strip_suffix("</D:lockdiscovery>\n"))
        .unwrap_or_default()
        .to_owned(),
    ),
    _ => None,
  }
}

/// A provider's property in `DAV:` (`liveprops.c`; `resourcetype` `mod_dav`'s standard provider's).
fn dav_value<R: ?Sized>(
  served: &Served<'_, R>,
  subject: &Subject<'_>,
  name: &str,
) -> Option<String> {
  match name {
    "resourcetype" => Some(String::from(if subject.collection {
      "<D:collection/>"
    } else {
      ""
    })),
    "getcontentlength" if !subject.collection => {
      Some(subject.text.map_or(0, |text| text.size).to_string())
    }
    "getcontenttype" => subject
      .live_media_type()
      .map(|media| quote(&media, false).into_owned()),
    "getetag" => Some(subject.etag()),
    "creationdate" => subject
      .date
      .as_deref()
      .map(|date| quote(date, true).into_owned()),
    "getlastmodified" => subject.date.as_deref().and_then(rfc1123),
    _ => dav_version_value(served, subject, name),
  }
}

/// The DeltaV properties in `DAV:` (`liveprops.c`).
fn dav_version_value<R: ?Sized>(
  served: &Served<'_, R>,
  subject: &Subject<'_>,
  name: &str,
) -> Option<String> {
  let root = uri_encode(served.root);
  match name {
    "checked-in" => Some(format!(
      "<D:href>{}</D:href>",
      quote(
        &format!(
          "{root}/{SPECIAL}/ver/{}{}",
          subject.safe_created,
          uri_encode(&subject.place.path)
        ),
        true
      )
    )),
    "version-controlled-configuration" => Some(format!(
      "<D:href>{root}/{SPECIAL}/vcc/{DEFAULT_VCC}</D:href>"
    )),
    "version-name" => Some(subject.created.to_string()),
    "creator-displayname" => subject.author.as_deref().map(author_value),
    _ => None,
  }
}

/// `dav_svn__fuzzy_escape_author` for an svn client: control characters dropped from an author XML cannot carry, then
/// quoted.
fn author_value(author: &[u8]) -> String {
  let text = String::from_utf8_lossy(author);
  let kept: String = if is_xml_safe(author) {
    text.into_owned()
  } else {
    text
      .chars()
      .filter(|character| !character.is_ascii_control())
      .collect()
  };
  quote(&kept, true).into_owned()
}

/// `mod_dav_svn`'s own properties (`liveprops.c`).
fn svn_value<R: ?Sized>(
  served: &Served<'_, R>,
  subject: &Subject<'_>,
  name: &str,
) -> Option<String> {
  let file_text = (!subject.collection).then_some(subject.text).flatten();
  match name {
    "baseline-relative-path" => Some(
      quote(
        subject.place.path.strip_prefix('/').unwrap_or_default(),
        true,
      )
      .into_owned(),
    ),
    "md5-checksum" if !subject.collection => {
      Some(file_text.map_or_else(|| hex(&EMPTY_MD5), |text| hex(&text.md5)))
    }
    "sha1-checksum" if !subject.collection => {
      Some(file_text.map_or_else(|| hex(&EMPTY_SHA1), |text| hex(&text.sha1)))
    }
    "repository-uuid" => Some(served.uuid.to_owned()),
    "deadprop-count" => Some(String::from(if subject.properties.is_empty() {
      "0"
    } else {
      "1"
    })),
    _ => None,
  }
}

/// The MD5 of no bytes, a file with no text's.
/// Format: RFC 1321 Appendix A.5, `MD5 ("") = d41d8cd98f00b204e9800998ecf8427e`.
const EMPTY_MD5: [u8; 16] = [
  0xd4, 0x1d, 0x8c, 0xd9, 0x8f, 0x00, 0xb2, 0x04, 0xe9, 0x80, 0x09, 0x98, 0xec, 0xf8, 0x42, 0x7e,
];

/// The SHA-1 of no bytes, a file with no text's.
/// Format: RFC 3174 §7.3's empty message, `da39a3ee5e6b4b0d3255bfef95601890afd80709`.
const EMPTY_SHA1: [u8; 20] = [
  0xda, 0x39, 0xa3, 0xee, 0x5e, 0x6b, 0x4b, 0x0d, 0x32, 0x55, 0xbf, 0xef, 0x95, 0x60, 0x18, 0x90,
  0xaf, 0xd8, 0x07, 0x09,
];

/// `bytes` in lowercase hexadecimal, as `svn_checksum_to_cstring` writes a digest.
fn hex(bytes: &[u8]) -> String {
  bytes.iter().fold(String::new(), |mut text, byte| {
    text.push_str(&format!("{byte:02x}"));
    text
  })
}

/// The multistatus of `responses` (`dav_begin_multistatus`, `dav_send_one_response`), `207` with `headers` after its
/// content type.
fn multistatus(responses: &[Described], headers: Headers, room: usize) -> HttpAnswer {
  let mut body = Body::new(room);
  match write_multistatus(responses, &mut body) {
    Ok(()) => {
      let mut all = vec![("Content-Type", String::from(DAV_XML_CONTENT_TYPE))];
      all.extend(headers);
      HttpAnswer {
        status: HTTP_MULTI_STATUS,
        headers: all,
        body: body.into_bytes(),
      }
    }
    Err(refused) => refused_body(&refused),
  }
}

/// Writes the multistatus of `responses` into `body`.
fn write_multistatus(responses: &[Described], body: &mut Body) -> Result<(), DavRefusal> {
  body.push("<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<D:multistatus xmlns:D=\"DAV:\">\n")?;
  for response in responses {
    body.push("<D:response")?;
    for (prefix, uri) in response.declarations() {
      body.push(&format!(" xmlns:{prefix}=\"{}\"", quote(uri, true)))?;
    }
    body.push(&format!(
      ">\n<D:href>{}</D:href>\n",
      quote(&response.href, false)
    ))?;
    if response.any_found || response.missing.is_empty() {
      body.push(&propstat(&response.found, "200 OK"))?;
    }
    if !response.missing.is_empty() {
      body.push(&propstat(&response.missing, "404 Not Found"))?;
    }
    body.push("</D:response>\n")?;
  }
  body.push("</D:multistatus>\n")
}

/// One propstat holding `properties`, with `status`.
fn propstat(properties: &str, status: &str) -> String {
  format!(
    "<D:propstat>\n<D:prop>\n{properties}</D:prop>\n<D:status>HTTP/1.1 {status}</D:status>\n</D:propstat>\n"
  )
}
