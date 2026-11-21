//! The request bodies a server reads (§4.17.4 piece 13; A-609), each decoded as `mod_dav` and `mod_dav_svn` read it:
//! OPTIONS (`mod_dav.c` `dav_method_options`, `version.c` `get_option`), PROPFIND (`dav_method_propfind`), MERGE
//! (`dav_method_merge`, `version.c` `merge` and its lock-token list) and the REPORTs (`version.c` `deliver_report`
//! and each handler under `reports/`).
//!
//! A report's root must be in the `svn:` namespace and named for a report this server reads, or it is
//! [`DavRefusal::UnknownReport`]. Within it, as every handler's loop does, an element in another namespace is skipped
//! and an unknown `svn:` element is ignored; a value is read with the whitespace `dav_xml_get_cdata` keeps or cuts for
//! that element; a path passes `dav_svn__test_canonical`; and what a handler requires and finds missing is refused by
//! kind. Defaults that need the repository (the youngest revision for a start not given, the anchor a relative path
//! joins) are the server's, so a decoded body keeps what the client sent, and a revision the body writes negative is
//! `None`.

use super::xml::{self, Element};
use super::{Bounds, DAV, DavRefusal, RevisionArg, SVN};
use crate::index::{Revision, is_canonical};
use crate::lock::AprTime;

/// Microseconds in a second, for a date's fraction.
/// Format: `apr_time_t` counts microseconds; `svn_time_to_cstring` writes six fraction digits.
const MICROSECONDS_PER_SECOND: u64 = 1_000_000;

/// The digits of a date's fraction of a second.
/// Format: `TIMESTAMP_FORMAT` `"%04d-%02d-%02dT%02d:%02d:%02d.%06dZ"` (`libsvn_subr/time.c`).
const FRACTION_DIGITS: usize = 6;

/// A depth, as `svn_depth_from_word` reads one (`svn_types.h` `svn_depth_t`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SvnDepth {
  /// `exclude`.
  Exclude,
  /// `unknown`, and every word Subversion does not know.
  Unknown,
  /// `empty`.
  Empty,
  /// `files`.
  Files,
  /// `immediates`.
  Immediates,
  /// `infinity`.
  Infinity,
}

impl SvnDepth {
  /// The depth a word names; an unknown word is `Unknown`, as `svn_depth_from_word` answers it.
  pub fn from_word(word: &str) -> Self {
    match word {
      "exclude" => Self::Exclude,
      "empty" => Self::Empty,
      "files" => Self::Files,
      "immediates" => Self::Immediates,
      "infinity" => Self::Infinity,
      _ => Self::Unknown,
    }
  }
}

/// An OPTIONS body: what it asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OptionsRequest {
  /// Whether it asks for `DAV:activity-collection-set`; only then does `mod_dav_svn` add the HTTPv2 headers.
  pub activity_collection_set: bool,
}

/// One property a PROPFIND or a MERGE names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PropertyName {
  /// Its namespace.
  pub namespace: String,
  /// Its local name.
  pub name: String,
}

/// A PROPFIND body: what it asks for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PropfindRequest {
  /// Every property (`DAV:allprop`, or no body).
  AllProp,
  /// The names of every property (`DAV:propname`).
  PropName,
  /// The properties named (`DAV:prop`).
  Prop(Vec<PropertyName>),
}

/// A MERGE body.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MergeRequest {
  /// The activity or transaction merged (`DAV:source`'s `DAV:href`).
  pub source: String,
  /// `DAV:no-auto-merge` given.
  pub no_auto_merge: bool,
  /// `DAV:no-checkout` given.
  pub no_checkout: bool,
  /// The properties the response is asked to carry (`DAV:prop`).
  pub props: Vec<PropertyName>,
  /// The locks the commit presents (`S:lock-token-list`): each path, relative to the MERGE's target, with its token.
  pub lock_tokens: Vec<(String, String)>,
}

/// An `update-report` (`reports/update.c`): the client's working copy, and what it wants to reach.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UpdateReport {
  /// `send-all="true"`: the response carries the texts and properties inline (when the server allows bulk updates).
  pub send_all: bool,
  /// `S:src-path`: the anchor, a URL or a path.
  pub src_path: String,
  /// `S:dst-path`, for a switch.
  pub dst_path: Option<String>,
  /// `S:target-revision`; `None` for the youngest.
  pub target_revision: RevisionArg,
  /// `S:update-target`, the one entry of the anchor updated.
  pub update_target: Option<String>,
  /// `S:depth`, as given.
  pub depth: Option<SvnDepth>,
  /// `S:recursive`, as given: `no` is `false`.
  pub recursive: Option<bool>,
  /// `S:ignore-ancestry` other than `no`.
  pub ignore_ancestry: bool,
  /// `S:send-copyfrom-args` other than `no`.
  pub send_copyfrom_args: bool,
  /// `S:resource-walk` other than `no`.
  pub resource_walk: bool,
  /// `S:text-deltas`: `no` is `false`; it defaults to `true`.
  pub text_deltas: bool,
  /// `S:include-props` other than `no`.
  pub include_props: bool,
  /// The working copy's state, in order: each `S:entry` and `S:missing`.
  pub state: Vec<Reported>,
}

/// One item of an update-report's working-copy state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reported {
  /// `S:entry`: a path at a revision.
  Entry {
    /// The path, relative to the anchor.
    path: String,
    /// `rev`; `None` when written negative.
    revision: RevisionArg,
    /// `depth`; `infinity` when absent.
    depth: SvnDepth,
    /// `start-empty` present.
    start_empty: bool,
    /// `linkpath`, a switched path's URL.
    linkpath: Option<String>,
    /// `lock-token`.
    lock_token: Option<String>,
  },
  /// `S:missing`: a path the working copy lacks.
  Missing {
    /// The path, relative to the anchor.
    path: String,
  },
}

/// Which revision properties a `log-report` asks for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Revprops {
  /// No revprop element: a pre-1.5 client's author, date and log.
  Default,
  /// `S:all-revprops`.
  All,
  /// The `S:revprop` names given, none for `S:no-revprops` alone.
  Named(Vec<String>),
}

/// A `log-report` (`reports/log.c`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogReport {
  /// `S:start-revision`.
  pub start: RevisionArg,
  /// `S:end-revision`.
  pub end: RevisionArg,
  /// `S:limit`; 0 for none.
  pub limit: i32,
  /// `S:discover-changed-paths` present.
  pub discover_changed_paths: bool,
  /// `S:strict-node-history` present.
  pub strict_node_history: bool,
  /// `S:include-merged-revisions` present.
  pub include_merged_revisions: bool,
  /// `S:encode-binary-props` present.
  pub encode_binary_props: bool,
  /// The revision properties asked for.
  pub revprops: Revprops,
  /// `S:path`s, relative to the report's target.
  pub paths: Vec<String>,
}

/// A `dated-rev-report` (`reports/dated-rev.c`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DatedRevReport {
  /// `DAV:creationdate`.
  pub date: AprTime,
}

/// A `get-locations` report (`reports/get-locations.c`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GetLocationsReport {
  /// `S:path`.
  pub path: String,
  /// `S:peg-revision`.
  pub peg_revision: Revision,
  /// Each `S:location-revision`.
  pub location_revisions: Vec<RevisionArg>,
}

/// A `get-location-segments` report (`reports/get-location-segments.c`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GetLocationSegmentsReport {
  /// `S:path`.
  pub path: String,
  /// `S:peg-revision`; `None` for the youngest.
  pub peg_revision: RevisionArg,
  /// `S:start-revision`; `None` for the youngest.
  pub start: RevisionArg,
  /// `S:end-revision`; `None` for 0.
  pub end: RevisionArg,
}

/// A `get-locks-report` (`reports/get-locks.c`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GetLocksReport {
  /// The `depth` attribute; `infinity` when absent.
  pub depth: SvnDepth,
}

/// How mergeinfo is inherited (`svn_mergeinfo_inheritance_t`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Inherit {
  /// `explicit`, and every other word.
  Explicit,
  /// `inherited`.
  Inherited,
  /// `nearest-ancestor`.
  NearestAncestor,
}

/// A `mergeinfo-report` (`reports/mergeinfo.c`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MergeinfoReport {
  /// `S:revision`.
  pub revision: RevisionArg,
  /// `S:inherit`.
  pub inherit: Inherit,
  /// `S:include-descendants` is `yes`.
  pub include_descendants: bool,
  /// Each `S:path`.
  pub paths: Vec<String>,
}

/// A `get-deleted-rev-report` (`reports/deleted-rev.c`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GetDeletedRevReport {
  /// `S:path`.
  pub path: String,
  /// `S:peg-revision`.
  pub peg_revision: Revision,
  /// `S:end-revision`.
  pub end_revision: Revision,
}

/// An `inherited-props-report` (`reports/inherited-props.c`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InheritedPropsReport {
  /// `S:revision`.
  pub revision: RevisionArg,
  /// `S:path`, relative to the report's target; empty when absent.
  pub path: String,
}

/// A `replay-report` (`reports/replay.c`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReplayReport {
  /// `S:revision`, the old syntax's; the 1.8 syntax names the revision by the resource the report is sent to.
  pub revision: RevisionArg,
  /// `S:low-water-mark`.
  pub low_water_mark: Revision,
  /// `S:send-deltas` other than 0; `true` when absent.
  pub send_deltas: bool,
  /// `S:include-path`, the 1.8 syntax's subtree.
  pub include_path: Option<String>,
}

/// A REPORT body.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Report {
  /// `update-report`.
  Update(UpdateReport),
  /// `log-report`.
  Log(LogReport),
  /// `dated-rev-report`.
  DatedRev(DatedRevReport),
  /// `get-locations`.
  GetLocations(GetLocationsReport),
  /// `get-location-segments`.
  GetLocationSegments(GetLocationSegmentsReport),
  /// `get-locks-report`.
  GetLocks(GetLocksReport),
  /// `mergeinfo-report`.
  Mergeinfo(MergeinfoReport),
  /// `get-deleted-rev-report`.
  GetDeletedRev(GetDeletedRevReport),
  /// `inherited-props-report`.
  InheritedProps(InheritedPropsReport),
  /// `replay-report`.
  Replay(ReplayReport),
  /// `file-revs-report` (`svn blame`; A-726).
  FileRevs(super::listing::FileRevsReport),
}

/// Reads an OPTIONS body; no body asks for nothing.
pub fn options(body: &[u8], bounds: &Bounds) -> Result<OptionsRequest, DavRefusal> {
  let Some(root) = document(body, bounds)? else {
    return Ok(OptionsRequest {
      activity_collection_set: false,
    });
  };
  rooted(&root, DAV, "options")?;
  Ok(OptionsRequest {
    activity_collection_set: root
      .elements()
      .any(|child| child.is(DAV, "activity-collection-set")),
  })
}

/// Reads a PROPFIND body; no body is `allprop`. The request's command is found as `mod_dav` finds it: `allprop`, then
/// `propname`, then `prop`.
pub fn propfind(body: &[u8], bounds: &Bounds) -> Result<PropfindRequest, DavRefusal> {
  let Some(root) = document(body, bounds)? else {
    return Ok(PropfindRequest::AllProp);
  };
  rooted(&root, DAV, "propfind")?;
  if root.elements().any(|child| child.is(DAV, "allprop")) {
    return Ok(PropfindRequest::AllProp);
  }
  if root.elements().any(|child| child.is(DAV, "propname")) {
    return Ok(PropfindRequest::PropName);
  }
  let prop = root
    .elements()
    .find(|child| child.is(DAV, "prop"))
    .ok_or(DavRefusal::Missing { element: "prop" })?;
  Ok(PropfindRequest::Prop(names(prop)))
}

/// Reads a MERGE body.
pub fn merge(body: &[u8], bounds: &Bounds) -> Result<MergeRequest, DavRefusal> {
  let root = document(body, bounds)?.ok_or(DavRefusal::Missing { element: "merge" })?;
  rooted(&root, DAV, "merge")?;
  let source = root
    .elements()
    .find(|child| child.is(DAV, "source"))
    .ok_or(DavRefusal::Missing { element: "source" })?;
  let href = source
    .elements()
    .find(|child| child.is(DAV, "href"))
    .ok_or(DavRefusal::Missing { element: "href" })?;
  Ok(MergeRequest {
    source: href.trimmed_text(),
    no_auto_merge: root.elements().any(|child| child.is(DAV, "no-auto-merge")),
    no_checkout: root.elements().any(|child| child.is(DAV, "no-checkout")),
    props: root
      .elements()
      .find(|child| child.is(DAV, "prop"))
      .map(names)
      .unwrap_or_default(),
    lock_tokens: lock_token_list(&root)?,
  })
}

/// A CHECKOUT body as `mod_dav` reads it (`mod_dav.c` `dav_method_checkout`; DeltaV v1, A-766 part 2): the activities its
/// `DAV:activity-set` names by `DAV:href`, whether it asks for a new activity (`DAV:new`), and whether it carries
/// `DAV:apply-to-version`, which resolves a VCC to its checked-in baseline.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CheckoutRequest {
  /// The activities named, in order.
  pub activities: Vec<String>,
  /// `DAV:new` given in the activity set.
  pub new_activity: bool,
  /// `DAV:apply-to-version` given.
  pub apply_to_version: bool,
}

/// Reads a CHECKOUT body; an empty body asks for nothing. Refused for a root other than `DAV:checkout`, and for an
/// activity set with neither an href nor `DAV:new` (`mod_dav`'s `400`).
pub fn checkout(body: &[u8], bounds: &Bounds) -> Result<CheckoutRequest, DavRefusal> {
  let Some(root) = document(body, bounds)? else {
    return Ok(CheckoutRequest::default());
  };
  rooted(&root, DAV, "checkout")?;
  let mut request = CheckoutRequest {
    apply_to_version: root
      .elements()
      .any(|child| child.is(DAV, "apply-to-version")),
    ..CheckoutRequest::default()
  };
  if let Some(set) = root.elements().find(|child| child.is(DAV, "activity-set")) {
    request.new_activity = set.elements().any(|child| child.is(DAV, "new"));
    if !request.new_activity {
      request.activities = set
        .elements()
        .filter(|child| child.is(DAV, "href"))
        .map(Element::trimmed_text)
        .collect();
      if request.activities.is_empty() {
        return Err(DavRefusal::Missing { element: "href" });
      }
    }
  }
  Ok(request)
}

/// Reads a REPORT body.
pub fn report(body: &[u8], bounds: &Bounds) -> Result<Report, DavRefusal> {
  let root = document(body, bounds)?.ok_or(DavRefusal::Missing { element: "report" })?;
  if root.namespace != SVN {
    return Err(unknown(&root));
  }
  match root.name.as_str() {
    "update-report" => update_report(&root).map(Report::Update),
    "log-report" => log_report(&root).map(Report::Log),
    "dated-rev-report" => dated_rev_report(&root).map(Report::DatedRev),
    "get-locations" => get_locations(&root).map(Report::GetLocations),
    "get-location-segments" => get_location_segments(&root).map(Report::GetLocationSegments),
    "get-locks-report" => get_locks(&root).map(Report::GetLocks),
    "mergeinfo-report" => Ok(Report::Mergeinfo(mergeinfo(&root)?)),
    "get-deleted-rev-report" => get_deleted_rev(&root).map(Report::GetDeletedRev),
    "inherited-props-report" => Ok(Report::InheritedProps(inherited_props(&root)?)),
    "replay-report" => replay(&root).map(Report::Replay),
    "file-revs-report" => super::listing::file_revs_from(&root).map(Report::FileRevs),
    _ => Err(unknown(&root)),
  }
}

/// The body's root, or `None` for a body of nothing but whitespace.
fn document(body: &[u8], bounds: &Bounds) -> Result<Option<Element>, DavRefusal> {
  if body.iter().all(u8::is_ascii_whitespace) {
    return Ok(None);
  }
  Ok(Some(xml::parse(body, bounds)?))
}

/// Refuses a root other than `name` in `namespace`.
fn rooted(root: &Element, namespace: &str, name: &'static str) -> Result<(), DavRefusal> {
  if root.is(namespace, name) {
    Ok(())
  } else {
    Err(DavRefusal::UnexpectedRoot {
      expected: name,
      namespace: root.namespace.clone(),
      name: root.name.clone(),
    })
  }
}

/// The refusal of a report this server does not read.
fn unknown(root: &Element) -> DavRefusal {
  DavRefusal::UnknownReport {
    namespace: root.namespace.clone(),
    name: root.name.clone(),
  }
}

/// The names of the properties `prop` holds.
fn names(prop: &Element) -> Vec<PropertyName> {
  prop
    .elements()
    .map(|named| PropertyName {
      namespace: named.namespace.clone(),
      name: named.name.clone(),
    })
    .collect()
}

/// The `svn:` children of a report's root, in order: every other child is skipped.
fn svn_children(root: &Element) -> impl Iterator<Item = &Element> {
  root.elements().filter(|child| child.namespace == SVN)
}

/// A revision as a body writes it, `element`'s: decimal, and negative for none. Refused otherwise, where `atol` would
/// read garbage as 0.
fn revision(element: &'static str, text: &str) -> Result<RevisionArg, DavRefusal> {
  let malformed = || DavRefusal::Malformed {
    element,
    text: text.to_owned(),
  };
  let (negative, digits) = match text.strip_prefix('-') {
    Some(digits) => (true, digits),
    None => (false, text),
  };
  if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
    return Err(malformed());
  }
  let value: Revision = digits.parse().map_err(|_| malformed())?;
  Ok(if negative { None } else { Some(value) })
}

/// `element`'s trimmed text as a revision, refused empty.
fn revision_of(child: &Element, element: &'static str) -> Result<RevisionArg, DavRefusal> {
  revision(element, &child.trimmed_text())
}

/// A path that passes `dav_svn__test_canonical` as this server reads it: empty, a canonical absolute path, or a
/// canonical relative path, no name `.` or `..` in either.
fn canonical(path: String) -> Result<String, DavRefusal> {
  let relative_ok = path
    .split('/')
    .all(|name| !name.is_empty() && name != "." && name != ".." && !name.contains(['\n', '\0']));
  if path.is_empty() || (path.starts_with('/') && is_canonical(&path)) || relative_ok {
    Ok(path)
  } else {
    Err(DavRefusal::NotCanonical { path })
  }
}

/// A flag element's word, `element`'s: refused empty; `true` unless it is `no`.
fn not_no(child: &Element, element: &'static str) -> Result<bool, DavRefusal> {
  let word = child.trimmed_text();
  if word.is_empty() {
    return Err(DavRefusal::Malformed {
      element,
      text: word,
    });
  }
  Ok(word != "no")
}

/// Reads an `update-report`'s parameters, then its working-copy state.
fn update_report(root: &Element) -> Result<UpdateReport, DavRefusal> {
  let mut report = UpdateReport {
    send_all: root.attribute("send-all") == Some("true"),
    text_deltas: true,
    ..UpdateReport::default()
  };
  for child in svn_children(root) {
    update_parameter(&mut report, child)?;
  }
  if report.src_path.is_empty() {
    return Err(DavRefusal::Missing {
      element: "src-path",
    });
  }
  Ok(report)
}

/// One `svn:` child of an `update-report`: a parameter, an entry, or something ignored.
fn update_parameter(report: &mut UpdateReport, child: &Element) -> Result<(), DavRefusal> {
  match child.name.as_str() {
    "target-revision" => report.target_revision = nonempty_revision(child, "target-revision")?,
    "src-path" => report.src_path = nonempty_raw(child, "src-path")?,
    "dst-path" => report.dst_path = Some(nonempty_raw(child, "dst-path")?),
    "update-target" => report.update_target = Some(canonical(child.text())?),
    "depth" => report.depth = Some(SvnDepth::from_word(&nonempty_trimmed(child, "depth")?)),
    "recursive" => report.recursive = Some(not_no(child, "recursive")?),
    "ignore-ancestry" => report.ignore_ancestry = not_no(child, "ignore-ancestry")?,
    "send-copyfrom-args" => report.send_copyfrom_args = not_no(child, "send-copyfrom-args")?,
    "resource-walk" => report.resource_walk = not_no(child, "resource-walk")?,
    "text-deltas" => report.text_deltas = not_no(child, "text-deltas")?,
    "include-props" => report.include_props = not_no(child, "include-props")?,
    "entry" => report.state.push(entry(child)?),
    "missing" => report.state.push(Reported::Missing {
      path: canonical(child.text())?,
    }),
    _ => {}
  }
  Ok(())
}

/// `element`'s trimmed text, refused empty.
fn nonempty_trimmed(child: &Element, element: &'static str) -> Result<String, DavRefusal> {
  let text = child.trimmed_text();
  if text.is_empty() {
    return Err(DavRefusal::Malformed { element, text });
  }
  Ok(text)
}

/// `element`'s text as written, refused empty.
fn nonempty_raw(child: &Element, element: &'static str) -> Result<String, DavRefusal> {
  let text = child.text();
  if text.is_empty() {
    return Err(DavRefusal::Malformed { element, text });
  }
  Ok(text)
}

/// `element`'s trimmed text as a revision, refused empty.
fn nonempty_revision(child: &Element, element: &'static str) -> Result<RevisionArg, DavRefusal> {
  revision(element, &nonempty_trimmed(child, element)?)
}

/// One `S:entry`: its attributes, and its text as its path; `rev` is required.
fn entry(child: &Element) -> Result<Reported, DavRefusal> {
  let rev = child
    .attribute("rev")
    .ok_or(DavRefusal::Missing { element: "rev" })?;
  Ok(Reported::Entry {
    path: canonical(child.text())?,
    revision: revision("rev", rev)?,
    depth: child
      .attribute("depth")
      .map_or(SvnDepth::Infinity, SvnDepth::from_word),
    start_empty: child.attribute("start-empty").is_some(),
    linkpath: child.attribute("linkpath").map(str::to_owned),
    lock_token: child.attribute("lock-token").map(str::to_owned),
  })
}

/// Reads a `log-report`.
fn log_report(root: &Element) -> Result<LogReport, DavRefusal> {
  let mut report = LogReport {
    start: None,
    end: None,
    limit: 0,
    discover_changed_paths: false,
    strict_node_history: false,
    include_merged_revisions: false,
    encode_binary_props: false,
    revprops: Revprops::Default,
    paths: Vec::new(),
  };
  for child in svn_children(root) {
    log_parameter(&mut report, child)?;
  }
  Ok(report)
}

/// One `svn:` child of a `log-report`.
fn log_parameter(report: &mut LogReport, child: &Element) -> Result<(), DavRefusal> {
  match child.name.as_str() {
    "start-revision" => report.start = revision_of(child, "start-revision")?,
    "end-revision" => report.end = revision_of(child, "end-revision")?,
    "limit" => report.limit = limit(child)?,
    "discover-changed-paths" => report.discover_changed_paths = true,
    "strict-node-history" => report.strict_node_history = true,
    "include-merged-revisions" => report.include_merged_revisions = true,
    "encode-binary-props" => report.encode_binary_props = true,
    "all-revprops" => report.revprops = Revprops::All,
    "no-revprops" => {
      if report.revprops == Revprops::Default {
        report.revprops = Revprops::Named(Vec::new());
      }
    }
    "revprop" => revprop(&mut report.revprops, child.text()),
    "path" => report.paths.push(canonical(child.text())?),
    _ => {}
  }
  Ok(())
}

/// `S:limit` as `svn_cstring_atoi` reads it: a decimal `int`, refused otherwise.
fn limit(child: &Element) -> Result<i32, DavRefusal> {
  let text = child.trimmed_text();
  text.parse().map_err(|_| DavRefusal::Malformed {
    element: "limit",
    text,
  })
}

/// Adds a `S:revprop` name, unless all were asked for already.
fn revprop(revprops: &mut Revprops, name: String) {
  match revprops {
    Revprops::All => {}
    Revprops::Named(named) => named.push(name),
    Revprops::Default => *revprops = Revprops::Named(vec![name]),
  }
}

/// Reads a `dated-rev-report`: the last `DAV:creationdate` that parses.
fn dated_rev_report(root: &Element) -> Result<DatedRevReport, DavRefusal> {
  root
    .elements()
    .filter(|child| child.is(DAV, "creationdate"))
    .filter_map(|child| svn_time(&child.trimmed_text()))
    .last()
    .map(|date| DatedRevReport { date })
    .ok_or(DavRefusal::Missing {
      element: "creationdate",
    })
}

/// Reads a `get-locations` report.
fn get_locations(root: &Element) -> Result<GetLocationsReport, DavRefusal> {
  let mut path = None;
  let mut peg = None;
  let mut location_revisions = Vec::new();
  for child in svn_children(root) {
    match child.name.as_str() {
      "peg-revision" => peg = revision_of(child, "peg-revision")?,
      "location-revision" => location_revisions.push(revision_of(child, "location-revision")?),
      "path" => path = Some(canonical(child.text())?),
      _ => {}
    }
  }
  let path = path.ok_or(DavRefusal::Missing { element: "path" })?;
  let peg_revision = peg.ok_or(DavRefusal::Missing {
    element: "peg-revision",
  })?;
  Ok(GetLocationsReport {
    path,
    peg_revision,
    location_revisions,
  })
}

/// Reads a `get-location-segments` report.
fn get_location_segments(root: &Element) -> Result<GetLocationSegmentsReport, DavRefusal> {
  let mut path = None;
  let mut report = GetLocationSegmentsReport {
    path: String::new(),
    peg_revision: None,
    start: None,
    end: None,
  };
  for child in svn_children(root) {
    match child.name.as_str() {
      "peg-revision" => report.peg_revision = revision_of(child, "peg-revision")?,
      "start-revision" => report.start = revision_of(child, "start-revision")?,
      "end-revision" => report.end = revision_of(child, "end-revision")?,
      "path" => path = Some(canonical(child.text())?),
      _ => {}
    }
  }
  report.path = path.ok_or(DavRefusal::Missing { element: "path" })?;
  Ok(report)
}

/// Reads a `get-locks-report`: its `depth` attribute, refused unless empty, files, immediates or infinity.
fn get_locks(root: &Element) -> Result<GetLocksReport, DavRefusal> {
  let depth = root
    .attribute("depth")
    .map_or(SvnDepth::Infinity, SvnDepth::from_word);
  if matches!(depth, SvnDepth::Exclude | SvnDepth::Unknown) {
    return Err(DavRefusal::Malformed {
      element: "depth",
      text: root.attribute("depth").unwrap_or("").to_owned(),
    });
  }
  Ok(GetLocksReport { depth })
}

/// Reads a `mergeinfo-report`.
fn mergeinfo(root: &Element) -> Result<MergeinfoReport, DavRefusal> {
  let mut report = MergeinfoReport {
    revision: None,
    inherit: Inherit::Explicit,
    include_descendants: false,
    paths: Vec::new(),
  };
  for child in svn_children(root) {
    match child.name.as_str() {
      "revision" => report.revision = revision_of(child, "revision")?,
      "inherit" => report.inherit = inherit(&child.trimmed_text()),
      "path" => report.paths.push(canonical(child.text())?),
      "include-descendants" => report.include_descendants = child.trimmed_text() == "yes",
      _ => {}
    }
  }
  Ok(report)
}

/// The inheritance a word names (`svn_inheritance_from_word`): explicit for every word it does not know.
fn inherit(word: &str) -> Inherit {
  match word {
    "inherited" => Inherit::Inherited,
    "nearest-ancestor" => Inherit::NearestAncestor,
    _ => Inherit::Explicit,
  }
}

/// Reads a `get-deleted-rev-report`: all three parameters, or "Not all parameters passed".
fn get_deleted_rev(root: &Element) -> Result<GetDeletedRevReport, DavRefusal> {
  let (mut path, mut peg, mut end) = (None, None, None);
  for child in svn_children(root) {
    match child.name.as_str() {
      "peg-revision" => peg = revision_of(child, "peg-revision")?,
      "end-revision" => end = revision_of(child, "end-revision")?,
      "path" => path = Some(canonical(child.text())?),
      _ => {}
    }
  }
  Ok(GetDeletedRevReport {
    path: path.ok_or(DavRefusal::Missing { element: "path" })?,
    peg_revision: peg.ok_or(DavRefusal::Missing {
      element: "peg-revision",
    })?,
    end_revision: end.ok_or(DavRefusal::Missing {
      element: "end-revision",
    })?,
  })
}

/// Reads an `inherited-props-report`.
fn inherited_props(root: &Element) -> Result<InheritedPropsReport, DavRefusal> {
  let mut report = InheritedPropsReport {
    revision: None,
    path: String::new(),
  };
  for child in svn_children(root) {
    match child.name.as_str() {
      "revision" => report.revision = revision_of(child, "revision")?,
      "path" => report.path = canonical(child.text())?,
      _ => {}
    }
  }
  Ok(report)
}

/// Reads a `replay-report`: `S:revision` at most once, and `S:low-water-mark` required.
fn replay(root: &Element) -> Result<ReplayReport, DavRefusal> {
  let mut revision_given: Option<RevisionArg> = None;
  let mut low_water_mark = None;
  let mut send_deltas = true;
  let mut include_path = None;
  for child in svn_children(root) {
    match child.name.as_str() {
      "revision" if revision_given.is_some() => return Err(malformed(child, "revision")),
      "revision" => revision_given = Some(revision_of(child, "revision")?),
      "low-water-mark" => low_water_mark = revision_of(child, "low-water-mark")?,
      "send-deltas" => send_deltas = send_deltas_of(child)?,
      "include-path" => include_path = Some(canonical(child.trimmed_text())?),
      _ => {}
    }
  }
  Ok(ReplayReport {
    revision: revision_given.flatten(),
    low_water_mark: low_water_mark.ok_or(DavRefusal::Missing {
      element: "low-water-mark",
    })?,
    send_deltas,
    include_path,
  })
}

/// The refusal of `element` as malformed, with its text.
fn malformed(child: &Element, element: &'static str) -> DavRefusal {
  DavRefusal::Malformed {
    element,
    text: child.trimmed_text(),
  }
}

/// `S:send-deltas` as `svn_cstring_strtoi64` reads it within 0 and 1: true unless 0.
fn send_deltas_of(child: &Element) -> Result<bool, DavRefusal> {
  match child.trimmed_text().as_str() {
    "0" => Ok(false),
    "1" => Ok(true),
    _ => Err(malformed(child, "send-deltas")),
  }
}

/// A MERGE's `S:lock-token-list`, found as `mod_dav_svn` finds it (the root itself, or an `svn:` child of it), each
/// `lock`'s `lock-path` and `lock-token` paired as it pairs them: by local name in any namespace, as ra_serf writes
/// both unprefixed, a pair taken once both are there and the token is not empty.
fn lock_token_list(root: &Element) -> Result<Vec<(String, String)>, DavRefusal> {
  let list = if root.is(SVN, "lock-token-list") {
    Some(root)
  } else {
    svn_children(root).find(|child| child.name == "lock-token-list")
  };
  let mut pairs = Vec::new();
  for lock in list
    .into_iter()
    .flat_map(Element::elements)
    .filter(|lock| lock.name == "lock")
  {
    lock_pairs(lock, &mut pairs)?;
  }
  Ok(pairs)
}

/// The path and token pairs of one `lock`, in the order `version.c` takes them.
fn lock_pairs(lock: &Element, pairs: &mut Vec<(String, String)>) -> Result<(), DavRefusal> {
  let (mut path, mut token): (Option<String>, Option<String>) = (None, None);
  for field in lock.elements() {
    match field.name.as_str() {
      "lock-path" => path = Some(canonical(field.text())?),
      "lock-token" => token = Some(field.trimmed_text()),
      _ => continue,
    }
    if let (Some(found_path), Some(found_token)) = (&path, &token)
      && !found_token.is_empty()
    {
      pairs.push((found_path.clone(), found_token.clone()));
      path = None;
      token = None;
    }
  }
  Ok(())
}

/// A date in the form `svn_time_to_cstring` writes, `YYYY-MM-DDTHH:MM:SS.uuuuuuZ`, as microseconds since the epoch;
/// `None` for any other form, or a date before 1970.
pub(crate) fn svn_time(text: &str) -> Option<AprTime> {
  let (date, clock) = text.strip_suffix('Z')?.split_once('T')?;
  let (year, month, day) = three_fields(date, '-')?;
  let (whole, fraction) = clock.split_once('.')?;
  let (hour, minute, second) = three_fields(whole, ':')?;
  if fraction.len() != FRACTION_DIGITS {
    return None;
  }
  let micros = field(fraction)?;
  let days = super::time::days_from_civil(year, month, day)?;
  let seconds = super::time::seconds_of_day(hour, minute, second)?;
  let total = days
    .checked_mul(super::time::SECONDS_PER_DAY)?
    .checked_add(seconds)?
    .checked_mul(MICROSECONDS_PER_SECOND)?
    .checked_add(micros)?;
  i64::try_from(total).ok().map(AprTime)
}

/// Exactly three fields of `text` between `separator`s, each decimal digits only: a date's year, month and day, or a
/// time's hour, minute and second.
fn three_fields(text: &str, separator: char) -> Option<(u64, u64, u64)> {
  let mut fields = text.split(separator);
  let first = field(fields.next()?)?;
  let second = field(fields.next()?)?;
  let third = field(fields.next()?)?;
  fields.next().is_none().then_some((first, second, third))
}

/// One field of a date: decimal digits only.
fn field(text: &str) -> Option<u64> {
  if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
    return None;
  }
  text.parse().ok()
}
