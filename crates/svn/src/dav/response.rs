//! The response bodies a server writes (§4.17.4 piece 13; A-609), each as `mod_dav` and `mod_dav_svn` write it, every
//! byte their format strings name: `DAV_XML_HEADER`, `DEBUG_CR` (a line feed in httpd 2.4.66), and each report's own
//! strings in `reports/*.c`, `merge.c`, `version.c` `get_option`, and `mod_dav.c`'s multistatus.
//!
//! Every body is written into a [`Body`] within the room its caller gives, refused past it. A value goes out as
//! `mod_dav_svn` sends it: quoted by `apr_xml_quote_string` with the quotes flag its call site passes, or in base64
//! with `encoding="base64"` where it checks `svn_xml_is_xml_safe` first. Where `mod_dav_svn` walks a hash table, the
//! caller's order is written, which a client reads as a set.
//!
//! One departure, stated: a custom revision property's name is quoted with its quotes escaped, where `mod_dav_svn`
//! passes 0 and so writes an attribute a `"` would break.

use super::locking::HttpAnswer;
use super::xml::{base64_lines, fuzzy_escape, is_xml_safe, quote};
use super::{DavRefusal, RevisionArg};
use crate::index::Revision;
use crate::lock::{AprTime, Lock};

/// The XML declaration and `DEBUG_CR` every body begins with.
/// Format: `mod_dav.h` `DAV_XML_HEADER`, then `DEBUG_CR`, a line feed (httpd 2.4.66 `#if 1`).
const XML_HEADER: &str = "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n";

/// Microseconds in a second.
/// Format: `apr_time_t` counts microseconds.
const MICROSECONDS_PER_SECOND: u64 = 1_000_000;

/// A body being written, within its room.
#[derive(Debug)]
pub struct Body {
  /// The bytes so far.
  bytes: Vec<u8>,
  /// The most bytes it may hold.
  room: usize,
}

impl Body {
  /// An empty body that may hold `room` bytes.
  pub fn new(room: usize) -> Self {
    Self {
      bytes: Vec::new(),
      room,
    }
  }

  /// Appends `text`, refused past the room.
  pub fn push(&mut self, text: &str) -> Result<(), DavRefusal> {
    let needed = self.bytes.len().saturating_add(text.len());
    if needed > self.room {
      return Err(DavRefusal::OutputRoom {
        needed,
        room: self.room,
      });
    }
    self.bytes.extend_from_slice(text.as_bytes());
    Ok(())
  }

  /// The bytes it may still take.
  pub fn left(&self) -> usize {
    self.room.saturating_sub(self.bytes.len())
  }

  /// The bytes so far, taken out: the body is empty again and may hold its room anew (a streamed answer's slice).
  pub fn drain(&mut self) -> Vec<u8> {
    std::mem::take(&mut self.bytes)
  }

  /// The bytes written.
  pub fn into_bytes(self) -> Vec<u8> {
    self.bytes
  }
}

/// A time as `svn_time_to_cstring` writes it, `YYYY-MM-DDTHH:MM:SS.uuuuuuZ`; refused before 1970 or past 9999.
pub fn svn_time(time: AprTime) -> Result<String, DavRefusal> {
  let unwritable = || DavRefusal::Malformed {
    element: "date",
    text: time.0.to_string(),
  };
  let micros = u64::try_from(time.0).map_err(|_| unwritable())?;
  let seconds = micros / MICROSECONDS_PER_SECOND;
  let (year, month, day) =
    super::time::civil_from_days(seconds / super::time::SECONDS_PER_DAY).ok_or_else(unwritable)?;
  let (hour, minute, second) = super::time::time_of_day(seconds % super::time::SECONDS_PER_DAY);
  let fraction = micros % MICROSECONDS_PER_SECOND;
  Ok(format!(
    "{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{fraction:06}Z"
  ))
}

/// The HTTPv2 facts an OPTIONS answer gives (`version.c` `get_option`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Httpv2 {
  /// The repository's root path on the server, `SVN-Repository-Root` (`/repo`).
  pub root: String,
  /// The special URI's name (`SVNSpecialURI`, `!svn` by default).
  pub special: String,
  /// `SVN-Youngest-Rev`.
  pub youngest: Option<Revision>,
  /// `SVN-Repository-UUID`.
  pub uuid: Option<String>,
  /// `SVN-Repository-MergeInfo`: whether the repository has the mergeinfo capability.
  pub mergeinfo: Option<bool>,
  /// `SVN-Allow-Bulk-Updates`.
  pub bulk_updates: BulkUpdates,
  /// The capabilities the `DAV` lines list: what the site serves (A-681), every one for `mod_dav_svn`'s own list
  /// ([`Capability::ALL`]).
  pub served: Vec<Capability>,
}

/// A capability `mod_dav_svn` names on a `DAV` line (`svn_dav.h`'s `SVN_DAV_NS_DAV_SVN_*`): a report, a form of an
/// answer or of a commit that a client uses once it is listed, so a site lists only those it serves (A-681).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Capability {
  /// `depth`: depth in an update-report.
  Depth,
  /// `log-revprops`: a log-report's revision properties named.
  LogRevprops,
  /// `atomic-revprops`: a revision property changed against its old value.
  AtomicRevprops,
  /// `partial-replay`: a replay of a subtree.
  PartialReplay,
  /// `inherited-props`: the `inherited-props-report`.
  InheritedProps,
  /// `inline-props`: properties inline in an update-report.
  InlineProps,
  /// `reverse-file-revs`: a `file-revs-report` newest first.
  ReverseFileRevs,
  /// `list`: the `list-report`.
  List,
  /// `mergeinfo`: the `mergeinfo-report`.
  Mergeinfo,
  /// `replay-rev-resource`: a replay against a revision's resource.
  ReplayRevResource,
  /// `ephemeral-txnprops`: a commit's transaction properties.
  EphemeralTxnprops,
  /// `svndiff1`: svndiff 1 in a commit's bodies.
  Svndiff1,
  /// `svndiff2`: svndiff 2 in a commit's bodies.
  Svndiff2,
  /// `put-result-checksum`: a PUT's answer carries its result's checksum.
  PutResultChecksum,
}

impl Capability {
  /// Every capability `mod_dav_svn` 1.14 lists, in its order.
  /// Format: `version.c` `get_vsn_options` and `get_option`.
  pub const ALL: [Capability; 14] = [
    Capability::Depth,
    Capability::LogRevprops,
    Capability::AtomicRevprops,
    Capability::PartialReplay,
    Capability::InheritedProps,
    Capability::InlineProps,
    Capability::ReverseFileRevs,
    Capability::List,
    Capability::Mergeinfo,
    Capability::ReplayRevResource,
    Capability::EphemeralTxnprops,
    Capability::Svndiff1,
    Capability::Svndiff2,
    Capability::PutResultChecksum,
  ];

  /// Its URI on a `DAV` line.
  /// Format: `svn_dav.h`, `SVN_DAV_NS_DAV_SVN_*`.
  pub fn uri(self) -> &'static str {
    match self {
      Capability::Depth => "http://subversion.tigris.org/xmlns/dav/svn/depth",
      Capability::LogRevprops => "http://subversion.tigris.org/xmlns/dav/svn/log-revprops",
      Capability::AtomicRevprops => "http://subversion.tigris.org/xmlns/dav/svn/atomic-revprops",
      Capability::PartialReplay => "http://subversion.tigris.org/xmlns/dav/svn/partial-replay",
      Capability::InheritedProps => "http://subversion.tigris.org/xmlns/dav/svn/inherited-props",
      Capability::InlineProps => "http://subversion.tigris.org/xmlns/dav/svn/inline-props",
      Capability::ReverseFileRevs => "http://subversion.tigris.org/xmlns/dav/svn/reverse-file-revs",
      Capability::List => "http://subversion.tigris.org/xmlns/dav/svn/list",
      Capability::Mergeinfo => "http://subversion.tigris.org/xmlns/dav/svn/mergeinfo",
      Capability::ReplayRevResource => {
        "http://subversion.tigris.org/xmlns/dav/svn/replay-rev-resource"
      }
      Capability::EphemeralTxnprops => {
        "http://subversion.tigris.org/xmlns/dav/svn/ephemeral-txnprops"
      }
      Capability::Svndiff1 => "http://subversion.tigris.org/xmlns/dav/svn/svndiff1",
      Capability::Svndiff2 => "http://subversion.tigris.org/xmlns/dav/svn/svndiff2",
      Capability::PutResultChecksum => {
        "http://subversion.tigris.org/xmlns/dav/svn/put-result-checksum"
      }
    }
  }
}

/// `SVNAllowBulkUpdates`, as `SVN-Allow-Bulk-Updates` names it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BulkUpdates {
  /// `On`.
  On,
  /// `Off`.
  Off,
  /// `Prefer`.
  Prefer,
}

/// The DeltaV classes `mod_dav_svn` lists on `DAV` lines before its capabilities (`get_vsn_options`).
/// Format: `version.c` `get_vsn_options`.
const VSN_CLASSES: [&str; 2] = [
  "version-control,checkout,working-resource",
  "merge,baseline,activity,version-controlled-collection",
];

/// The capabilities `get_vsn_options` lists, in its order: those before the HTTPv2 headers.
/// Format: `version.c` `get_vsn_options`.
const VSN_CAPABILITIES: [Capability; 9] = [
  Capability::Depth,
  Capability::LogRevprops,
  Capability::AtomicRevprops,
  Capability::PartialReplay,
  Capability::InheritedProps,
  Capability::InlineProps,
  Capability::ReverseFileRevs,
  Capability::List,
  Capability::Mergeinfo,
];

/// The commit capabilities `get_option` adds once the activity collection is asked for, in its order.
/// Format: `version.c` `get_option` `capabilities[]`.
const COMMIT_CAPABILITIES: [Capability; 4] = [
  Capability::EphemeralTxnprops,
  Capability::Svndiff1,
  Capability::Svndiff2,
  Capability::PutResultChecksum,
];

/// The OPTIONS answer's headers, in the order `mod_dav` and `mod_dav_svn` set them: `DAV` lines (the class `1,2`, the
/// version options), then, when the body asked for the activity collection, the HTTPv2 headers and the commit
/// capabilities; a capability only where `facts` lists it served.
pub fn options_headers(
  facts: &Httpv2,
  activity_collection_set: bool,
) -> Vec<(&'static str, String)> {
  let served = |capability: &&Capability| facts.served.contains(capability);
  let mut headers: Vec<(&'static str, String)> = vec![("DAV", String::from("1,2"))];
  headers.extend(VSN_CLASSES.iter().map(|class| ("DAV", (*class).to_owned())));
  headers.extend(
    VSN_CAPABILITIES
      .iter()
      .filter(served)
      .map(|capability| ("DAV", capability.uri().to_owned())),
  );
  if !activity_collection_set {
    return headers;
  }
  headers.extend(
    facts
      .youngest
      .map(|youngest| ("SVN-Youngest-Rev", youngest.to_string())),
  );
  headers.extend(facts.uuid.clone().map(|uuid| ("SVN-Repository-UUID", uuid)));
  headers.extend(facts.mergeinfo.map(|has| {
    (
      "SVN-Repository-MergeInfo",
      String::from(if has { "yes" } else { "no" }),
    )
  }));
  headers.extend(
    [Capability::ReplayRevResource]
      .iter()
      .filter(served)
      .map(|capability| ("DAV", capability.uri().to_owned())),
  );
  let stub = |name: &str| format!("{}/{}/{name}", facts.root, facts.special);
  headers.push(("SVN-Repository-Root", facts.root.clone()));
  headers.push(("SVN-Me-Resource", stub("me")));
  headers.push(("SVN-Rev-Root-Stub", stub("rvr")));
  headers.push(("SVN-Rev-Stub", stub("rev")));
  headers.push(("SVN-Txn-Root-Stub", stub("txr")));
  headers.push(("SVN-Txn-Stub", stub("txn")));
  headers.push(("SVN-VTxn-Root-Stub", stub("vtxr")));
  headers.push(("SVN-VTxn-Stub", stub("vtxn")));
  let bulk = match facts.bulk_updates {
    BulkUpdates::On => "On",
    BulkUpdates::Off => "Off",
    BulkUpdates::Prefer => "Prefer",
  };
  headers.push(("SVN-Allow-Bulk-Updates", String::from(bulk)));
  headers.push(("SVN-Supported-Posts", String::from("create-txn")));
  headers.push(("SVN-Supported-Posts", String::from("create-txn-with-props")));
  headers.extend(
    COMMIT_CAPABILITIES
      .iter()
      .filter(served)
      .map(|capability| ("DAV", capability.uri().to_owned())),
  );
  headers
}

/// The OPTIONS answer's body (`mod_dav.c` `dav_method_options`): the activity collection's href when asked for.
pub fn options_body(
  facts: &Httpv2,
  activity_collection_set: bool,
  room: usize,
) -> Result<Vec<u8>, DavRefusal> {
  let mut body = Body::new(room);
  body.push(XML_HEADER)?;
  body.push("<D:options-response xmlns:D=\"DAV:\">\n")?;
  if activity_collection_set {
    body.push(&format!(
      "<D:activity-collection-set><D:href>{}/{}/act/</D:href></D:activity-collection-set>",
      facts.root, facts.special
    ))?;
  }
  body.push("</D:options-response>\n")?;
  Ok(body.into_bytes())
}

/// A report answering with one revision: `dated-rev-report` or `get-deleted-rev-report` (`dated-rev.c`,
/// `deleted-rev.c`); `None` is written `-1`, Subversion's invalid revision.
pub fn revision_report(
  report: &'static str,
  revision: RevisionArg,
  room: usize,
) -> Result<Vec<u8>, DavRefusal> {
  let written = revision.map_or_else(|| String::from("-1"), |revision| revision.to_string());
  let mut body = Body::new(room);
  body.push(XML_HEADER)?;
  body.push(&format!(
    "<S:{report} xmlns:S=\"svn:\" xmlns:D=\"DAV:\">\n<D:version-name>{written}</D:version-name></S:{report}>"
  ))?;
  Ok(body.into_bytes())
}

/// A `get-locations` answer (`get-locations.c` `send_get_locations_report`): each revision and the path there.
pub fn locations(found: &[(Revision, String)], room: usize) -> Result<Vec<u8>, DavRefusal> {
  let mut body = Body::new(room);
  body.push(XML_HEADER)?;
  body.push("<S:get-locations-report xmlns:S=\"svn:\" xmlns:D=\"DAV:\">\n")?;
  for (revision, path) in found {
    body.push(&format!(
      "<S:location rev=\"{revision}\" path=\"{}\"/>\n",
      quote(path, true)
    ))?;
  }
  body.push("</S:get-locations-report>\n")?;
  Ok(body.into_bytes())
}

/// One location segment: a range of revisions and the path the node had there, if it had one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Segment {
  /// The path, relative to the repository's root; `None` for a gap.
  pub path: Option<String>,
  /// The range's first revision.
  pub start: Revision,
  /// The range's last revision.
  pub end: Revision,
}

/// A `get-location-segments` answer (`get-location-segments.c`).
pub fn location_segments(segments: &[Segment], room: usize) -> Result<Vec<u8>, DavRefusal> {
  let mut body = Body::new(room);
  body.push(XML_HEADER)?;
  body.push("<S:get-location-segments-report xmlns:S=\"svn:\" xmlns:D=\"DAV:\">\n")?;
  for segment in segments {
    let path = segment
      .path
      .as_ref()
      .map(|path| format!("path=\"{}\" ", quote(path, true)))
      .unwrap_or_default();
    body.push(&format!(
      "<S:location-segment {path}range-start=\"{}\" range-end=\"{}\"/>\n",
      segment.start, segment.end
    ))?;
  }
  body.push("</S:get-location-segments-report>\n")?;
  Ok(body.into_bytes())
}

/// A value as `get-locks.c` writes an owner or a comment: quoted, or in base64 when XML cannot carry it.
fn safe_or_base64(value: &str) -> (&'static str, String) {
  if is_xml_safe(value.as_bytes()) {
    ("", quote(value, true).into_owned())
  } else {
    ("encoding=\"base64\"", base64_lines(value.as_bytes()))
  }
}

/// A `get-locks-report` answer (`get-locks.c` `send_get_lock_response`).
pub fn locks(found: &[Lock], room: usize) -> Result<Vec<u8>, DavRefusal> {
  let mut body = Body::new(room);
  body.push(XML_HEADER)?;
  body.push("<S:get-locks-report xmlns:S=\"svn:\" xmlns:D=\"DAV:\">\n")?;
  for lock in found {
    body.push(&format!(
      "<S:lock>\n<S:path>{}</S:path>\n<S:token>{}</S:token>\n<S:creationdate>{}</S:creationdate>\n",
      quote(&lock.path, true),
      quote(lock.token.as_str(), true),
      svn_time(lock.created)?
    ))?;
    if let Some(expires) = lock.expires {
      body.push(&format!(
        "<S:expirationdate>{}</S:expirationdate>\n",
        svn_time(expires)?
      ))?;
    }
    let (encoding, owner) = safe_or_base64(&lock.owner);
    body.push(&format!("<S:owner {encoding}>{owner}</S:owner>\n"))?;
    if let Some(comment) = &lock.comment {
      let (encoding, comment) = safe_or_base64(comment);
      body.push(&format!("<S:comment {encoding}>{comment}</S:comment>\n"))?;
    }
    body.push("</S:lock>\n")?;
  }
  body.push("</S:get-locks-report>\n")?;
  Ok(body.into_bytes())
}

/// A `mergeinfo-report` answer (`mergeinfo.c`): each path, relative to the report's target, and its mergeinfo.
pub fn mergeinfo(items: &[(String, String)], room: usize) -> Result<Vec<u8>, DavRefusal> {
  let mut body = Body::new(room);
  body.push(XML_HEADER)?;
  body.push("<S:mergeinfo-report xmlns:S=\"svn:\" xmlns:D=\"DAV:\">\n")?;
  for (path, info) in items {
    body.push(&format!(
      "<S:mergeinfo-item>\n<S:mergeinfo-path>{}</S:mergeinfo-path>\n<S:mergeinfo-info>{}</S:mergeinfo-info>\n</S:mergeinfo-item>",
      quote(path, false),
      quote(info, false)
    ))?;
  }
  body.push("</S:mergeinfo-report>\n")?;
  Ok(body.into_bytes())
}

/// One item of an `inherited-props-report`: a parent's path and its properties.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InheritedItem {
  /// The parent's path or URL.
  pub path: String,
  /// Its properties, each a name and a value.
  pub props: Vec<(String, Vec<u8>)>,
}

/// An `inherited-props-report` answer (`inherited-props.c`): a value XML can carry is escaped as
/// `svn_xml_escape_cdata_string` escapes it, `\r` as `&#13;`; any other in base64.
pub fn inherited_props(items: &[InheritedItem], room: usize) -> Result<Vec<u8>, DavRefusal> {
  let mut body = Body::new(room);
  body.push(XML_HEADER)?;
  body.push("<S:inherited-props-report xmlns:S=\"svn:\" xmlns:D=\"DAV:\">\n")?;
  for item in items {
    body.push(&format!(
      "<S:iprop-item>\n<S:iprop-path>{}</S:iprop-path>\n",
      quote(&item.path, false)
    ))?;
    for (name, value) in &item.props {
      body.push(&format!(
        "<S:iprop-propname>{}</S:iprop-propname>\n",
        quote(name, false)
      ))?;
      body.push(&match std::str::from_utf8(value)
        .ok()
        .filter(|_| is_xml_safe(value))
      {
        Some(text) => format!(
          "<S:iprop-propval>{}</S:iprop-propval>\n",
          escape_cdata(text)
        ),
        None => format!(
          "<S:iprop-propval encoding=\"base64\">{}</S:iprop-propval>\n",
          base64_lines(value)
        ),
      })?;
    }
    body.push("</S:iprop-item>\n")?;
  }
  body.push("</S:inherited-props-report>\n")?;
  Ok(body.into_bytes())
}

/// `text` as `svn_xml_escape_cdata_string` escapes it: `&`, `<`, `>`, and a carriage return as `&#13;`.
fn escape_cdata(text: &str) -> String {
  quote(text, false).replace('\r', "&#13;")
}

/// What a log item's changed path is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Change {
  /// `S:added-path`.
  Added,
  /// `S:replaced-path`.
  Replaced,
  /// `S:deleted-path`.
  Deleted,
  /// `S:modified-path`.
  Modified,
}

/// One changed path of a log item (`svn_repos_path_change_t`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChangedPath {
  /// The path.
  pub path: String,
  /// What changed.
  pub change: Change,
  /// The node's kind as `svn_node_kind_to_word` names it: `file`, `dir`, `none`, `unknown` or `symlink`.
  pub kind: &'static str,
  /// The text changed.
  pub text_mods: bool,
  /// The properties changed.
  pub prop_mods: bool,
  /// An addition's or a replacement's copy source.
  pub copy_from: Option<(String, Revision)>,
}

/// One log item: its changed paths, then its revision and revision properties.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogItem {
  /// The revision; `None` ends a sequence of merged revisions, written `-1`.
  pub revision: RevisionArg,
  /// Its changed paths, written before the rest as `svn_repos_get_logs5` calls the path receiver first.
  pub changed: Vec<ChangedPath>,
  /// Its revision properties, each a name and a value.
  pub revprops: Vec<(String, Vec<u8>)>,
  /// `S:has-children`: merged revisions follow.
  pub has_children: bool,
  /// `S:subtractive-merge`.
  pub subtractive_merge: bool,
}

/// The start of a `log-report` answer: the XML declaration, then the report's element.
/// Format: `reports/log.c` `maybe_send_header`: `DAV_XML_HEADER DEBUG_CR` and the element, then `DEBUG_CR`.
pub(crate) const LOG_REPORT_START: [&str; 2] = [
  XML_HEADER,
  "<S:log-report xmlns:S=\"svn:\" xmlns:D=\"DAV:\">\n",
];

/// A log item's start, written before its first changed path or its revision.
/// Format: `reports/log.c` `maybe_start_log_item`.
pub(crate) const LOG_ITEM_START: &str = "<S:log-item>\n";

/// The end of a `log-report` answer.
/// Format: `reports/log.c` `dav_svn__log_report`.
pub(crate) const LOG_REPORT_END: &str = "</S:log-report>\n";

/// A `log-report` answer (`log.c`), its items written as the client asked: a revision property in base64 when it asked
/// for binary properties encoded and XML cannot carry it.
pub fn log(
  items: &[LogItem],
  encode_binary_props: bool,
  room: usize,
) -> Result<Vec<u8>, DavRefusal> {
  let mut body = Body::new(room);
  for part in LOG_REPORT_START {
    body.push(part)?;
  }
  for item in items {
    body.push(LOG_ITEM_START)?;
    for changed in &item.changed {
      body.push(&changed_path(changed))?;
    }
    let revprops: Vec<(&str, &[u8])> = item
      .revprops
      .iter()
      .map(|(name, value)| (name.as_str(), value.as_slice()))
      .collect();
    let flags = (item.has_children, item.subtractive_merge);
    body.push(&log_item_end(
      item.revision,
      &revprops,
      flags,
      encode_binary_props,
    ))?;
  }
  body.push(LOG_REPORT_END)?;
  Ok(body.into_bytes())
}

/// What `log_revision_receiver` writes of an item after its changed paths: the revision (`-1` for none, the entry
/// closing a run of merged revisions), each revision property in `revprops`' order, `S:has-children` and
/// `S:subtractive-merge` when `flags` (in that order) set them, and the item's end.
pub(crate) fn log_item_end(
  revision: RevisionArg,
  revprops: &[(&str, &[u8])],
  flags: (bool, bool),
  encode_binary_props: bool,
) -> String {
  let revision = revision.map_or_else(|| String::from("-1"), |revision| revision.to_string());
  let mut text = format!("<D:version-name>{revision}</D:version-name>\n");
  for (name, value) in revprops {
    text.push_str(&revprop(name, value, encode_binary_props));
  }
  let (has_children, subtractive_merge) = flags;
  if has_children {
    text.push_str("<S:has-children/>");
  }
  if subtractive_merge {
    text.push_str("<S:subtractive-merge/>");
  }
  text.push_str("</S:log-item>\n");
  text
}

/// One changed path as `log_change_receiver` writes it.
pub(crate) fn changed_path(changed: &ChangedPath) -> String {
  let element = match changed.change {
    Change::Added => "S:added-path",
    Change::Replaced => "S:replaced-path",
    Change::Deleted => "S:deleted-path",
    Change::Modified => "S:modified-path",
  };
  let copied = matches!(changed.change, Change::Added | Change::Replaced);
  let copy_from = match &changed.copy_from {
    Some((path, revision)) if copied => format!(
      " copyfrom-path=\"{}\" copyfrom-rev=\"{revision}\"",
      quote(path, true)
    ),
    _ => String::new(),
  };
  format!(
    "<{element}{copy_from} node-kind=\"{}\" text-mods=\"{}\" prop-mods=\"{}\">{}</{element}>\n",
    changed.kind,
    changed.text_mods,
    changed.prop_mods,
    quote(&changed.path, false)
  )
}

/// One revision property as `log_revision_receiver` writes it: author, date and log in their elements, the log message
/// fuzzily escaped, any other as `S:revprop`.
fn revprop(name: &str, value: &[u8], encode_binary_props: bool) -> String {
  let (encoding, text) = match std::str::from_utf8(value) {
    Ok(text) if !encode_binary_props || is_xml_safe(value) => ("", text.to_owned()),
    Ok(_) | Err(_) => (" encoding=\"base64\"", base64_lines(value)),
  };
  match name {
    "svn:author" => format!(
      "<D:creator-displayname{encoding}>{}</D:creator-displayname>\n",
      quote(&text, false)
    ),
    "svn:date" => format!("<S:date{encoding}>{}</S:date>\n", quote(&text, false)),
    "svn:log" => format!(
      "<D:comment{encoding}>{}</D:comment>\n",
      quote(&fuzzy_escape(&text), false)
    ),
    _ => format!(
      "<S:revprop name=\"{}\"{encoding}>{}</S:revprop>\n",
      quote(name, true),
      quote(&text, false)
    ),
  }
}

/// A MERGE answer (`merge.c` `dav_svn__merge_response`): the new baseline's response, as written when the client sent
/// `X-SVN-Options: no-merge-response`, with the post-commit hook's failure when it failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MergeResponse {
  /// The version-controlled configuration's href (`/repo/!svn/vcc/default`).
  pub vcc: String,
  /// The new revision.
  pub revision: Revision,
  /// Its `svn:date`, written as it is stored.
  pub date: Option<String>,
  /// Its `svn:author`.
  pub author: Option<String>,
  /// The post-commit hook's failure, if it failed.
  pub post_commit_error: Option<String>,
}

/// The MERGE answer's body.
pub fn merge(response: &MergeResponse, room: usize) -> Result<Vec<u8>, DavRefusal> {
  let mut body = Body::new(room);
  body.push(XML_HEADER)?;
  let (header, element) = match &response.post_commit_error {
    Some(error) => (
      " xmlns:S=\"svn:\"",
      format!(
        "<S:post-commit-err>{}</S:post-commit-err>",
        quote(error, false)
      ),
    ),
    None => ("", String::new()),
  };
  body.push(&format!(
    "<D:merge-response xmlns:D=\"DAV:\"{header}>\n<D:updated-set>\n<D:response>\n<D:href>{}</D:href>\n\
     <D:propstat><D:prop>\n<D:resourcetype><D:baseline/></D:resourcetype>\n{element}\n\
     <D:version-name>{}</D:version-name>\n",
    quote(&response.vcc, false),
    response.revision
  ))?;
  if let Some(date) = &response.date {
    body.push(&format!(
      "<D:creationdate>{}</D:creationdate>\n",
      quote(date, false)
    ))?;
  }
  if let Some(author) = &response.author {
    body.push(&format!(
      "<D:creator-displayname>{}</D:creator-displayname>\n",
      quote(author, true)
    ))?;
  }
  body.push(
    "</D:prop>\n<D:status>HTTP/1.1 200 OK</D:status>\n</D:propstat>\n</D:response>\n</D:updated-set>\n\
     </D:merge-response>\n",
  )?;
  Ok(body.into_bytes())
}

/// One property of a multistatus answer, already written as its element (`<lp1:version-name>5</lp1:version-name>`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Propstat {
  /// The status line's code and phrase, `200 OK` or `404 Not Found`.
  pub status: &'static str,
  /// The properties, each its whole element.
  pub props: Vec<String>,
}

/// A live property of `mod_dav_svn` as `liveprops.c` writes it: `lp1` for `DAV:`, `lp2` for its own namespace; an
/// empty value is an empty element.
pub fn live_prop(dav_namespace: bool, name: &str, value: &str) -> String {
  let prefix = if dav_namespace { "lp1" } else { "lp2" };
  if value.is_empty() {
    format!("<{prefix}:{name}/>\n")
  } else {
    format!("<{prefix}:{name}>{value}</{prefix}:{name}>\n")
  }
}

/// A multistatus answer (`mod_dav.c` `dav_begin_multistatus`, `dav_send_one_response`): each response's href and its
/// property results; the live properties' prefixes declared on each response, as `mod_dav_svn`'s `lp1` and `lp2` are.
pub fn multistatus(
  responses: &[(String, Vec<Propstat>)],
  room: usize,
) -> Result<Vec<u8>, DavRefusal> {
  let mut body = Body::new(room);
  body.push(XML_HEADER)?;
  body.push("<D:multistatus xmlns:D=\"DAV:\">\n")?;
  for (href, propstats) in responses {
    body.push(&format!(
      "<D:response xmlns:lp1=\"DAV:\" xmlns:lp2=\"http://subversion.tigris.org/xmlns/dav/\">\n<D:href>{}</D:href>\n",
      quote(href, false)
    ))?;
    for propstat in propstats {
      body.push("<D:propstat>\n<D:prop>\n")?;
      for prop in &propstat.props {
        body.push(prop)?;
      }
      body.push(&format!(
        "</D:prop>\n<D:status>HTTP/1.1 {}</D:status>\n</D:propstat>\n",
        propstat.status
      ))?;
    }
    body.push("</D:response>\n")?;
  }
  body.push("</D:multistatus>\n")?;
  Ok(body.into_bytes())
}

/// Created.
/// Format: RFC 9110 §15.3.2.
const HTTP_CREATED: u16 = 201;

/// The start of `mod_dav`'s HTML answer body, before the status line.
/// Format: httpd 2.4 `modules/dav/main/mod_dav.h` `DAV_RESPONSE_BODY_1`.
const DAV_RESPONSE_BODY_1: &str = "<!DOCTYPE HTML PUBLIC \"-//W3C//DTD HTML 4.01//EN\" \"http://www.w3.org/TR/html4/strict.dtd\">\n<html>\n<head>\n<title>";

/// After the status line, before the status's words.
/// Format: `mod_dav.h` `DAV_RESPONSE_BODY_2`.
const DAV_RESPONSE_BODY_2: &str = "</title>\n</head><body>\n<h1>";

/// After the status's words, before the message.
/// Format: `mod_dav.h` `DAV_RESPONSE_BODY_3`.
const DAV_RESPONSE_BODY_3: &str = "</h1>\n<p>";

/// After the message, before the server's signature (none: httpd's `ServerSignature` is off by default).
/// Format: `mod_dav.h` `DAV_RESPONSE_BODY_4`.
const DAV_RESPONSE_BODY_4: &str = "</p>\n";

/// The end of the body.
/// Format: `mod_dav.h` `DAV_RESPONSE_BODY_5`.
const DAV_RESPONSE_BODY_5: &str = "</body></html>\n";

/// `201`, as `mod_dav`'s `dav_created` answers a resource it made (DeltaV v1's MKACTIVITY, "Activity", and CHECKOUT,
/// "Checked-out resource"; A-766 part 2): `location` the resource's absolute URL, `Cache-Control: no-cache` (both
/// methods set it), and `dav_error_response`'s HTML body: "WHAT URI has been created.", `uri` the resource's path with
/// HTML's specials escaped (`ap_escape_html`).
pub fn created(what: &str, uri: &str, location: &str) -> HttpAnswer {
  let mut body = String::new();
  body.push_str(DAV_RESPONSE_BODY_1);
  body.push_str("201 Created");
  body.push_str(DAV_RESPONSE_BODY_2);
  body.push_str("Created");
  body.push_str(DAV_RESPONSE_BODY_3);
  body.push_str(what);
  body.push(' ');
  for character in uri.chars() {
    match character {
      '<' => body.push_str("&lt;"),
      '>' => body.push_str("&gt;"),
      '&' => body.push_str("&amp;"),
      '"' => body.push_str("&quot;"),
      other => body.push(other),
    }
  }
  body.push_str(" has been created.");
  body.push_str(DAV_RESPONSE_BODY_4);
  body.push_str(DAV_RESPONSE_BODY_5);
  HttpAnswer {
    status: HTTP_CREATED,
    headers: vec![
      ("Location", location.to_owned()),
      ("Cache-Control", String::from("no-cache")),
      (
        "Content-Type",
        String::from("text/html; charset=ISO-8859-1"),
      ),
    ],
    body: body.into_bytes(),
  }
}
