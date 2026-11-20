//! A `getbundle`'s answer (A-619 part 2; §4.17.3 "Pull and clone" items 2 to 4): the bundle2 stream 7.2's
//! `exchange.getbundlechunks` writes for a client's arguments, assembled through this crate's codecs over a [`Store`],
//! the served view with the revisions it names.
//!
//! **The parts**, in 7.2's order (`getbundle2partsorder`), each only where asked: `CHANGEGROUP` (the highest version
//! the client's bundle2 capabilities list among `02` and `03`, its `version` mandatory and `nbchanges` advisory; none
//! when nothing is missing, or when the client named no bundle2 capability at all), `BOOKMARKS` (`listbinbookmarks`;
//! none when there are none), a `LISTKEYS` part for each namespace asked, `OBSMARKERS` (the markers relevant to the
//! heads' ancestors, in the newest format both sides read; none when there are none) and `PHASE-HEADS`. A part the
//! client asked for and cannot read (bookmarks, phases or markers absent from its capabilities, no common changegroup
//! version) is refused, as 7.2 aborts; the version is chosen before anything is found missing, as 7.2 chooses it. A
//! client lists its bundle2 capabilities in `bundle2=` bundle capabilities, every one read and a later one's
//! capability replacing an earlier one's (`b2caps.update`); one that names no bundle capability starting `HG2` asks
//! for a bundle1 (`bundle2requested`), and one that lists no changegroup version (3.1 and 3.2 sent the capability
//! empty) for version 01: neither is served here. `HGTAGSFNODES`, advisory, is never sent: 7.2 sends only what its tag
//! cache already holds, and a client computes what it lacks. An unknown head is refused before any part, where 7.2
//! fails inside the first part that reads it.
//!
//! **The changegroup** (7.2's `cgpacker` on a served view, the path that reads the manifests): the missing changesets
//! oldest first; every manifest they name, once, linked to the first missing changeset that names it, none left out
//! (`_prunemanifests` keeps every one outside an ellipsis clone), in the manifest log's order; then for each path a
//! missing changeset lists, by path, the file revisions the manifests of the missing changesets that list it name
//! there, each linked to the first of them, those whose own file-log link the client holds left out
//! (`generatefiles`), in the file log's order. 7.2 finds those file revisions by reading each manifest's delta against
//! a parent (`read_delta_parents(exact=False)`, the whole manifest where its revlog stored the delta against another
//! revision), which yields the same revisions linked the same way, and besides them, now and then, one the client
//! already holds through a changeset other than the one its file log links it to, which 7.2 sends again and this
//! module does not: the client keeps one copy either way. Every revision travels as its full text against the null
//! base, which every reader takes (`mdiff.trivialdiffheader`); held deltas and computed ones are the learned delta
//! choice's (§4.17.3's tunables).
//!
//! **Made a slice at a time** (A-745; §4.17.1 "A fetch's answer"). [`Answering`] writes the answer in steps, each given
//! the served view and a room of its own: the stream's head, then the changegroup a unit a step ([`Walk`]), then the
//! parts after it. A fetch that leaves no served changeset neither sent nor held, over a view that serves every
//! changeset (a clone of every head, the large case), is walked by its links, 7.2's own fast path (`fastpathlinkrev`):
//! the missing changesets as ranges, the manifest log in its order and then the file logs from row to row, each
//! revision whose log links it to a missing changeset, so nothing grows with the repository but the walk's place. Any
//! other fetch (a pull of some heads, a view that hides changesets) takes the slow path below, bounded by what it sends.
//!
//! **Texts lent** (A-680). The store lends each text in place where it holds it in one piece and copies it otherwise,
//! the copy charged to the request's admission before it is made ([`Store`]). Each missing changeset's text is lent
//! once and kept in its entry, whose list of paths the files are read from; each manifest is lent once, checked whole,
//! and each path a changeset lists is found in its text by halving ([`manifest::find`]), so no manifest is read again
//! for a path.
//!
//! **Relevant markers** (`obsstore.relevantmarkers`): those naming one of the nodes as a successor, prunes naming one
//! as predecessor or parent, and then, again and again, those of each found marker's predecessor; sorted as
//! `obsutil.sortedmarkers` sorts 7.2's marker tuples: by predecessor, successors, flags, metadata, date and parents,
//! a marker without parent data ordered as one with none. 7.2 lifts a format 0 marker's date and parents out of its
//! metadata before it sorts; a format 0 marker held here sorts by its metadata as held (7.2 writes format 1).

use super::pull::{self, Outgoing};
use super::{Refusal, Repository, Rev, RevSet, node_of, parents_of};
use crate::bundle2::{Bundle2, Event, Param, Part};
use crate::capabilities::{self, Bundle2Capabilities};
use crate::changegroup::{self, Changegroup, Group, Revision, Version};
use crate::obsmarkers::{self, Format, Marker};
use crate::out::{Measure, Out};
use crate::read::to_u64;
use crate::{
  Admission, NULL_NODE, Node, PastAdmission, bookmarks, changeset, delta, manifest, phases,
  pushkey, quote,
};
use core::cmp::Ordering;
use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};

/// The served view with what its changesets name: their texts, and the manifest and file logs' revisions (A-619 part
/// 2). A text is lent (A-680): in place where the store holds it in one piece, else copied, the copy's bytes charged to
/// the request's room before it is made, so nothing a store copies is held past the request's admission.
pub trait Store: Repository {
  /// A changeset's text; `None` past the view, or where the store does not hold it whole.
  fn changeset_text(
    &self,
    rev: Rev,
    room: &mut Admission,
  ) -> Result<Option<Cow<'_, [u8]>>, PastAdmission>;

  /// The manifest revision of `node`; `None` where the store does not hold it whole.
  fn manifest(
    &self,
    node: &Node,
    room: &mut Admission,
  ) -> Result<Option<Logged<'_>>, PastAdmission>;

  /// The revision of the file at `path` whose node is `node`; `None` where the store does not hold it whole.
  fn file(
    &self,
    path: &[u8],
    node: &Node,
    room: &mut Admission,
  ) -> Result<Option<Logged<'_>>, PastAdmission>;

  /// The repository's obsolescence markers, in any order.
  fn markers(&self) -> impl Iterator<Item = Marker<'_>>;

  /// The manifest log's revision at `position` (A-745: a fetch walks the log in its order): its node, and the served
  /// revision of the changeset the log links it to; `None` past the log's end.
  fn manifest_linked(&self, position: u64) -> Option<Linked>;

  /// The first file revision at or after `from` (a path and a position in its log) in the file logs' order, by path
  /// and then position (A-745): its path, its position, its node and its link; `None` past the last file log.
  fn file_linked_from(&self, from: (&[u8], u64)) -> Option<(Vec<u8>, u64, Linked)>;

  /// Whether the manifest log holds `node` (A-745: a changeset sent is held to name a manifest the log holds).
  fn holds_manifest(&self, node: &Node) -> bool;

  /// Whether the view serves every changeset its changelog holds (A-745): then every link names a served changeset,
  /// and a fetch that leaves none neither sent nor held is walked by its links.
  fn serves_every_changeset(&self) -> bool;

  /// Whether a stream clone is served (stream clones part 2): a view over a repository's index that holds no secret
  /// changeset, as 7.2's `allowservergeneration` refuses a store with one. None by default.
  fn streams(&self) -> bool {
    false
  }

  /// The revlogs a stream clone sends ([`crate::render::revlogs`]), charged to `room`; `None` where none is served
  /// ([`Store::streams`]).
  fn stream_files(
    &self,
    room: &mut Admission,
  ) -> Result<Option<Vec<crate::render::File>>, Refusal> {
    let _ = room;
    Ok(None)
  }
}

/// A manifest or file revision as a fetch walks its log (A-745): its node, and the served revision of the changeset its
/// log links it to, none where the view does not serve that changeset.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Linked {
  /// The node.
  pub node: Node,
  /// The served revision of the changeset its log links it to (its linkrev).
  pub link: Option<Rev>,
}

/// A manifest or file revision as its log keeps it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Logged<'a> {
  /// Its position in its log: what orders a group.
  pub position: u64,
  /// Its parents, the null node for each that is missing.
  pub parents: [Node; 2],
  /// The changeset its log links it to.
  pub link: Node,
  /// Its revision flags.
  pub flags: u16,
  /// Its text, lent.
  pub text: Cow<'a, [u8]>,
}

/// What a `getbundle` asks, its arguments as the wire carried them (`wireprotov1server.getbundle`): `heads` and
/// `common` node lists, `bundlecaps` and `listkeys` comma-separated, and the booleans and `listkeys` absent where not
/// sent.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Asked<'a> {
  /// The heads asked for.
  pub heads: &'a [u8],
  /// The nodes the client holds.
  pub common: &'a [u8],
  /// The client's bundle capabilities.
  pub bundlecaps: &'a [u8],
  /// Whether to send the changegroup (sent unless `0`).
  pub cg: Option<&'a [u8]>,
  /// Whether to send the phase heads.
  pub phases: Option<&'a [u8]>,
  /// Whether to send the bookmarks.
  pub bookmarks: Option<&'a [u8]>,
  /// Whether to send the obsolescence markers.
  pub obsmarkers: Option<&'a [u8]>,
  /// The pushkey namespaces to list, every comma-separated item one, an empty one too (7.2's `csv`).
  pub listkeys: Option<&'a [u8]>,
}

/// Format: the bundle capability that carries the bundle2 ones (`bundle2=<quoted blob>`).
const BUNDLE2_CAPABILITY: &[u8] = b"bundle2=";

/// Format: what a bundle capability starts with when the client asks for a bundle2 answer (`bundle2requested`).
const BUNDLE2_REQUESTED: &[u8] = b"HG2";

/// Format: the changegroup versions served, oldest first (§4.17.3 "Changegroup versions").
const SERVED_VERSIONS: [&[u8]; 2] = [b"02", b"03"];

/// Format: the marker formats served, newest first, with their capability names (`obsolete.commonversion`).
const MARKER_FORMATS: [(&[u8], Format); 2] = [(b"V1", Format::V1), (b"V0", Format::V0)];

/// Format: the comma that separates `bundlecaps` and `listkeys` items.
const COMMA: u8 = b',';

/// Format: a boolean argument's false (`gboptsmap`'s `boolean`: `0` is false, any other value true).
const FALSE: &[u8] = b"0";

/// Format: the line end that separates a changeset's files and a bundle2 capability blob's lines.
const NEWLINE: u8 = b'\n';

/// A part's parameter before it is framed: its key and its value.
type Parameter = (&'static [u8], Vec<u8>);

/// One part before it is framed: its type, its parameters (mandatory first) and its payload.
struct Built {
  /// The type, as mandatory parts are written: upper-case.
  kind: &'static [u8],
  /// The mandatory parameters.
  mandatory: Vec<Parameter>,
  /// The advisory parameters.
  advisory: Vec<Parameter>,
  /// The payload.
  payload: Vec<u8>,
}

/// Appends a `getbundle`'s answer for `asked`: the bundle2 stream of the parts asked for, nothing past the admission.
/// The client's bundle capabilities are judged before what is missing is looked for, in 7.2's order (`getbundle`'s
/// bundle2 check, then `getbundlechunks`' capabilities before its changegroup part), so a bundle1 request is refused as
/// one whatever it names, and its client is never sent a bundle2 abort (A-650).
pub fn getbundle(
  store: &impl Store,
  asked: &Asked<'_>,
  admission: &mut Admission,
  out: &mut Vec<u8>,
) -> Result<(), Refusal> {
  let mut answering = Answering::begin(store, asked, admission)?;
  while answering.step(store, admission, out)? {}
  Ok(())
}

/// A `getbundle`'s answer made a slice at a time (A-745; §4.17.1 "A fetch's answer"): what [`getbundle`] writes whole,
/// cut into steps its owner advances, each lent the served view anew and charged to the room it is given. A step writes
/// the stream's head and the changegroup part's header, or one unit of the changegroup (a revision, a group's end) as
/// one payload chunk, or the parts after it and the stream's end. What the answer holds across steps is the parts after
/// the changegroup (made when it begins: bookmarks, `listkeys`, markers, phase heads), the missing changesets as ranges
/// ([`pull::Ranges`]) and its place in the changegroup ([`Walk`]); never a revision's text past its step.
pub struct Answering {
  /// The stream's head and the changegroup part's header, until the first step writes them.
  head: Option<Vec<u8>>,
  /// The changegroup, while one is sent.
  walk: Option<Walk>,
  /// The parts after the changegroup and the stream's end, until the last step writes them.
  tail: Option<Vec<u8>>,
}

impl Answering {
  /// Begins the answer to `asked` over `store` (the module docs' parts, in 7.2's order), what it holds charged to
  /// `admission`; refused before anything is written, as 7.2 refuses before its first part.
  pub fn begin(
    store: &impl Store,
    asked: &Asked<'_>,
    admission: &mut Admission,
  ) -> Result<Answering, Refusal> {
    let mut unquoted = Vec::new();
    let caps = bundle2_capabilities(asked.bundlecaps, admission, &mut unquoted)?;
    let outgoing = pull::outgoing(store, asked.heads, asked.common, admission)?;
    let changegroup = changegroup_part(&outgoing, asked.cg, &caps)?;
    let mut parts = Vec::new();
    if flag(asked.bookmarks, false) {
      parts.extend(bookmarks_part(store, &caps, admission)?);
    }
    listkeys_parts(store, asked.listkeys, admission, &mut parts)?;
    if flag(asked.obsmarkers, false) {
      parts.extend(markers_part(store, &outgoing.heads, &caps, admission)?);
    }
    if flag(asked.phases, false) {
      parts.push(phases_part(store, &outgoing.heads, &caps, admission)?);
    }
    let mut head = Vec::new();
    crate::bundle2::put_stream_head(&mut head);
    let first = match &changegroup {
      Some(built) => {
        let header = built_part(built, 0);
        crate::bundle2::put_part_header(&header, &mut head).map_err(|_| unencodable("a part"))?;
        1
      }
      None => 0,
    };
    let tail = frame_after(&parts, first, admission)?;
    admission.charge(to_u64(head.len()))?;
    let walk = match changegroup {
      Some(_) => Some(Walk::begin(
        store,
        &outgoing,
        version_of(&caps)?,
        admission,
      )?),
      None => None,
    };
    Ok(Answering {
      head: Some(head),
      walk,
      tail: Some(tail),
    })
  }

  /// Appends the answer's next slice to `out`, what it makes charged to `room`: whether more follow. Refused past the
  /// head, the answer cannot be finished, and its owner ends the stream unfinished.
  pub fn step(
    &mut self,
    store: &impl Store,
    room: &mut Admission,
    out: &mut Vec<u8>,
  ) -> Result<bool, Refusal> {
    if let Some(head) = self.head.take() {
      out.extend_from_slice(&head);
      return Ok(true);
    }
    if let Some(walk) = &mut self.walk {
      let mut chunk = Vec::new();
      let more = walk.step(store, room, &mut chunk)?;
      if !chunk.is_empty() {
        crate::bundle2::put_payload_chunk(out, &chunk).map_err(|_| unencodable("a chunk"))?;
      }
      if !more {
        crate::bundle2::put_payload_chunk(out, &[]).map_err(|_| unencodable("a chunk"))?;
        self.walk = None;
      }
      return Ok(true);
    }
    if let Some(tail) = self.tail.take() {
      out.extend_from_slice(&tail);
    }
    Ok(false)
  }
}

/// A boolean argument as 7.2 reads one: `default` where absent, false for `0` or nothing, true otherwise.
fn flag(value: Option<&[u8]>, default: bool) -> bool {
  value.map_or(default, |value| !value.is_empty() && value != FALSE)
}

/// The client's bundle2 capabilities: every `bundle2=` bundle capability's blob, unquoted and joined line after line,
/// so a later one's capability replaces an earlier one's as 7.2's `update` replaces it. A client that names no bundle
/// capability starting `HG2` asks for a bundle1, which is refused.
fn bundle2_capabilities<'b>(
  bundlecaps: &[u8],
  admission: &mut Admission,
  unquoted: &'b mut Vec<u8>,
) -> Result<Bundle2Capabilities<'b>, Refusal> {
  let items = || bundlecaps.split(|byte| *byte == COMMA);
  if !items().any(|item| item.starts_with(BUNDLE2_REQUESTED)) {
    return Err(Refusal::NoBundle2);
  }
  let blobs = || items().filter_map(|item| item.strip_prefix(BUNDLE2_CAPABILITY));
  let mut length = Measure(0);
  for blob in blobs() {
    quote::unquote(blob, &mut length);
    length.put(&[NEWLINE]);
  }
  admission.charge(length.0)?;
  let mut joined = Vec::with_capacity(usize::try_from(length.0).unwrap_or(0));
  for blob in blobs() {
    quote::unquote(blob, &mut joined);
    joined.push(NEWLINE);
  }
  Ok(capabilities::decode_bundle2(&joined, admission, unquoted)?)
}

/// Whether the client's bundle2 capability `name` holds `value` (or exists, for `None`).
fn offers(caps: &Bundle2Capabilities<'_>, name: &[u8], value: Option<&[u8]>) -> bool {
  caps
    .get(name)
    .is_some_and(|values| value.is_none_or(|value| values.contains(&value)))
}

/// The `CHANGEGROUP` part where it is asked (sent unless `cg` is false) and the client named bundle2 capabilities:
/// the version first, then none where nothing is missing; else its header (the version and the count of changesets),
/// its payload made a step at a time ([`Walk`]).
fn changegroup_part(
  outgoing: &Outgoing,
  cg: Option<&[u8]>,
  caps: &Bundle2Capabilities<'_>,
) -> Result<Option<Built>, Refusal> {
  if !flag(cg, true) || caps.iter().next().is_none() {
    return Ok(None);
  }
  let (name, _) = changegroup_version(caps)?;
  if outgoing.missing.is_empty() {
    return Ok(None);
  }
  Ok(Some(Built {
    kind: b"CHANGEGROUP",
    mandatory: vec![(&b"version"[..], name.to_vec())],
    advisory: vec![(
      &b"nbchanges"[..],
      outgoing.missing.len().to_string().into_bytes(),
    )],
    payload: Vec::new(),
  }))
}

/// The changegroup version the client and this server share ([`changegroup_version`]'s).
fn version_of(caps: &Bundle2Capabilities<'_>) -> Result<Version, Refusal> {
  changegroup_version(caps).map(|(_, version)| version)
}

/// The highest served changegroup version the client lists.
fn changegroup_version(
  caps: &Bundle2Capabilities<'_>,
) -> Result<(&'static [u8], Version), Refusal> {
  let name = SERVED_VERSIONS
    .into_iter()
    .rev()
    .find(|version| offers(caps, b"changegroup", Some(*version)))
    .ok_or_else(no_common_version)?;
  let version = Version::from_name(name).map_err(|_| no_common_version())?;
  Ok((name, version))
}

/// The refusal of a client that lists no served changegroup version.
fn no_common_version() -> Refusal {
  Refusal::NoCommonExchange {
    what: "changegroup version",
  }
}

/// A `LISTKEYS` part for each namespace asked: the argument split at every comma, as 7.2 splits a `csv` argument, so
/// an empty item is a namespace too, one that lists nothing.
fn listkeys_parts(
  store: &impl Store,
  listkeys: Option<&[u8]>,
  admission: &mut Admission,
  parts: &mut Vec<Built>,
) -> Result<(), Refusal> {
  for namespace in listkeys
    .into_iter()
    .flat_map(|list| list.split(|byte| *byte == COMMA))
  {
    admission.charge_items::<Built>(1)?;
    admission.charge(to_u64(namespace.len()))?;
    let mut payload = Vec::new();
    pull::listkeys(store, namespace, admission, &mut payload)?;
    parts.push(Built {
      kind: b"LISTKEYS",
      mandatory: vec![(&b"namespace"[..], namespace.to_vec())],
      advisory: Vec::new(),
      payload,
    });
  }
  Ok(())
}

/// The `BOOKMARKS` part, none where the repository has no bookmark the served view holds.
fn bookmarks_part(
  store: &impl Store,
  caps: &Bundle2Capabilities<'_>,
  admission: &mut Admission,
) -> Result<Option<Built>, Refusal> {
  if !offers(caps, b"bookmarks", None) {
    return Err(Refusal::NoCommonExchange { what: "bookmarks" });
  }
  let entries = pull::bookmark_entries(store, admission)?;
  let len = bookmarks::encoded_len(&entries).map_err(|_| unencodable("a bookmark"))?;
  admission.charge(len)?;
  let mut payload = Vec::new();
  bookmarks::encode(&entries, &mut payload).map_err(|_| unencodable("a bookmark"))?;
  Ok((!payload.is_empty()).then_some(Built {
    kind: b"BOOKMARKS",
    mandatory: Vec::new(),
    advisory: Vec::new(),
    payload,
  }))
}

/// The `PHASE-HEADS` part for `heads`.
fn phases_part(
  store: &impl Store,
  heads: &[Rev],
  caps: &Bundle2Capabilities<'_>,
  admission: &mut Admission,
) -> Result<Built, Refusal> {
  if !offers(caps, b"phases", Some(b"heads")) {
    return Err(Refusal::NoCommonExchange { what: "phases" });
  }
  let entries = pull::phase_heads(store, heads, admission)?;
  admission.charge(phases::encoded_len(&entries))?;
  let mut payload = Vec::new();
  phases::encode(&entries, &mut payload);
  Ok(Built {
    kind: b"PHASE-HEADS",
    mandatory: Vec::new(),
    advisory: Vec::new(),
    payload,
  })
}

/// The `OBSMARKERS` part: the markers relevant to the heads' ancestors in the newest format both read; none where
/// there are none.
fn markers_part(
  store: &impl Store,
  heads: &[Rev],
  caps: &Bundle2Capabilities<'_>,
  admission: &mut Admission,
) -> Result<Option<Built>, Refusal> {
  let markers = relevant_markers(store, heads, admission)?;
  if markers.is_empty() {
    return Ok(None);
  }
  let format = MARKER_FORMATS
    .into_iter()
    .find(|(name, _)| offers(caps, b"obsmarkers", Some(*name)))
    .map(|(_, format)| format)
    .ok_or(Refusal::NoCommonExchange {
      what: "obsolescence marker format",
    })?;
  let len = obsmarkers::encoded_len(format, &markers).map_err(|_| unencodable("a marker"))?;
  admission.charge(len)?;
  let mut payload = Vec::new();
  obsmarkers::encode(format, &markers, &mut payload).map_err(|_| unencodable("a marker"))?;
  Ok(Some(Built {
    kind: b"OBSMARKERS",
    mandatory: Vec::new(),
    advisory: Vec::new(),
    payload,
  }))
}

/// An encoder's refusal of what the store holds.
fn unencodable(what: &'static str) -> Refusal {
  Refusal::Unencodable { what }
}

/// A built part's header as part `id` of the stream.
fn built_part(built: &Built, id: u32) -> Part<'_> {
  let params = built
    .mandatory
    .iter()
    .chain(&built.advisory)
    .map(|(key, value)| Param {
      key,
      value: value.as_slice(),
    })
    .collect();
  Part {
    kind: built.kind,
    id,
    params,
    mandatory_params: built.mandatory.len(),
    depth: 0,
  }
}

/// The parts after the changegroup, numbered from `first`, and the stream's end, their length charged before they are
/// written.
fn frame_after(parts: &[Built], first: u32, admission: &mut Admission) -> Result<Vec<u8>, Refusal> {
  // Format: a payload chunk's length is a signed 32-bit field, so a chunk holds at most `i32::MAX` bytes.
  let largest = usize::try_from(i32::MAX).unwrap_or(usize::MAX);
  let mut stream = Bundle2 {
    params: Vec::new(),
    parts: Vec::with_capacity(parts.len()),
    events: Vec::new(),
  };
  for (index, built) in parts.iter().enumerate() {
    let id = first.saturating_add(u32::try_from(index).unwrap_or(u32::MAX));
    stream.parts.push(built_part(built, id));
    stream.events.push(Event::Open(index));
    stream
      .events
      .extend(built.payload.chunks(largest).map(Event::Chunk));
    stream.events.push(Event::Close);
  }
  let len = stream.encoded_len().map_err(|_| unencodable("a part"))?;
  admission.charge(len)?;
  let mut out = Vec::new();
  stream
    .encode_parts(&mut out)
    .map_err(|_| unencodable("a part"))?;
  Ok(out)
}

/// Whether a fetch of `outgoing` is walked by its links ([`Walk::begin`]): the view serves every changeset, and every
/// one it serves is sent or descends to a common revision, which a bit each of the view, let go after, tells.
fn by_links(
  store: &impl Store,
  outgoing: &Outgoing,
  admission: &mut Admission,
) -> Result<bool, Refusal> {
  if !store.serves_every_changeset() {
    return Ok(false);
  }
  let held = common_ancestors(store, &outgoing.common, admission)?;
  let held_count = (0..store.count()).filter(|rev| held.contains(*rev)).count();
  let covered = to_u64(held_count).saturating_add(outgoing.missing.len());
  Ok(covered == u64::from(store.count()))
}

/// The changegroup of `outgoing` in `version`, its payload whole: [`Walk`]'s steps at once.
pub fn changegroup(
  store: &impl Store,
  outgoing: &Outgoing,
  version: Version,
  admission: &mut Admission,
) -> Result<Vec<u8>, Refusal> {
  let mut walk = Walk::begin(store, outgoing, version, admission)?;
  let mut out = Vec::new();
  while walk.step(store, admission, &mut out)? {}
  Ok(out)
}

/// Where a changegroup's walk stands (A-745).
#[derive(Clone, Debug, PartialEq, Eq)]
enum At {
  /// The changelog's group, from this served revision.
  Changelog(Rev),
  /// The manifest log, from this position.
  Manifests(u64),
  /// The file logs, from this path and position; the path whose group is open, its name written.
  Files {
    /// The path the walk resumes at.
    path: Vec<u8>,
    /// The position in its log.
    position: u64,
    /// The path whose group is open.
    open: Option<Vec<u8>>,
  },
}

/// A changegroup made a unit at a time from what the store holds (A-745; 7.2's `cgpacker` with its link-revision fast
/// path, `fastpathlinkrev`): the missing changesets oldest first; the manifest log in its order, each revision whose
/// link is a missing changeset, linked to it; then each file log in its path's order, each revision whose link is a
/// missing changeset. A revision travels as its full text against the null base, which every reader takes
/// (`mdiff.trivialdiffheader`). The walk holds its place alone: the missing changesets as ranges, and the log and
/// position next. It never collects the paths the changesets touch: the file logs are walked from one to the next
/// ([`Store::file_linked_from`]), and a log whose revisions no missing changeset links sends no group.
pub struct Walk {
  /// The changesets the client lacks.
  missing: pull::Ranges,
  /// The version written.
  version: Version,
  /// Where the walk stands; none once the changegroup's end is written.
  at: Option<At>,
  /// A fetch the links cannot walk ([`named_changegroup`]): its changegroup, made whole when it began, written in one
  /// step.
  named: Option<Vec<u8>>,
}

impl Walk {
  /// A walk of the changegroup of `outgoing` in `version`, at its start: by the links where the fetch leaves no served
  /// changeset neither sent nor held and the view serves every changeset (7.2's `fastpathlinkrev`, which it takes for
  /// a fetch of every head), else by the changesets that name each revision, made whole within `admission`.
  pub fn begin(
    store: &impl Store,
    outgoing: &Outgoing,
    version: Version,
    admission: &mut Admission,
  ) -> Result<Walk, Refusal> {
    let mut walk = Walk {
      missing: outgoing.missing.clone(),
      version,
      at: Some(At::Changelog(0)),
      named: None,
    };
    if !by_links(store, outgoing, admission)? {
      walk.named = Some(named_changegroup(store, outgoing, version, admission)?);
      walk.at = None;
    }
    Ok(walk)
  }

  /// Appends the next unit of the changegroup to `out`, a text lent charged to `room`: whether more follow.
  pub fn step(
    &mut self,
    store: &impl Store,
    room: &mut Admission,
    out: &mut Vec<u8>,
  ) -> Result<bool, Refusal> {
    if let Some(named) = self.named.take() {
      out.extend_from_slice(&named);
      return Ok(false);
    }
    // A refused step leaves the walk where it was, so a step given more room takes the same unit again.
    let Some(at) = self.at.clone() else {
      return Ok(false);
    };
    self.at = match at {
      At::Changelog(from) => self.changeset(store, from, room, out)?,
      At::Manifests(position) => self.manifest(store, position, room, out)?,
      At::Files {
        path,
        position,
        open,
      } => self.file(store, (path, position), open, room, out)?,
    };
    Ok(self.at.is_some())
  }

  /// The missing changeset at or after `from`, or the changelog group's end.
  fn changeset(
    &self,
    store: &impl Store,
    from: Rev,
    room: &mut Admission,
    out: &mut Vec<u8>,
  ) -> Result<Option<At>, Refusal> {
    let Some(rev) = self.missing.next_from(from) else {
      changegroup::put_end(out);
      return Ok(Some(At::Manifests(0)));
    };
    let node = node_of(store, rev)?;
    let [first, second] = parents_of(store, rev)?;
    let parent =
      |parent: Option<Rev>| parent.map_or(Ok(NULL_NODE), |parent| node_of(store, parent));
    let parents = [parent(first)?, parent(second)?];
    let text = store
      .changeset_text(rev, room)?
      .ok_or_else(no_changeset_text)?;
    let (manifest, _) = read_changeset(&text)?;
    if manifest != NULL_NODE && !store.holds_manifest(&manifest) {
      return Err(no_manifest());
    }
    self.put((node, parents, node, 0), &text, room, out)?;
    Ok(Some(At::Changelog(rev.saturating_add(1))))
  }

  /// The manifest at `position` where a missing changeset links it, or the manifest group's end.
  fn manifest(
    &self,
    store: &impl Store,
    position: u64,
    room: &mut Admission,
    out: &mut Vec<u8>,
  ) -> Result<Option<At>, Refusal> {
    let Some(linked) = store.manifest_linked(position) else {
      changegroup::put_end(out);
      if self.version.has_tree_list() {
        changegroup::put_end(out);
      }
      return Ok(Some(At::Files {
        path: Vec::new(),
        position: 0,
        open: None,
      }));
    };
    let next = Some(At::Manifests(position.saturating_add(1)));
    let Some(link) = linked.link.filter(|link| self.missing.contains(*link)) else {
      return Ok(next);
    };
    let logged = store
      .manifest(&linked.node, room)?
      .ok_or_else(no_manifest)?;
    let link = node_of(store, link)?;
    self.put(
      (linked.node, logged.parents, link, logged.flags),
      &logged.text,
      room,
      out,
    )?;
    Ok(next)
  }

  /// The file revision at or after `from` where a missing changeset links it (its group's name first where it opens
  /// one, the open group's end first where it is another path's), or the file list's end.
  fn file(
    &self,
    store: &impl Store,
    (path, position): (Vec<u8>, u64),
    open: Option<Vec<u8>>,
    room: &mut Admission,
    out: &mut Vec<u8>,
  ) -> Result<Option<At>, Refusal> {
    let Some((found, position, linked)) = store.file_linked_from((&path, position)) else {
      if open.is_some() {
        changegroup::put_end(out);
      }
      changegroup::put_end(out);
      return Ok(None);
    };
    let next = |open| At::Files {
      path: found.clone(),
      position: position.saturating_add(1),
      open,
    };
    let Some(link) = linked.link.filter(|link| self.missing.contains(*link)) else {
      return Ok(Some(next(open)));
    };
    let logged = store
      .file(&found, &linked.node, room)?
      .ok_or(Refusal::Store {
        what: "a file revision",
      })?;
    if open.as_deref() != Some(found.as_slice()) {
      if open.is_some() {
        changegroup::put_end(out);
      }
      changegroup::put_name(&found, out).map_err(|_| unencodable("a path"))?;
    }
    let link = node_of(store, link)?;
    self.put(
      (linked.node, logged.parents, link, logged.flags),
      &logged.text,
      room,
      out,
    )?;
    Ok(Some(next(Some(found.clone()))))
  }

  /// Appends one revision (its node, parents, link and flags) as its full text against the null base, its bytes
  /// charged to `room` before they are written.
  fn put(
    &self,
    (node, parents, link, flags): (Node, [Node; 2], Node, u16),
    text: &[u8],
    room: &mut Admission,
    out: &mut Vec<u8>,
  ) -> Result<(), Refusal> {
    room.charge(to_u64(delta::HUNK_HEADER_LEN.saturating_add(text.len())))?;
    let mut data = Vec::new();
    delta::encode_full_text(text, &mut data).map_err(|_| unencodable("a full text"))?;
    let revision = Revision {
      node,
      p1: parents[0],
      p2: parents[1],
      base: NULL_NODE,
      link,
      flags: if self.version == Version::V02 {
        0
      } else {
        flags
      },
      info: None,
      protocol_flags: 0,
      data: &data,
      sidedata: None,
    };
    changegroup::put_revision(&revision, self.version, out).map_err(|_| unencodable("a revision"))
  }
}

/// What one revision of the changegroup carries before its delta is written.
struct Entry<'s> {
  /// Its node and parents.
  node: Node,
  /// Its parents.
  parents: [Node; 2],
  /// The changeset it is linked to in this changegroup.
  link: Node,
  /// Its flags.
  flags: u16,
  /// Its full text, lent by the store.
  text: Cow<'s, [u8]>,
}

/// The changegroup of `outgoing` in `version` by the changesets that name each revision (7.2's `cgpacker` off its
/// link-revision fast path), encoded whole: a fetch that leaves some served changeset neither sent nor held, or a view
/// that hides some, whose revisions a link names past what the client holds after it. Bounded by what the fetch
/// sends, never the repository. Each missing changeset's text is lent once and each manifest's once (A-680): the
/// changesets' entries hold their texts, which the paths they list are read from, and each path's file node is found
/// in the text of the manifest's entry ([`manifest::find`]).
pub(crate) fn named_changegroup(
  store: &impl Store,
  outgoing: &Outgoing,
  version: Version,
  admission: &mut Admission,
) -> Result<Vec<u8>, Refusal> {
  let held = common_ancestors(store, &outgoing.common, admission)?;
  let missing: Vec<Rev> = outgoing.missing.iter().collect();
  admission.charge_items::<Rev>(missing.len())?;
  let changelog = changelog_entries(store, &missing, admission)?;
  let (manifests, files) = named_revisions(store, &changelog, &held, admission)?;
  let mut groups: Vec<(&[u8], Vec<Entry<'_>>)> = Vec::with_capacity(files.len());
  groups.extend(files);
  encode_changegroup(version, &changelog, &manifests, &groups, admission)
}

/// The missing changesets' entries, oldest first, each linked to itself and holding its text, lent once.
fn changelog_entries<'s>(
  store: &'s impl Store,
  missing: &[Rev],
  admission: &mut Admission,
) -> Result<Vec<Entry<'s>>, Refusal> {
  admission.charge_items::<Entry<'_>>(missing.len())?;
  let mut entries = Vec::with_capacity(missing.len());
  for rev in missing {
    let node = node_of(store, *rev)?;
    let parent =
      |parent: Option<Rev>| parent.map_or(Ok(NULL_NODE), |parent| node_of(store, parent));
    let [first, second] = parents_of(store, *rev)?;
    let text = store
      .changeset_text(*rev, admission)?
      .ok_or_else(no_changeset_text)?;
    entries.push(Entry {
      node,
      parents: [parent(first)?, parent(second)?],
      link: node,
      flags: 0,
      text,
    });
  }
  Ok(entries)
}

/// A changeset's manifest node and the paths it touches, read from its text.
fn read_changeset(text: &[u8]) -> Result<(Node, &[u8]), Refusal> {
  let changeset = changeset::decode(text).map_err(|_| no_changeset_text())?;
  let manifest = pushkey::hex_node(changeset.manifest)
    .ok()
    .flatten()
    .ok_or(Refusal::Store {
      what: "a changeset's manifest node",
    })?;
  Ok((manifest, changeset.files))
}

/// The manifests a changegroup carries and its file groups by path, each path read from a changeset's text.
type Named<'t, 's> = (Vec<Entry<'s>>, BTreeMap<&'t [u8], Vec<Entry<'s>>>);

/// A revision found for a group, with its position in its log: what orders the group.
type Placed<'s> = (u64, Entry<'s>);

/// Every manifest the missing changesets name and, by path, the file revisions their manifests name at the paths they
/// list: each linked to the first missing changeset that names it, the file revisions whose own link the client holds
/// left out, each group in its log's order. Each manifest is lent once and checked whole before its paths are looked up
/// in it by halving.
fn named_revisions<'t, 's>(
  store: &'s impl Store,
  changelog: &'t [Entry<'s>],
  held: &RevSet,
  admission: &mut Admission,
) -> Result<Named<'t, 's>, Refusal> {
  let mut manifests: Vec<Placed<'s>> = Vec::new();
  let mut seen_manifests: BTreeMap<Node, usize> = BTreeMap::new();
  let mut files: BTreeMap<&'t [u8], Vec<Placed<'s>>> = BTreeMap::new();
  let mut seen_files = BTreeSet::new();
  for changeset in changelog {
    let link = changeset.node;
    let (manifest, touched) = read_changeset(&changeset.text)?;
    let at = match seen_manifests.get(&manifest) {
      Some(at) => *at,
      None => {
        let logged = store
          .manifest(&manifest, admission)?
          .ok_or_else(no_manifest)?;
        manifest::decode(&logged.text).map_err(|_| no_manifest())?;
        admission.charge_items::<(Placed<'_>, Node, usize)>(1)?;
        manifests.push((logged.position, entry(manifest, link, logged)));
        let at = manifests.len().saturating_sub(1);
        seen_manifests.insert(manifest, at);
        at
      }
    };
    let Some((_, kept)) = manifests.get(at) else {
      return Err(no_manifest());
    };
    for path in touched
      .split(|byte| *byte == NEWLINE)
      .filter(|path| !path.is_empty())
    {
      let Some(found) = manifest::find(&kept.text, path) else {
        continue;
      };
      if seen_files.insert((path, found.node)) {
        let logged = store
          .file(path, &found.node, admission)?
          .ok_or(Refusal::Store {
            what: "a file revision",
          })?;
        admission.charge_items::<(Placed<'_>, &[u8], Node)>(1)?;
        if !client_holds(store, held, &logged) {
          files
            .entry(path)
            .or_default()
            .push((logged.position, entry(found.node, link, logged)));
        }
      }
    }
  }
  manifests.sort_unstable_by_key(|(position, _)| *position);
  let files = files
    .into_iter()
    .map(|(path, mut revisions)| {
      revisions.sort_unstable_by_key(|(position, _)| *position);
      (
        path,
        revisions.into_iter().map(|(_, entry)| entry).collect(),
      )
    })
    .collect();
  Ok((
    manifests.into_iter().map(|(_, entry)| entry).collect(),
    files,
  ))
}

/// Whether the client holds a logged revision: its log's link is a changeset the common revisions descend from.
fn client_holds(store: &impl Store, held: &RevSet, logged: &Logged<'_>) -> bool {
  store
    .rev(&logged.link)
    .is_some_and(|rev| held.contains(rev))
}

/// A logged revision's entry, linked to `link`.
fn entry<'s>(node: Node, link: Node, logged: Logged<'s>) -> Entry<'s> {
  Entry {
    node,
    parents: logged.parents,
    link,
    flags: logged.flags,
    text: logged.text,
  }
}

/// Writes every entry's full-text delta into one buffer, charged first, and encodes the changegroup from it.
fn encode_changegroup(
  version: Version,
  changelog: &[Entry<'_>],
  manifests: &[Entry<'_>],
  files: &[(&[u8], Vec<Entry<'_>>)],
  admission: &mut Admission,
) -> Result<Vec<u8>, Refusal> {
  let all = || {
    changelog
      .iter()
      .chain(manifests)
      .chain(files.iter().flat_map(|(_, entries)| entries))
  };
  let len = all().fold(0_usize, |total, entry| {
    total
      .saturating_add(delta::HUNK_HEADER_LEN)
      .saturating_add(entry.text.len())
  });
  admission.charge(to_u64(len))?;
  let count = all().count();
  admission.charge_items::<(usize, Revision<'_>)>(count)?;
  let mut deltas = Vec::with_capacity(len);
  let mut ends = Vec::with_capacity(count);
  for entry in all() {
    delta::encode_full_text(&entry.text, &mut deltas).map_err(|_| unencodable("a full text"))?;
    ends.push(deltas.len());
  }
  let starts = core::iter::once(0).chain(ends.iter().copied());
  let revisions: Vec<Revision<'_>> = all()
    .zip(starts.zip(&ends))
    .map(|(entry, (start, end))| Revision {
      node: entry.node,
      p1: entry.parents[0],
      p2: entry.parents[1],
      base: NULL_NODE,
      link: entry.link,
      flags: if version == Version::V02 {
        0
      } else {
        entry.flags
      },
      info: None,
      protocol_flags: 0,
      data: deltas.get(start..*end).unwrap_or_default(),
      sidedata: None,
    })
    .collect();
  let manifests_end = changelog.len().saturating_add(manifests.len());
  let mut groups = Vec::with_capacity(files.len());
  let mut at = manifests_end;
  for (path, entries) in files {
    let end = at.saturating_add(entries.len());
    groups.push(Group {
      name: path,
      revisions: at..end,
    });
    at = end;
  }
  let changegroup = Changegroup {
    version,
    revisions,
    changelog: 0..changelog.len(),
    manifests: changelog.len()..manifests_end,
    trees: Vec::new(),
    files: groups,
  };
  let len = changegroup
    .encoded_len()
    .map_err(|_| unencodable("a changegroup"))?;
  admission.charge(len)?;
  let mut out = Vec::new();
  changegroup
    .encode(&mut out)
    .map_err(|_| unencodable("a changegroup"))?;
  Ok(out)
}

/// The revisions every one of `revs` descends from: themselves and all their ancestors, a bit each of the view.
fn common_ancestors(
  store: &impl Store,
  revs: &[Rev],
  admission: &mut Admission,
) -> Result<RevSet, Refusal> {
  let mut held = RevSet::new(store.count(), admission)?;
  let Some(top) = revs.iter().copied().max() else {
    return Ok(held);
  };
  for rev in revs {
    held.insert(*rev);
  }
  for rev in (0..=top).rev() {
    if held.contains(rev) {
      for parent in parents_of(store, rev)?.into_iter().flatten() {
        held.insert(parent);
      }
    }
  }
  Ok(held)
}

/// The refusal of a store that lacks a changeset's text, or holds one no reader takes.
fn no_changeset_text() -> Refusal {
  Refusal::Store {
    what: "a changeset's text",
  }
}

/// The refusal of a store that lacks a manifest revision, or holds one no reader takes.
fn no_manifest() -> Refusal {
  Refusal::Store {
    what: "a manifest revision",
  }
}

/// The markers relevant to the ancestors of `heads` (`obsstore.relevantmarkers`), sorted.
fn relevant_markers<'s>(
  store: &'s impl Store,
  heads: &[Rev],
  admission: &mut Admission,
) -> Result<Vec<Marker<'s>>, Refusal> {
  let all: Vec<Marker<'s>> = store.markers().collect();
  admission.charge_items::<Marker<'_>>(all.len())?;
  if all.is_empty() {
    return Ok(Vec::new());
  }
  let links = all.iter().fold(0_usize, |total, marker| {
    let parents = marker.parent_nodes().map_or(0, |parents| parents.len());
    total
      .saturating_add(marker.successor_nodes().len())
      .saturating_add(parents)
      .saturating_add(1)
  });
  admission.charge_items::<(&[u8], usize)>(links)?;
  let index = MarkerIndex::new(&all);
  let mut pending: BTreeSet<&[u8]> = BTreeSet::new();
  let mut nodes: Vec<Node> = Vec::new();
  let ancestors = common_ancestors(store, heads, admission)?;
  let held = (0..store.count()).filter(|rev| ancestors.contains(*rev));
  admission.charge_items::<(Node, &[u8], &[u8])>(held.clone().count())?;
  for rev in held {
    nodes.push(node_of(store, rev)?);
  }
  pending.extend(nodes.iter().map(|node| &node[..]));
  let mut seen_nodes = pending.clone();
  let mut found: BTreeSet<usize> = BTreeSet::new();
  while !pending.is_empty() {
    let direct: BTreeSet<usize> = pending
      .iter()
      .flat_map(|node| index.direct(node))
      .filter(|marker| !found.contains(marker))
      .collect();
    pending = direct
      .iter()
      .filter_map(|marker| all.get(*marker).map(|marker| marker.predecessor))
      .filter(|predecessor| !seen_nodes.contains(predecessor))
      .collect();
    seen_nodes.extend(pending.iter().copied());
    found.extend(direct);
  }
  let mut markers: Vec<Marker<'s>> = found
    .into_iter()
    .filter_map(|marker| all.get(marker).copied())
    .collect();
  markers.sort_by(marker_order);
  Ok(markers)
}

/// 7.2's order of two markers (`obsutil.sortedmarkers`): Python's order of their tuples, field after field, a node
/// list compared node by node, a date by its seconds (a NaN, which Python never orders, as equal) and then its
/// timezone, and a marker without parent data as one with none.
fn marker_order(left: &Marker<'_>, right: &Marker<'_>) -> Ordering {
  left
    .predecessor
    .cmp(right.predecessor)
    .then_with(|| left.successor_nodes().cmp(right.successor_nodes()))
    .then_with(|| left.flags.cmp(&right.flags))
    .then_with(|| left.entries().cmp(right.entries()))
    .then_with(|| date_order(left, right))
    .then_with(|| {
      let right_parents = right.parent_nodes().into_iter().flatten();
      left.parent_nodes().into_iter().flatten().cmp(right_parents)
    })
}

/// Two markers' dates in 7.2's order: seconds, then timezone; a marker that keeps none (format 0) first.
fn date_order(left: &Marker<'_>, right: &Marker<'_>) -> Ordering {
  match (left.date, right.date) {
    (Some(left), Some(right)) => left
      .seconds()
      .partial_cmp(&right.seconds())
      .unwrap_or(Ordering::Equal)
      .then_with(|| left.offset_minutes.cmp(&right.offset_minutes)),
    (left, right) => left.is_some().cmp(&right.is_some()),
  }
}

/// The markers by the nodes that make them relevant: a successor; a prune's predecessor; a prune's parent.
struct MarkerIndex<'m> {
  /// Markers naming the node as a successor.
  by_successor: BTreeMap<&'m [u8], Vec<usize>>,
  /// Prunes of the node.
  prunes_of: BTreeMap<&'m [u8], Vec<usize>>,
  /// Prunes naming the node as a parent of their predecessor.
  prunes_under: BTreeMap<&'m [u8], Vec<usize>>,
}

impl<'m> MarkerIndex<'m> {
  /// Indexes `markers`.
  fn new(markers: &[Marker<'m>]) -> Self {
    let mut index = Self {
      by_successor: BTreeMap::new(),
      prunes_of: BTreeMap::new(),
      prunes_under: BTreeMap::new(),
    };
    for (at, marker) in markers.iter().enumerate() {
      for successor in marker.successor_nodes() {
        index.by_successor.entry(successor).or_default().push(at);
      }
      if marker.successors.is_empty() {
        index
          .prunes_of
          .entry(marker.predecessor)
          .or_default()
          .push(at);
        for parent in marker.parent_nodes().into_iter().flatten() {
          index.prunes_under.entry(parent).or_default().push(at);
        }
      }
    }
    index
  }

  /// The markers `node` makes directly relevant (the iterator holds both the index and `node`, as edition 2024's
  /// return-position `impl Trait` captures every lifetime in scope).
  fn direct(&self, node: &[u8]) -> impl Iterator<Item = usize> {
    [&self.by_successor, &self.prunes_of, &self.prunes_under]
      .into_iter()
      .flat_map(move |map| map.get(node).into_iter().flatten().copied())
  }
}

#[cfg(test)]
#[path = "getbundle_tests.rs"]
pub(crate) mod tests;
