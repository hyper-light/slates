//! The HTTP side's bodies (§4.17.4 item 5 "The wire" and piece 13, its first part; A-609): the XML that stock
//! Subversion clients send over `http://` and `https://` (ra_serf), and that `mod_dav_svn` answers, as codecs.
//!
//! What it holds: a bounded XML reader ([`xml`]), the decoders of the request bodies a server reads ([`request`]:
//! OPTIONS, PROPFIND, MERGE and the REPORTs `update-report`, `log-report`, `dated-rev-report`, `get-locations`,
//! `get-location-segments`, `get-locks-report`, `mergeinfo-report`, `get-deleted-rev-report`, `inherited-props-report`
//! and `replay-report`), and the encoders of the bodies it writes ([`response`]: OPTIONS with its HTTPv2 headers, a
//! multistatus, a MERGE response, and every report response but the two editor drives). Nothing here performs I/O,
//! reads a clock or draws randomness; every refusal is typed ([`DavRefusal`]) and names the HTTP status a client is
//! answered with. `mod_dav_svn` names no Subversion error for any of them: its refusals of a body carry error id 0.
//!
//! The rules, read in Subversion's `1.14.x` branch at `d434dc14` (`docs/wip/research/vcs-servers-subversion.md` §3):
//! - **Each body as `mod_dav_svn` reads it** (`mod_dav_svn/reports/*.c`, `version.c`): a report needs the `svn:`
//!   namespace declared, or it is refused before anything is read; children in another namespace are skipped, and an
//!   unknown `svn:` child is ignored, as the handlers' loops ignore it; a missing required element is refused by kind
//!   ("Not all parameters passed"); a path must be canonical (`dav_svn__test_canonical`: empty, a canonical absolute
//!   path, or a canonical relative path). An unknown report is `501` ("The requested report is unknown").
//! - **Each body as `mod_dav_svn` writes it**, byte for byte where its own code writes the bytes: `DAV_XML_HEADER`,
//!   then `DEBUG_CR`, a line feed in httpd 2.4.66 (`mod_dav.h`: `#if 1`, `#define DEBUG_CR "\n"`), the format strings
//!   of each report, `apr_xml_quote_string`'s escapes, and `svn_base64_encode_string2`'s lines for a value XML cannot
//!   carry. The multistatus is `mod_dav`'s writer (`dav_begin_multistatus`, `dav_send_one_response`) with the live
//!   properties `mod_dav_svn` names `lp1` (`DAV:`) and `lp2` (its own namespace).
//! - **The request bodies are stock svn 1.15's own**: the fixtures under `dav/fixtures` were recorded from svn 1.15.0
//!   (Homebrew, the Mac Studio, 2026-11-19) against a recorded responder, since no `mod_dav_svn` runs there; the live
//!   capture against Apache with `mod_dav_svn` is main's run. `get-deleted-rev-report`'s body, which no command of that
//!   run sent, is ra_serf's own writer's (`get_deleted_rev.c`).
//!
//! Stricter than `mod_dav_svn`, each stated, since a stock client never sends what is refused:
//! - **No DTD, ever, and no entity but XML's five and character references**: a `DOCTYPE` is refused, so no entity can
//!   be declared and none expanded; a billion laughs is refused at its first byte of DTD.
//! - **A revision is decimal**, negative for none; `SVN_STR_TO_REV` is `atol`, which reads garbage as 0.
//! - **A date is the form `svn_time_to_cstring` writes**, `YYYY-MM-DDTHH:MM:SS.uuuuuuZ`.
//! - **UTF-8 only**: a body declaring another encoding is refused, where expat would also read Latin-1 and UTF-16.
//! - **A path holds no `.` or `..` name, and is no URL**: `dav_svn__test_canonical` passes both, and a filesystem path
//!   with either is refused later by `svn_fs__path_valid`; a URL is never what ra_serf sends in a body.
//!
//! The second part of piece 13 (A-627): `list-report` and `file-revs-report` read and answered ([`listing`]); the two
//! answers that are an editor drive, the update-report's and the replay report's, written by one [`drive::Drive`]
//! from the one drive A-616's reporter yields (`report::drive::Call`), which ra_svn's editor writes too; and
//! PROPPATCH read, LOCK and UNLOCK read into the lock table's guards (A-602) and answered, with the
//! `DAV:lockdiscovery` property a PROPFIND reports ([`locking`]). The request heads and bodies are stock svn 1.15.0's
//! own, recorded on the Mac Studio on 2026-11-19. Not written (GAPS): the update-report's resource walk, a pre-1.2
//! client's; a LOCK's refresh, which a stock client never sends; and a generic DAV client's owner element, since every
//! client is read as an svn client.
//!
//! The third part (A-681), its first: [`site`], the read side's resources a request names below a repository's root
//! (a public URL, a revision root's `!svn/rvr`, a baseline collection's `!svn/bc`) answered to OPTIONS, PROPFIND, GET
//! and HEAD from the index, the green and the lock table, each path judged by the principal's compiled read rules; and
//! its second, the `log-report` streamed from A-613's log walk with `mod_dav_svn`'s receivers
//! (`crate::reads::DavLog`). The edge's site in `slates-server` (`vcs_http::svn`) routes and authenticates the
//! requests it answers.
//!
//! No I/O, no clock, no randomness, no panic.

pub mod activity;
pub mod commit;
pub mod drive;
pub mod listing;
pub mod locking;
mod report_file_revs;
mod report_locations;
mod report_mergeinfo;
pub mod request;
pub mod response;
pub mod site;
mod time;
pub mod update;
pub mod xml;

/// The `DAV:` namespace (RFC 4918).
/// Format: the namespace name WebDAV gives its elements.
pub const DAV: &str = "DAV:";

/// Subversion's own namespace for report bodies (`SVN_XML_NAMESPACE`, `svn_xml.h`).
/// Format: `"svn:"`.
pub const SVN: &str = "svn:";

/// `mod_dav_svn`'s namespace for its live properties and capabilities (`SVN_DAV_PROP_NS_DAV`, `svn_dav.h`).
/// Format: `"http://subversion.tigris.org/xmlns/dav/"`.
pub const SVN_DAV: &str = "http://subversion.tigris.org/xmlns/dav/";

/// The namespace of versioned `svn:` properties over WebDAV (`SVN_DAV_PROP_NS_SVN`, `svn_dav.h`).
/// Format: `"http://subversion.tigris.org/xmlns/svn/"`.
pub const SVN_PROPS: &str = "http://subversion.tigris.org/xmlns/svn/";

/// The namespace of every other versioned property over WebDAV (`SVN_DAV_PROP_NS_CUSTOM`, `svn_dav.h`).
/// Format: `"http://subversion.tigris.org/xmlns/custom/"`.
pub const CUSTOM_PROPS: &str = "http://subversion.tigris.org/xmlns/custom/";

/// The bounds a body is read within, every one the caller's: the server derives them from the connection's share of
/// its shard's memory (§4.17.4 item 12) and from what the request can need; none is a number of this module's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Bounds {
  /// The most bytes a body may hold.
  pub body_bytes: usize,
  /// The deepest element nesting.
  pub depth: usize,
  /// The longest name, an element's or an attribute's, in bytes.
  pub name_bytes: usize,
  /// The most attributes one element may carry, namespace declarations among them.
  pub attributes: usize,
  /// The most elements a body may hold.
  pub elements: usize,
}

/// A revision as a body names one: `None` is Subversion's invalid revision, which a body writes as a negative number.
pub type RevisionArg = Option<crate::index::Revision>;

/// Why a body was refused, each answered with an HTTP status ([`DavRefusal::http_status`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DavRefusal {
  /// The XML itself: malformed, hostile, or past a bound ([`xml::XmlRefusal`]).
  Xml(xml::XmlRefusal),
  /// A root element other than the method's (`mod_dav`: "The \"%s\" element was not found").
  UnexpectedRoot {
    /// What the method's root is.
    expected: &'static str,
    /// The root's namespace.
    namespace: String,
    /// The root's local name.
    name: String,
  },
  /// A report whose body declares no `svn:` namespace ("The request does not contain the 'svn:' namespace").
  NoSvnNamespace,
  /// A required element absent ("Not all parameters passed", or the element named).
  Missing {
    /// The element.
    element: &'static str,
  },
  /// An element whose content is not what it must be ("The '%s' element is malformed").
  Malformed {
    /// The element.
    element: &'static str,
    /// What it held.
    text: String,
  },
  /// A relative path out of Subversion's canonical form (`dav_svn__test_canonical`).
  NotCanonical {
    /// The path.
    path: String,
  },
  /// A report this server does not know ("The requested report is unknown").
  UnknownReport {
    /// The report's namespace.
    namespace: String,
    /// The report's local name.
    name: String,
  },
  /// A response that would outgrow the room its caller gave.
  OutputRoom {
    /// The bytes it would take.
    needed: usize,
    /// The room given.
    room: usize,
  },
  /// An editor call the drive's order does not allow where it came (a close with nothing open, a file's call on a
  /// directory, a second root): a fault of the drive's producer, refused before a byte of the call is written.
  DriveOrder {
    /// The call.
    call: &'static str,
  },
  /// An answer whose drive was aborted: its partial bytes are never sent, so the server answers the drive's refusal
  /// whole, as `mod_dav` answers an error met before it has sent a byte (`dav_method_report`).
  Aborted,
  /// A value the server handed a writer that its answer cannot carry as `mod_dav_svn` writes it (a root path a URL
  /// cannot hold as it is, a header value with a line break).
  Unwritable {
    /// What the value is.
    what: &'static str,
    /// The value.
    value: String,
  },
}

/// HTTP 400, Bad Request: every refusal of a body's content.
/// Format: RFC 9110 §15.5.1.
const HTTP_BAD_REQUEST: u16 = 400;

/// HTTP 413, Content Too Large: a body past its bound.
/// Format: RFC 9110 §15.5.14.
const HTTP_CONTENT_TOO_LARGE: u16 = 413;

/// HTTP 500, Internal Server Error: a response past its room, the server's own limit.
/// Format: RFC 9110 §15.6.1.
const HTTP_INTERNAL_SERVER_ERROR: u16 = 500;

/// HTTP 501, Not Implemented: an unknown report, as `mod_dav_svn` answers it.
/// Format: RFC 9110 §15.6.2.
const HTTP_NOT_IMPLEMENTED: u16 = 501;

impl DavRefusal {
  /// The HTTP status the request is answered with.
  pub fn http_status(&self) -> u16 {
    match self {
      Self::Xml(refusal) if refusal.past_a_bound() => HTTP_CONTENT_TOO_LARGE,
      Self::UnknownReport { .. } => HTTP_NOT_IMPLEMENTED,
      Self::OutputRoom { .. }
      | Self::DriveOrder { .. }
      | Self::Aborted
      | Self::Unwritable { .. } => HTTP_INTERNAL_SERVER_ERROR,
      _ => HTTP_BAD_REQUEST,
    }
  }
}

impl From<xml::XmlRefusal> for DavRefusal {
  fn from(refusal: xml::XmlRefusal) -> Self {
    Self::Xml(refusal)
  }
}

#[cfg(test)]
mod tests;
