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
//! parts after it. A clone of every head (the client holding nothing, over a view that serves every changeset: the large
//! case) is walked by its links, 7.2's own fast path (`fastpathlinkrev`): the missing changesets as ranges, the manifest
//! log in its order and then the file logs from row to row, each revision whose log links it to a missing changeset, so
//! nothing grows with the repository but the walk's place, and every row it visits is sent. Any other fetch (a pull,
//! whose client holds the history before it; a pull of some heads; a view that hides changesets) takes the slow path
//! below, bounded by what it sends. 7.2 walks a pull of every head by its links too, visiting every row to send a few:
//! a one-changeset pull of Mercurial's history took 4.2 s here that way (goal 93 perf), so slates takes the slow path,
//! which may resend a manifest the client holds (an empty changeset's, reused), which its `addgroup` drops.
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
use crate::changegroup::{self, Revision, Version};
use crate::marker_tuple::Tuple;
use crate::obsmarkers::{self, Format};
use crate::out::{Measure, Out};
use crate::read::to_u64;
use crate::{
  Admission, NULL_NODE, Node, PastAdmission, bookmarks, changeset, delta, manifest, phases,
  pushkey, quote,
};
use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};

/// The served view with what its changesets name: their texts, and the manifest and file logs' revisions (A-619 part
/// 2). A text is lent (A-680): in place where the store holds it in one piece, else copied, the copy's bytes charged to
/// the request's room before it is made, so nothing a store copies is held past the request's admission.
pub trait Store: Repository {
  /// Derived: the most rows one step of a fetch asks of the changeset store at once, one revision's at least: a content
  /// window's worth of their stored form for the indexed view ([`crate::render::rows_a_step`]), the store's own unit of a
  /// step, so no step decides or holds more however long the history (goal 93: a clone's file section asked the whole
  /// history's rows in one batch and decided them in one 2 s step, CI 45117277400).
  fn rows_a_step(&self) -> u64;

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

  /// The file revision at `path` whose node is `node` as its log's row holds it, with no text read ([`FileEntry`];
  /// A-767 C1: a fetch sends each file revision as the changeset store keeps it, so the served view reads no file's
  /// whole text); none where the log lacks it.
  fn file_entry(&self, path: &[u8], node: &Node) -> Option<FileEntry>;

  /// The manifest revision whose node is `node` as its log's row holds it, with no text read
  /// (goal 93 C1 (1): manifest texts let go to the changeset store); `None` by default.
  fn manifest_entry(&self, _node: &Node) -> Option<FileEntry> {
    None
  }

  /// What a whole answer, which cannot wait between its steps, reads of the changeset store where its walk asks it
  /// ([`crate::render::Want`]), at once; none where the store answers only between steps, as the daemon's does (its
  /// changeset store is its owner's thread's): its edges stream such answers and ask it between turns. None by default.
  fn stored_now(&self, _want: &crate::render::Want) -> Option<Given> {
    None
  }

  /// The manifest log's revision at `position` (A-745: a fetch walks the log in its order): its node, and the served
  /// revision of the changeset the log links it to; `None` past the log's end.
  fn manifest_linked(&self, position: u64) -> Option<Linked>;

  /// The first file revision at or after `from` (a path and a position in its log) in the file logs' order, by path
  /// and then position (A-745): its path, its position, its node and its link; `None` past the last file log.
  fn file_linked_from(&self, from: (&[u8], u64)) -> Option<(Vec<u8>, u64, Linked)>;

  /// The manifest log's revision at `position` as the store keeps it where that is a delta (A-729): its delta's base's
  /// position, its parents' nodes, its flags and the delta, in Mercurial's form; `None` for a revision kept whole, or
  /// by a store that keeps none. A clone sends it as it is where its base went before it in the same answer, so the
  /// server rebuilds no text and the client diffs none (Ada's no-allocation rule; goal 93's clone). Lent as a text is,
  /// a copy charged to `room`.
  fn manifest_delta(
    &self,
    _position: u64,
    _room: &mut Admission,
  ) -> Result<Option<StoredDelta<'_>>, PastAdmission> {
    Ok(None)
  }

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

  /// The index and content store a stream clone's revlogs are rendered from as they are sent
  /// ([`crate::render::Streaming`], A-745); `None` where none is served ([`Store::streams`]).
  fn revlogs(&self) -> Option<(&crate::index::HgIndex, &slates_vcs::ContentStore)> {
    None
  }
}

/// A file revision as its log's row holds it ([`Store::file_entry`]): its position in its log, its parents' nodes, the
/// node of the changeset its log links it to, and its flags.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileEntry {
  /// Its position in its log.
  pub position: u64,
  /// Its parents' nodes.
  pub parents: [Node; 2],
  /// The node of the changeset its log links it to.
  pub link: Node,
  /// Its flags.
  pub flags: u16,
}

/// What the changeset store gives back for a walk's want ([`crate::render::Want`]): a log's rows in turn, or the
/// chunks at the places asked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Given {
  /// Each log's rows, in the order asked, none for a revision the store keeps no row of.
  Rows(Vec<Vec<Option<crate::render::StoredRevision>>>),
  /// The bytes at each place asked, in order.
  Chunks(Vec<Vec<u8>>),
  /// Each full text asked, in order.
  Texts(Vec<Vec<u8>>),
  /// The full texts asked hold more than the want's `most` bytes: none read past it.
  TextsPast,
}

/// A manifest revision as the store keeps it, a delta ([`Store::manifest_delta`]): its base's position in the log, its
/// parents' nodes, its flags and the delta's bytes.
pub type StoredDelta<'s> = (u64, [Node; 2], u16, Cow<'s, [u8]>);

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
  /// Whether to send a `STREAM2` streaming-clone part (`stream=1`, Mercurial 7.2 `getbundle`).
  pub stream: Option<&'a [u8]>,
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
  while answering.step(store, admission, out)? {
    if answering.wants().is_some() {
      return Err(unanswered());
    }
  }
  Ok(())
}

/// A `getbundle`'s answer made a slice at a time (A-745; §4.17.1 "A fetch's answer"): what [`getbundle`] writes whole,
/// cut into steps its owner advances, each lent the served view anew and charged to the room it is given. A step writes
/// the stream's head and the changegroup part's header, or one unit of the changegroup (a revision, a group's end) as
/// one payload chunk, or the parts after it and the stream's end. What the answer holds across steps is the parts after
/// the changegroup (made when it begins: bookmarks, `listkeys`, markers, phase heads), the missing changesets as ranges
/// ([`pull::Ranges`]) and its place in the changegroup ([`Walk`]); never a revision's text past its step.
/// A `STREAM2` bundle2 part being streamed inside [`Answering`]: counts files and bytes across
/// turns via [`crate::render::Streaming`], writes the `HG20` stream head and `STREAM2` part header
/// once counts are known, then emits payload chunks until done.
#[derive(Debug)]
struct Stream2State {
  /// The revlog + extra store files streamer in `WireFormat::V2`.
  streaming: Box<crate::render::Streaming>,
  /// Whether the `HG20` stream head and `STREAM2` part header have been emitted.
  head_written: bool,
  /// Reusable chunk staging buffer so `step_stream2` batches up to one content window per bundle2
  /// payload chunk instead of allocating a `Vec` and 4-byte chunk header per revlog revision.
  scratch: Vec<u8>,
}

/// A `getbundle`'s answer made a slice at a time (A-745; §4.17.1 "A fetch's answer"): what [`getbundle`] writes whole,
/// cut into steps its owner advances, each lent the served view anew and charged to the room it is given. A step writes
/// the stream's head and the changegroup or `STREAM2` part's header, or one unit of the stream as one payload chunk, or
/// the parts after it and the stream's end.
#[derive(Debug)]
pub struct Answering {
  /// The stream's head and the changegroup part's header, until the first step writes them.
  head: Option<Vec<u8>>,
  /// A `STREAM2` part streamed first when `stream=1` was asked.
  stream2: Option<Stream2State>,
  /// The changegroup, while one is sent.
  walk: Option<Walk>,
  /// The parts after the changegroup and the stream's end, until the last step writes them.
  tail: Option<Vec<u8>>,
}

impl Answering {
  /// Whether this answer is streaming a `STREAM2` bundle2 clone (`stream=1`).
  pub fn is_stream2(&self) -> bool {
    self.stream2.is_some()
  }

  /// What the answer's next step asks of the changeset store, if anything (a `STREAM2` part or
  /// a changegroup's file section, [`Walk`]).
  pub fn wants(&self) -> Option<&crate::render::Want> {
    if let Some(stream2) = &self.stream2 {
      return stream2.streaming.wants();
    }
    self.walk.as_ref()?.wants()
  }

  /// The changeset store's answer to [`Answering::wants`].
  pub fn give(&mut self, given: Given) {
    if let Some(stream2) = &mut self.stream2 {
      match given {
        Given::Rows(rows) => stream2.streaming.give_rows(rows),
        Given::Chunks(chunks) => stream2.streaming.give_chunks(chunks),
        Given::Texts(_) | Given::TextsPast => {}
      }
      return;
    }
    if let Some(walk) = &mut self.walk {
      walk.give(given);
    }
  }

  /// Begins the answer to `asked` over `store` (the module docs' parts, in 7.2's order), what it holds charged to
  /// `admission`; refused before anything is written, as 7.2 refuses before its first part.
  pub fn begin(
    store: &impl Store,
    asked: &Asked<'_>,
    admission: &mut Admission,
  ) -> Result<Answering, Refusal> {
    let mut unquoted = Vec::new();
    let caps = bundle2_capabilities(asked.bundlecaps, admission, &mut unquoted)?;
    let stream2 = stream2_state(store, asked.stream, &caps, admission)?;
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
    let cg_id = u32::from(stream2.is_some());
    let mut head = Vec::new();
    if stream2.is_none() {
      crate::bundle2::put_stream_head(&mut head);
    }
    let first = match &changegroup {
      Some(built) => {
        let header = built_part(built, cg_id);
        crate::bundle2::put_part_header(&header, &mut head).map_err(|_| unencodable("a part"))?;
        cg_id.saturating_add(1)
      }
      None => cg_id,
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
      head: (!head.is_empty()).then_some(head),
      stream2,
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
    if self.stream2.is_some() {
      return self.step_stream2(store, room, out);
    }
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

  /// Advances a `STREAM2` part one step: counts files and bytes, writes the `HG20` head and
  /// `STREAM2` part header once counted, then streams payload chunks until the part closes.
  fn step_stream2(
    &mut self,
    store: &impl Store,
    room: &mut Admission,
    out: &mut Vec<u8>,
  ) -> Result<bool, Refusal> {
    let Some(stream2) = &mut self.stream2 else {
      return Ok(true);
    };
    let (index, content) = store.revlogs().ok_or(Refusal::Store { what: "a revlog" })?;
    if !stream2.head_written {
      if !stream2.streaming.at_head() {
        stream2
          .streaming
          .step(index, content, room, out)
          .map_err(super::stream::revlog_refused)?;
        return Ok(true);
      }
      if stream2.streaming.is_not_served() {
        return Err(Refusal::Store { what: "a revlog" });
      }
      let (filecount, bytecount) = stream2.streaming.counts();
      write_stream2_head(filecount, bytecount, room, out)?;
      stream2.head_written = true;
      let mut scratch = Vec::new();
      let more = stream2
        .streaming
        .step(index, content, room, &mut scratch)
        .map_err(super::stream::revlog_refused)?;
      if !more {
        crate::bundle2::put_payload_chunk(out, &[]).map_err(|_| unencodable("a chunk"))?;
        self.stream2 = None;
      }
      return Ok(true);
    }
    stream2.scratch.clear();
    let target = content.window().max(1);
    let mut more = true;
    while stream2.scratch.len() < target {
      match stream2
        .streaming
        .step(index, content, room, &mut stream2.scratch)
      {
        Ok(next) => {
          more = next;
          if !more || stream2.streaming.wants().is_some() {
            break;
          }
        }
        Err(crate::render::Refused::Past(_)) if !stream2.scratch.is_empty() => {
          break;
        }
        Err(refused) => return Err(super::stream::revlog_refused(refused)),
      }
    }
    if !stream2.scratch.is_empty() {
      crate::bundle2::put_payload_chunk(out, &stream2.scratch)
        .map_err(|_| unencodable("a chunk"))?;
    }
    if !more {
      crate::bundle2::put_payload_chunk(out, &[]).map_err(|_| unencodable("a chunk"))?;
      self.stream2 = None;
    }
    Ok(true)
  }
}

/// Validates and starts `STREAM2` streaming state when `stream=1` was asked.
fn stream2_state(
  store: &impl Store,
  stream: Option<&[u8]>,
  caps: &Bundle2Capabilities<'_>,
  admission: &mut Admission,
) -> Result<Option<Stream2State>, Refusal> {
  if !flag(stream, false) {
    return Ok(None);
  }
  if !store.streams() || store.revlogs().is_none() || !offers(caps, b"stream", Some(b"v2")) {
    return Err(Refusal::NoCommonExchange { what: "stream" });
  }
  let mut extra = Vec::new();
  if !store.publishing() {
    let mut roots: Vec<Node> = store.draft_roots().collect();
    roots.sort_unstable();
    roots.dedup();
    if !roots.is_empty() {
      // Format: `<phase> <40-hex-node>\n` per draft root (`phases.py` `phaseroots`).
      let line_bytes = 2_u64
        .saturating_add(to_u64(crate::NODE_LEN).saturating_mul(2))
        .saturating_add(1);
      admission.charge(to_u64(roots.len()).saturating_mul(line_bytes))?;
      let mut bytes = Vec::with_capacity(
        roots
          .len()
          .saturating_mul(usize::try_from(line_bytes).unwrap_or(0)),
      );
      for node in roots {
        bytes.extend_from_slice(b"1 ");
        pushkey::encode_hex(&node, &mut bytes);
        bytes.push(b'\n');
      }
      extra.push((b"phaseroots".to_vec(), bytes));
    }
  }
  Ok(Some(Stream2State {
    streaming: Box::new(crate::render::Streaming::v2(extra)),
    head_written: false,
    scratch: Vec::new(),
  }))
}

/// Writes the `HG20` bundle2 stream head and mandatory `STREAM2` part header (`part_id = 0`) with
/// Mercurial 7.2's `bytecount`, `filecount`, and URL-quoted `requirements` parameters.
fn write_stream2_head(
  filecount: u64,
  bytecount: u64,
  room: &mut Admission,
  out: &mut Vec<u8>,
) -> Result<(), Refusal> {
  let bytecount = crate::decimal::Decimal::unsigned(bytecount);
  let filecount = crate::decimal::Decimal::unsigned(filecount);
  let params = vec![
    Param {
      key: b"bytecount",
      value: bytecount.as_bytes(),
    },
    Param {
      key: b"filecount",
      value: filecount.as_bytes(),
    },
    Param {
      key: b"requirements",
      value: crate::render::STREAM2_REQUIREMENTS,
    },
  ];
  let mandatory_params = params.len();
  let part = Part {
    kind: b"STREAM2",
    id: 0,
    params,
    mandatory_params,
    depth: 0,
  };
  let mut head = Vec::new();
  crate::bundle2::put_stream_head(&mut head);
  crate::bundle2::put_part_header(&part, &mut head).map_err(|_| unencodable("a part"))?;
  room.charge(to_u64(head.len()))?;
  out.extend_from_slice(&head);
  Ok(())
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

/// The `OBSMARKERS` part: the markers relevant to the heads' ancestors in the newest format both read, each written
/// from 7.2's tuple of it, whatever format it was received in (`encodemarkers`); none where there are none.
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
  let mut len = 1_u64;
  for marker in &markers {
    len = len.saturating_add(
      marker
        .marker_len(format)
        .map_err(|_| unencodable("a marker"))?,
    );
  }
  admission.charge(len)?;
  let mut payload = Vec::with_capacity(usize::try_from(len).unwrap_or_default());
  payload.push(obsmarkers::header(format));
  for marker in &markers {
    marker
      .encode_marker(format, &mut payload)
      .map_err(|_| unencodable("a marker"))?;
  }
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

/// Whether a fetch of `outgoing` is walked by its links ([`Walk::begin`]): a clone of every head, the client holding
/// nothing (no common revision) and the view serving every changeset, so every changeset is missing and every manifest
/// and file revision is sent: the walk visits what it sends. Any fetch whose client holds something is made from the
/// changesets it lacks ([`Named`]), so a pull of one changeset costs what it sends, never a walk of every
/// row of the history for its links (goal 93 perf: a one-changeset pull of Mercurial's history took 4.2 s by the
/// walk). Decided from the fetch's own counts, nothing walked.
fn by_links(store: &impl Store, outgoing: &Outgoing) -> bool {
  outgoing.common.is_empty()
    && store.serves_every_changeset()
    && outgoing.missing.len() == u64::from(store.count())
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
  while walk.step(store, admission, &mut out)? {
    if walk.wants().is_some() {
      return Err(unanswered());
    }
  }
  Ok(out)
}

/// The changegroup of `outgoing` in `version` made by the changesets that name each revision ([`Named`]),
/// whatever the fetch, its payload whole: the two walks' oracle.
#[cfg(test)]
pub(crate) fn named_changegroup_whole(
  store: &impl Store,
  outgoing: &Outgoing,
  version: Version,
  admission: &mut Admission,
) -> Result<Vec<u8>, Refusal> {
  let mut walk = Walk::begun(store, outgoing, version, admission, false)?;
  let mut out = Vec::new();
  while walk.step(store, admission, &mut out)? {
    if walk.wants().is_some() {
      return Err(unanswered());
    }
  }
  Ok(out)
}

/// The refusal of a whole answer whose walk asks the changeset store between steps, which only an edge answering a step
/// at a time can ask ([`Store::stored_now`]).
fn unanswered() -> Refusal {
  Refusal::Store {
    what: "a file revision read from the changeset store between steps",
  }
}

/// Where a changegroup's walk stands (A-745).
#[derive(Clone, Debug, PartialEq, Eq)]
enum At {
  /// The changelog's group, from this served revision.
  Changelog(Rev),
  /// The manifest log, from this position; the position and node of the manifest this answer sent last, which a
  /// manifest kept as a delta against it is sent as.
  Manifests(u64, Option<(u64, Node)>),
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
#[derive(Debug)]
pub struct Walk {
  /// The changesets the client lacks.
  missing: pull::Ranges,
  /// The version written.
  version: Version,
  /// Where the walk stands; none once the changegroup's end is written.
  at: Option<At>,
  /// Whether changeset texts may still be resident in memory (`false` after the first away miss, avoiding redundant
  /// `HgIndex::revision_text` walks).
  changeset_resident: bool,
  /// Whether manifest texts may still be resident in memory (`false` after the first away miss).
  manifest_resident: bool,
  /// A fetch the links cannot walk, walked by the changesets that name each revision ([`Named`]), until its file
  /// section ([`Walk::pull`]).
  named: Option<Named>,
  /// The file revisions read from the changeset store ahead of their steps ([`Ahead`]).
  files: Files,
  /// A pull's file section, once its changelog and manifests are written ([`Pull`]).
  pull: Option<Pull>,
}

/// The revisions whose rows are asked: each log's path, and its revisions' positions and nodes, in the walk's order.
type AskedRows = Vec<(Vec<u8>, Vec<(u64, Node)>)>;

/// The file section's reads of the changeset store (A-767 C1 step 1; main's ruling of 2026-11-21 08:45): each file
/// revision sent as the store keeps it, its stored delta against the revision its base text was sent as earlier in
/// this answer (an earlier revision of the same log), or its stored whole against the null base; no whole text read or
/// made. The walk asks the store by [`Walk::wants`] before the step that needs it, a batch at a time: the rows of as
/// many revisions ahead as the room the step was given holds, then the chunks of as many of those as it holds, given
/// back by [`Walk::give`]. A revision the store keeps no row of, or whose stored base this answer did not send, is
/// refused typed.
#[derive(Debug, Default)]
struct Files {
  /// What the next step asks of the store.
  want: Option<crate::render::Want>,
  /// The revisions whose rows are asked, each log's path and its revisions' positions and nodes, in the walk's order.
  asked: AskedRows,
  /// The revisions read ahead, in the walk's order.
  ahead: std::collections::VecDeque<Ahead>,
  /// How many of `ahead`'s front revisions have their chunks asked.
  chunks_asked: usize,
  /// A revision the store keeps no row of: refused at the next step.
  unkept: bool,
  /// The open group's texts sent, by identity: the node each was sent as, which a later revision's stored delta names.
  sent: BTreeMap<[u8; 32], Node>,
  /// The revisions sent as the store keeps them.
  stored: u64,
  /// The room the step that asked was given: what sizes the chunks asked after the rows.
  room: u64,
}

impl Files {
  /// Asks the store for the chunks of the revisions read ahead that lack theirs, as many as `room`'s bytes hold (one
  /// at least).
  fn ask_chunks(&mut self, room: u64) {
    let mut places = Vec::new();
    let mut bytes = 0_u64;
    for ahead in self.ahead.iter().filter(|ahead| ahead.chunk.is_none()) {
      if !places.is_empty() && bytes.saturating_add(ahead.row.place.len) > room {
        break;
      }
      bytes = bytes.saturating_add(ahead.row.place.len);
      places.push(ahead.row.place);
    }
    self.chunks_asked = self
      .ahead
      .iter()
      .take_while(|ahead| ahead.chunk.is_some())
      .count()
      .saturating_add(places.len());
    if !places.is_empty() {
      self.want = Some(crate::render::Want::Chunks { places });
    }
  }
}

/// A file revision read ahead of its step: where it is, its stored row, and its chunk once given.
#[derive(Debug)]
struct Ahead {
  /// Its log's path.
  path: Vec<u8>,
  /// Its position in its log.
  position: u64,
  /// Its node.
  node: Node,
  /// How the store keeps it.
  row: crate::render::StoredRevision,
  /// Its stored edit or whole, once given.
  chunk: Option<Vec<u8>>,
}

impl Walk {
  /// A walk of the changegroup of `outgoing` in `version`, at its start: by the links for a clone of every head
  /// ([`by_links`]: the client holds nothing, so every row is sent), else by the changesets that name each revision,
  /// made whole within `admission`.
  pub fn begin(
    store: &impl Store,
    outgoing: &Outgoing,
    version: Version,
    admission: &mut Admission,
  ) -> Result<Walk, Refusal> {
    Walk::begun(
      store,
      outgoing,
      version,
      admission,
      by_links(store, outgoing),
    )
  }

  /// A walk of the changegroup of `outgoing` in `version` at its start, by the links if `linked`, else by the
  /// changesets that name each revision ([`Named`]), a unit a step, within
  /// `admission`.
  fn begun(
    store: &impl Store,
    outgoing: &Outgoing,
    version: Version,
    admission: &mut Admission,
    linked: bool,
  ) -> Result<Walk, Refusal> {
    let mut walk = Walk {
      missing: outgoing.missing.clone(),
      version,
      at: Some(At::Changelog(0)),
      changeset_resident: true,
      manifest_resident: true,
      named: None,
      files: Files::default(),
      pull: None,
    };
    if !linked {
      walk.named = Some(Named {
        held: common_ancestors(store, &outgoing.common, admission)?,
        stage: NamedStage::Changelog(0),
        manifests: Vec::new(),
        seen_manifests: BTreeSet::new(),
        files: BTreeMap::new(),
        seen_files: BTreeSet::new(),
        last_sent: None,
        changeset_resident: true,
        manifest_resident: true,
        store: NamedStore::default(),
      });
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
    if let Some(mut named) = self.named.take() {
      loop {
        while let Some(want) = named.wants().cloned() {
          let Some(given) = store.stored_now(&want) else {
            self.named = Some(named);
            return Ok(true);
          };
          named.give(given);
        }
        let done = named.step(store, (&self.missing, self.version), room, out)?;
        if done {
          self.pull = Some(named.pull());
          return Ok(true);
        }
        if named.wants().is_none() {
          self.named = Some(named);
          return Ok(true);
        }
      }
    }
    if let Some(pull) = &mut self.pull {
      if pull.ended {
        return Ok(false);
      }
      loop {
        while let Some(want) = pull.want.clone() {
          let Some(given) = store.stored_now(&want) else {
            return Ok(true);
          };
          pull.give(given);
        }
        let more = pull.step(store, self.version, room, out)?;
        if pull.want.is_none() {
          return Ok(more);
        }
      }
    }
    loop {
      // A want the store answers at once (a whole answer's, [`Store::stored_now`]) is given here and the unit taken
      // again; one it answers only between steps (the daemon's) stays, and the step takes nothing: its edge asks the
      // store before the next.
      while let Some(want) = self.files.want.clone() {
        let Some(given) = store.stored_now(&want) else {
          return Ok(true);
        };
        self.give(given);
      }
      // A refused step leaves the walk where it was, so a step given more room takes the same unit again.
      let Some(at) = self.at.clone() else {
        return Ok(false);
      };
      self.at = match at {
        At::Changelog(from) => self.changeset(store, from, room, out)?,
        At::Manifests(position, sent) => self.manifest(store, (position, sent), room, out)?,
        At::Files {
          path,
          position,
          open,
        } => self.file(store, (path, position), open, room, out)?,
      };
      if self.files.want.is_none() {
        return Ok(self.at.is_some());
      }
    }
  }

  /// What the next step asks of the changeset store, if anything ([`Named`], [`Files`], [`Pull`]): given back by
  /// [`Walk::give`] before it.
  pub fn wants(&self) -> Option<&crate::render::Want> {
    if let Some(named) = &self.named {
      return named.wants();
    }
    match &self.pull {
      Some(pull) => pull.want.as_ref(),
      None => self.files.want.as_ref(),
    }
  }

  /// The changeset store's answer to [`Walk::wants`]: rows ask the chunks of as many of their revisions as the room the
  /// asking step was given holds, at once.
  pub fn give(&mut self, given: Given) {
    if let Some(named) = &mut self.named {
      named.give(given);
      return;
    }
    if let Some(pull) = &mut self.pull {
      pull.give(given);
      return;
    }
    let files = &mut self.files;
    let room = files.room;
    files.want = None;
    match given {
      Given::Rows(rows) => {
        let asked = core::mem::take(&mut files.asked);
        if rows.len() != asked.len() {
          files.unkept = true;
        }
        for ((path, revisions), rows) in asked.into_iter().zip(rows) {
          if rows.len() != revisions.len() {
            files.unkept = true;
          }
          for ((position, node), row) in revisions.into_iter().zip(rows) {
            match row {
              Some(row) => files.ahead.push_back(Ahead {
                path: path.clone(),
                position,
                node,
                row,
                chunk: None,
              }),
              None => files.unkept = true,
            }
          }
        }
        if !files.unkept {
          files.ask_chunks(room);
        }
      }
      Given::Chunks(chunks) => {
        let asked = core::mem::take(&mut files.chunks_asked);
        for (ahead, chunk) in files.ahead.iter_mut().take(asked).zip(chunks) {
          ahead.chunk = Some(chunk);
        }
      }
      // A clone walked by its links sends every base before its delta: it asks no whole text.
      Given::Texts(_) | Given::TextsPast => files.unkept = true,
    }
  }

  /// The file revisions a pull sent whole against the null base, its stored base not on the client nor sent before it
  /// (main's ruling of 2026-11-21 08:45, counted).
  pub fn whole_sent(&self) -> u64 {
    self.pull.as_ref().map_or(0, |pull| pull.whole)
  }

  /// The file revisions this walk sent as the changeset store keeps them.
  pub fn stored_sent(&self) -> u64 {
    self
      .files
      .stored
      .saturating_add(self.pull.as_ref().map_or(0, |pull| pull.stored))
  }

  /// The missing changeset at or after `from`, or the changelog group's end. If the changeset's
  /// text was let go to the changeset store (goal 93 C1 (1)), reads batched rows and chunks from
  /// `CHANGELOG_PATH` through [`Files`] and emits stored deltas verbatim.
  fn changeset(
    &mut self,
    store: &impl Store,
    from: Rev,
    room: &mut Admission,
    out: &mut Vec<u8>,
  ) -> Result<Option<At>, Refusal> {
    let Some(rev) = self.missing.next_from(from) else {
      self.files.ahead.clear();
      self.files.chunks_asked = 0;
      self.files.sent.clear();
      changegroup::put_end(out);
      return Ok(Some(At::Manifests(0, None)));
    };
    if self.files.unkept {
      return Err(no_changeset_text());
    }
    let node = node_of(store, rev)?;
    let [first, second] = parents_of(store, rev)?;
    let parent =
      |parent: Option<Rev>| parent.map_or(Ok(NULL_NODE), |parent| node_of(store, parent));
    let parents = [parent(first)?, parent(second)?];
    let ready = matches!(
      self.files.ahead.front(),
      Some(ahead)
        if ahead.path == crate::push::intake::CHANGELOG_PATH
          && ahead.position == u64::from(rev)
    );
    if self.changeset_resident && !ready && self.files.ahead.is_empty() {
      if let Some(text) = store.changeset_text(rev, room)? {
        let (manifest, _) = read_changeset(&text)?;
        if manifest != NULL_NODE && !store.holds_manifest(&manifest) {
          return Err(no_manifest());
        }
        self.put((node, parents, node, 0), &text, room, out)?;
        return Ok(Some(At::Changelog(rev.saturating_add(1))));
      }
      self.changeset_resident = false;
    }
    if !ready {
      self.files.ahead.clear();
      self.files.chunks_asked = 0;
      self.ask_changelog_rows(store, rev, room.left());
      return Ok(Some(At::Changelog(rev)));
    }
    if self
      .files
      .ahead
      .front()
      .is_some_and(|ahead| ahead.chunk.is_none())
    {
      self.files.ask_chunks(room.left());
      return Ok(Some(At::Changelog(rev)));
    }
    let Some(Ahead {
      node: stored_node,
      row,
      chunk,
      ..
    }) = self.files.ahead.pop_front()
    else {
      return Ok(Some(At::Changelog(rev)));
    };
    let chunk = chunk.unwrap_or_default();
    if let Some(manifest) = store.manifest_node(rev)
      && manifest != NULL_NODE
      && !store.holds_manifest(&manifest)
    {
      return Err(no_manifest());
    }
    match row.base {
      None => self.put((stored_node, parents, stored_node, 0), &chunk, room, out)?,
      Some(base) => {
        let base = *self.files.sent.get(&base).ok_or_else(no_changeset_text)?;
        self.put_delta(
          (stored_node, parents, stored_node, 0),
          (base, &chunk),
          room,
          out,
        )?;
      }
    }
    self.files.sent.insert(row.text, stored_node);
    Ok(Some(At::Changelog(rev.saturating_add(1))))
  }

  /// The manifest at `position` where a missing changeset links it, or the manifest group's end:
  /// sent as the stored delta from the changeset store when let go (goal 93 C1 (1)), else from the
  /// resident index (`manifest_delta` / `manifest`).
  fn manifest(
    &mut self,
    store: &impl Store,
    (position, sent): (u64, Option<(u64, Node)>),
    room: &mut Admission,
    out: &mut Vec<u8>,
  ) -> Result<Option<At>, Refusal> {
    let Some(linked) = store.manifest_linked(position) else {
      self.files.ahead.clear();
      self.files.chunks_asked = 0;
      self.files.sent.clear();
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
    let following = position.saturating_add(1);
    let Some(link) = linked.link.filter(|link| self.missing.contains(*link)) else {
      return Ok(Some(At::Manifests(following, sent)));
    };
    if self.files.unkept {
      return Err(no_manifest());
    }
    let link = node_of(store, link)?;
    let ready = matches!(
      self.files.ahead.front(),
      Some(ahead)
        if ahead.path == crate::push::intake::MANIFEST_PATH && ahead.position == position
    );
    if self.manifest_resident && !ready && self.files.ahead.is_empty() {
      let stored = match sent {
        Some((sent_position, sent_node)) => store
          .manifest_delta(position, room)?
          .filter(|(base, ..)| *base == sent_position)
          .map(|(_, parents, flags, delta)| (sent_node, parents, flags, delta)),
        None => None,
      };
      if let Some((base, parents, flags, delta)) = stored {
        self.put_delta(
          (linked.node, parents, link, flags),
          (base, &delta),
          room,
          out,
        )?;
        return Ok(Some(At::Manifests(
          following,
          Some((position, linked.node)),
        )));
      }
      if let Some(logged) = store.manifest(&linked.node, room)? {
        self.put(
          (linked.node, logged.parents, link, logged.flags),
          &logged.text,
          room,
          out,
        )?;
        return Ok(Some(At::Manifests(
          following,
          Some((position, linked.node)),
        )));
      }
      self.manifest_resident = false;
    }
    if !ready {
      self.files.ahead.clear();
      self.files.chunks_asked = 0;
      self.ask_manifest_rows(store, position, room.left());
      return Ok(Some(At::Manifests(position, sent)));
    }
    if self
      .files
      .ahead
      .front()
      .is_some_and(|ahead| ahead.chunk.is_none())
    {
      self.files.ask_chunks(room.left());
      return Ok(Some(At::Manifests(position, sent)));
    }
    let Some(Ahead {
      node, row, chunk, ..
    }) = self.files.ahead.pop_front()
    else {
      return Ok(Some(At::Manifests(position, sent)));
    };
    let chunk = chunk.unwrap_or_default();
    let FileEntry { parents, flags, .. } = store.manifest_entry(&node).ok_or_else(no_manifest)?;
    match row.base {
      None => self.put((node, parents, link, flags), &chunk, room, out)?,
      Some(base) => {
        let base = *self.files.sent.get(&base).ok_or_else(no_manifest)?;
        self.put_delta((node, parents, link, flags), (base, &chunk), room, out)?;
      }
    }
    self.files.sent.insert(row.text, node);
    Ok(Some(At::Manifests(following, Some((position, node)))))
  }

  /// Batches up to `rows_a_step` missing changesets from `from` on into `CHANGELOG_PATH` row asks.
  fn ask_changelog_rows(&mut self, store: &impl Store, from: Rev, room: u64) {
    let most = usize::try_from(
      room
        .checked_div(to_u64(core::mem::size_of::<Ahead>()).max(1))
        .unwrap_or(room),
    )
    .unwrap_or(usize::MAX)
    .max(1);
    let rows = store.rows_a_step().max(1);
    let mut revisions = Vec::new();
    let mut at = from;
    while revisions.len() < most && to_u64(revisions.len()) < rows {
      let Some(rev) = self.missing.next_from(at) else {
        break;
      };
      let Some(node) = store.node(rev) else {
        break;
      };
      revisions.push((u64::from(rev), node));
      at = rev.saturating_add(1);
    }
    let nodes = revisions.iter().map(|(_, node)| *node).collect();
    let path = crate::push::intake::CHANGELOG_PATH.to_vec();
    self.files.asked = vec![(path.clone(), revisions)];
    self.files.room = room;
    self.files.want = Some(crate::render::Want::Rows {
      logs: vec![(path, nodes)],
    });
  }

  /// Batches up to `rows_a_step` missing manifests from `position` on into `MANIFEST_PATH` row asks.
  fn ask_manifest_rows(&mut self, store: &impl Store, position: u64, room: u64) {
    let most = usize::try_from(
      room
        .checked_div(to_u64(core::mem::size_of::<Ahead>()).max(1))
        .unwrap_or(room),
    )
    .unwrap_or(usize::MAX)
    .max(1);
    let rows = store.rows_a_step().max(1);
    let mut revisions = Vec::new();
    let mut scanned = 0_u64;
    let mut at = position;
    while revisions.len() < most && scanned < rows {
      let Some(linked) = store.manifest_linked(at) else {
        break;
      };
      scanned = scanned.saturating_add(1);
      if linked.link.is_some_and(|link| self.missing.contains(link)) {
        revisions.push((at, linked.node));
      }
      at = at.saturating_add(1);
    }
    let nodes = revisions.iter().map(|(_, node)| *node).collect();
    let path = crate::push::intake::MANIFEST_PATH.to_vec();
    self.files.asked = vec![(path.clone(), revisions)];
    self.files.room = room;
    self.files.want = Some(crate::render::Want::Rows {
      logs: vec![(path, nodes)],
    });
  }

  /// The file revision at or after `from` where a missing changeset links it (its group's name first where it opens
  /// one, the open group's end first where it is another path's), or the file list's end: sent as the changeset store
  /// keeps it ([`Files`]), read ahead of this step; where it is not read yet, this step asks the store and takes the
  /// same place again.
  fn file(
    &mut self,
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
    if self.files.unkept {
      return Err(Refusal::Store {
        what: "a file revision the changeset store keeps no row of",
      });
    }
    let here = At::Files {
      path: found.clone(),
      position,
      open: open.clone(),
    };
    let ready = matches!(
      self.files.ahead.front(),
      Some(ahead) if ahead.path == found && ahead.position == position
    );
    if !ready {
      self.files.ahead.clear();
      self.files.chunks_asked = 0;
      self.ask_rows(store, (&found, position), room.left());
      return Ok(Some(here));
    }
    if self
      .files
      .ahead
      .front()
      .is_some_and(|ahead| ahead.chunk.is_none())
    {
      self.files.ask_chunks(room.left());
      return Ok(Some(here));
    }
    let Some(Ahead {
      node, row, chunk, ..
    }) = self.files.ahead.pop_front()
    else {
      return Ok(Some(here));
    };
    let chunk = chunk.unwrap_or_default();
    let FileEntry { parents, flags, .. } =
      store.file_entry(&found, &node).ok_or(Refusal::Store {
        what: "a file revision",
      })?;
    if open.as_deref() != Some(found.as_slice()) {
      if open.is_some() {
        changegroup::put_end(out);
      }
      changegroup::put_name(&found, out).map_err(|_| unencodable("a path"))?;
      self.files.sent.clear();
    }
    let link = node_of(store, link)?;
    match row.base {
      // A stored whole is its raw text (the store's contract, A-767 C1), sent as the one hunk 7.2 sends a whole in.
      None => self.put((node, parents, link, flags), &chunk, room, out)?,
      Some(base) => {
        let base = *self.files.sent.get(&base).ok_or(Refusal::Store {
          what: "a file revision whose stored base this answer did not send",
        })?;
        self.put_delta((node, parents, link, flags), (base, &chunk), room, out)?;
      }
    }
    self.files.sent.insert(row.text, node);
    self.files.stored = self.files.stored.saturating_add(1);
    Ok(Some(next(Some(found.clone()))))
  }

  /// Asks the store for the rows of the file revisions from `from` on that a missing changeset links, as many as
  /// `room` holds (one at least), each log's in turn, scanning a step's rows at most ([`Store::rows_a_step`]): a clone's
  /// file section is the whole history's, never one step's scan or ask.
  fn ask_rows(&mut self, store: &impl Store, (path, position): (&[u8], u64), room: u64) {
    let most = usize::try_from(
      room
        .checked_div(to_u64(core::mem::size_of::<Ahead>()).max(1))
        .unwrap_or(room),
    )
    .unwrap_or(usize::MAX)
    .max(1);
    let mut asked: AskedRows = Vec::new();
    let mut taken = 0_usize;
    let (mut scanned, rows) = (0_u64, store.rows_a_step().max(1));
    let mut at = (path.to_vec(), position);
    while taken < most && scanned < rows {
      let Some((found, position, linked)) = store.file_linked_from((&at.0, at.1)) else {
        break;
      };
      scanned = scanned.saturating_add(1);
      if linked.link.is_some_and(|link| self.missing.contains(link)) {
        match asked.last_mut() {
          Some((last, revisions)) if *last == found => revisions.push((position, linked.node)),
          _ => asked.push((found.clone(), vec![(position, linked.node)])),
        }
        taken = taken.saturating_add(1);
      }
      at = (found, position.saturating_add(1));
    }
    let logs = asked
      .iter()
      .map(|(path, revisions)| {
        (
          path.clone(),
          revisions.iter().map(|(_, node)| *node).collect(),
        )
      })
      .collect();
    self.files.asked = asked;
    self.files.room = room;
    self.files.want = Some(crate::render::Want::Rows { logs });
  }

  /// Appends one revision (its node, parents, link and flags) as the delta `data` against `base`, a revision sent
  /// before it in this answer, its bytes charged to `room` before they are written.
  fn put_delta(
    &self,
    (node, parents, link, flags): (Node, [Node; 2], Node, u16),
    (base, data): (Node, &[u8]),
    room: &mut Admission,
    out: &mut Vec<u8>,
  ) -> Result<(), Refusal> {
    room.charge(to_u64(data.len()))?;
    let revision = Revision {
      node,
      p1: parents[0],
      p2: parents[1],
      base,
      link,
      flags: if self.version == Version::V02 {
        0
      } else {
        flags
      },
      info: None,
      protocol_flags: 0,
      data,
      sidedata: None,
    };
    changegroup::put_revision(&revision, self.version, out).map_err(|_| unencodable("a revision"))
  }

  /// Appends one revision (its node, parents, link and flags) as its full text against the null base, its bytes
  /// charged to `room` before they are written.
  fn put(
    &self,
    revision: (Node, [Node; 2], Node, u16),
    text: &[u8],
    room: &mut Admission,
    out: &mut Vec<u8>,
  ) -> Result<(), Refusal> {
    put_whole(revision, self.version, text, room, out)
  }
}

/// A pull's file section (A-767 C1 step 1b; main's ruling of 2026-11-21 08:45, with its condition on pulls): each file
/// revision the client lacks, its logs in their paths' order and each in its own, read from the node's changeset store
/// a batch at a time, never a whole text from the content store. First the rows of a batch and of its revisions'
/// parents; then for each revision its stored edit where the edit's base is on the client (a parent whose changeset the
/// common set holds) or sent earlier in its group, else its whole text, made by the store from its chain, sent against
/// the null base and counted; then their bytes, asked as the store keeps them; then each sent.
#[derive(Debug)]
pub(crate) struct Pull {
  /// Each file log's path and the revisions it sends, in order.
  plan: Vec<(Vec<u8>, Vec<PlannedRevision>)>,
  /// The changesets the client holds: the common heads and their ancestors.
  held: RevSet,
  /// The group and the revision in it next.
  at: (usize, usize),
  /// Whether the group being sent has its name written.
  open: bool,
  /// The group's texts sent, by identity: the node each was sent as.
  sent: BTreeMap<[u8; 32], Node>,
  /// What the next step asks of the store.
  want: Option<crate::render::Want>,
  /// The revisions read ahead, from `at` on, in the plan's order.
  batch: std::collections::VecDeque<Batched>,
  /// The rows given, by path and node: the batch's revisions' and their parents'.
  rows: BTreeMap<(Vec<u8>, Node), crate::render::StoredRevision>,
  /// The rows asked, each log's path and nodes, in order.
  rows_asked: Vec<(Vec<u8>, Vec<Node>)>,
  /// The batch's revisions whose bytes are asked, in the order asked.
  data_asked: Vec<usize>,
  /// The revisions sent as the store keeps them.
  stored: u64,
  /// The revisions sent whole against the null base.
  whole: u64,
  /// Whether the file section's end is written.
  ended: bool,
  /// Whether the store gave fewer rows or bytes than were asked: refused at the next step.
  short: bool,
  /// The room a whole text was asked within, where the store found the text past it: refused at the next step.
  past: Option<u64>,
  /// The room the last whole text was asked within.
  texts_most: Option<u64>,
}

/// A pull's file revision read ahead: where it is in the plan, how it is sent once decided, and its bytes once given.
#[derive(Debug)]
struct Batched {
  /// Its group and its index in it.
  at: (usize, usize),
  /// How it is sent.
  send: Option<Sending>,
  /// Its stored edit or whole, or its whole text, once given.
  data: Option<Vec<u8>>,
}

/// How a pull sends a file revision.
#[derive(Clone, Debug)]
enum Sending {
  /// As the store keeps it, against `base` (the null node for a stored whole).
  Stored {
    /// The node its edit is against.
    base: Node,
    /// Where its edit or whole lies.
    place: crate::render::StoredPlace,
  },
  /// Its whole text, made by the store, against the null base.
  Whole {
    /// The text's identity.
    text: [u8; 32],
  },
}

impl Pull {
  /// A pull's file section over `plan`, the client holding `held`.
  fn new(plan: Vec<(Vec<u8>, Vec<PlannedRevision>)>, held: RevSet) -> Pull {
    Pull {
      plan,
      held,
      at: (0, 0),
      open: false,
      sent: BTreeMap::new(),
      want: None,
      batch: std::collections::VecDeque::new(),
      rows: BTreeMap::new(),
      rows_asked: Vec::new(),
      data_asked: Vec::new(),
      stored: 0,
      whole: 0,
      ended: false,
      short: false,
      past: None,
      texts_most: None,
    }
  }

  /// The store's answer to the pull's want.
  fn give(&mut self, given: Given) {
    self.want = None;
    match given {
      Given::Rows(rows) => {
        let asked = core::mem::take(&mut self.rows_asked);
        self.short |= rows.len() != asked.len();
        for ((path, nodes), rows) in asked.into_iter().zip(rows) {
          self.short |= rows.len() != nodes.len();
          for (node, row) in nodes.into_iter().zip(rows) {
            if let Some(row) = row {
              self.rows.insert((path.clone(), node), row);
            }
          }
        }
      }
      Given::TextsPast => {
        self.data_asked.clear();
        self.past = self.texts_most;
      }
      Given::Chunks(data) | Given::Texts(data) => {
        let asked = core::mem::take(&mut self.data_asked);
        self.short |= data.len() != asked.len();
        for (at, data) in asked.into_iter().zip(data) {
          if let Some(batched) = self.batch.get_mut(at) {
            batched.data = Some(data);
          }
        }
      }
    }
  }

  /// Takes the pull's next unit (module docs of [`Pull`]): whether more follow.
  fn step(
    &mut self,
    store: &impl Store,
    version: Version,
    room: &mut Admission,
    out: &mut Vec<u8>,
  ) -> Result<bool, Refusal> {
    if let Some(left) = self.past {
      return Err(Refusal::PastAdmission(crate::PastAdmission {
        asked: left.saturating_add(1),
        left,
      }));
    }
    if self.short {
      return Err(Refusal::Store {
        what: "a file revision the changeset store gave short",
      });
    }
    let (group, index) = self.at;
    let Some((path, revisions)) = self.plan.get(group) else {
      if self.open {
        changegroup::put_end(out);
      }
      changegroup::put_end(out);
      self.ended = true;
      return Ok(false);
    };
    let path = path.clone();
    let Some(planned) = revisions.get(index).copied() else {
      if self.open {
        changegroup::put_end(out);
      }
      self.open = false;
      self.sent.clear();
      self.at = (group.saturating_add(1), 0);
      return Ok(true);
    };
    if self.batch.front().map(|batched| batched.at) != Some((group, index)) {
      self.batch.clear();
      self.rows.clear();
      self.ask_rows(room.left(), store.rows_a_step());
      return Ok(true);
    }
    if self.batch.iter().any(|batched| batched.send.is_none()) {
      self.decide(store)?;
    }
    if self
      .batch
      .front()
      .is_some_and(|batched| batched.data.is_none())
    {
      self.ask_data(room.left());
      return Ok(true);
    }
    let Some(Batched {
      send: Some(send),
      data: Some(data),
      ..
    }) = self.batch.pop_front()
    else {
      return Err(Refusal::Store {
        what: "a file revision",
      });
    };
    let text = self
      .rows
      .get(&(path.clone(), planned.node))
      .map(|row| row.text)
      .ok_or(Refusal::Store {
        what: "a file revision the changeset store keeps no row of",
      })?;
    if !self.open {
      changegroup::put_name(&path, out).map_err(|_| unencodable("a path"))?;
      self.open = true;
    }
    // A stored whole is its raw text (the store's contract, A-767 C1), and a text made whole is one: each sent as the
    // one hunk 7.2 sends a whole in.
    let (base, made_whole) = match send {
      Sending::Stored { base, .. } => (base, false),
      Sending::Whole { .. } => (NULL_NODE, true),
    };
    let whole = base == NULL_NODE;
    let hunk = if whole { delta::HUNK_HEADER_LEN } else { 0 };
    room.charge(to_u64(data.len().saturating_add(hunk)))?;
    let revision = Revision {
      node: planned.node,
      p1: planned.entry.parents[0],
      p2: planned.entry.parents[1],
      base,
      link: planned.link,
      flags: if version == Version::V02 {
        0
      } else {
        planned.entry.flags
      },
      info: None,
      protocol_flags: 0,
      data: &data,
      sidedata: None,
    };
    let written = if whole {
      changegroup::put_revision_whole(&revision, version, out)
    } else {
      changegroup::put_revision(&revision, version, out)
    };
    written.map_err(|_| unencodable("a revision"))?;
    if made_whole {
      self.whole = self.whole.saturating_add(1);
    } else {
      self.stored = self.stored.saturating_add(1);
    }
    self.sent.insert(text, planned.node);
    self.at = (group, index.saturating_add(1));
    Ok(true)
  }

  /// Asks the rows of as many planned revisions from `at` on as `room` holds and as `rows` rows bound (theirs and their
  /// parents', [`Store::rows_a_step`]), one revision at least.
  fn ask_rows(&mut self, room: u64, rows: u64) {
    let most = usize::try_from(
      room
        .checked_div(to_u64(core::mem::size_of::<Batched>()).max(1))
        .unwrap_or(room),
    )
    .unwrap_or(usize::MAX)
    .max(1);
    let (mut group, mut index) = self.at;
    let mut asked: Vec<(Vec<u8>, Vec<Node>)> = Vec::new();
    let mut asked_rows = 0_u64;
    while self.batch.len() < most {
      let Some((path, revisions)) = self.plan.get(group) else {
        break;
      };
      let Some(planned) = revisions.get(index) else {
        group = group.saturating_add(1);
        index = 0;
        continue;
      };
      let nodes: Vec<Node> = core::iter::once(planned.node)
        .chain(
          planned
            .entry
            .parents
            .into_iter()
            .filter(|parent| *parent != NULL_NODE),
        )
        .collect();
      // Within the rows a step reads, one revision at least.
      let these = to_u64(nodes.len());
      if !self.batch.is_empty() && asked_rows.saturating_add(these) > rows {
        break;
      }
      asked_rows = asked_rows.saturating_add(these);
      match asked.last_mut() {
        Some((last, wanted)) if last == path => wanted.extend(nodes),
        _ => asked.push((path.clone(), nodes)),
      }
      self.batch.push_back(Batched {
        at: (group, index),
        send: None,
        data: None,
      });
      index = index.saturating_add(1);
    }
    self.rows_asked = asked.clone();
    self.want = Some(crate::render::Want::Rows { logs: asked });
  }

  /// Decides how each undecided revision of the batch is sent (module docs of [`Pull`]); refused where the store keeps
  /// no row of one.
  fn decide(&mut self, store: &impl Store) -> Result<(), Refusal> {
    let current = self.at.0;
    for at in 0..self.batch.len() {
      let Some((group, index)) = self
        .batch
        .get(at)
        .filter(|batched| batched.send.is_none())
        .map(|batched| batched.at)
      else {
        continue;
      };
      let (path, revisions) = self.plan.get(group).ok_or(Refusal::Store {
        what: "a file revision",
      })?;
      let planned = revisions.get(index).copied().ok_or(Refusal::Store {
        what: "a file revision",
      })?;
      let row = self
        .rows
        .get(&(path.clone(), planned.node))
        .cloned()
        .ok_or(Refusal::Store {
          what: "a file revision the changeset store keeps no row of",
        })?;
      let send = match row.base {
        None => Sending::Stored {
          base: NULL_NODE,
          place: row.place,
        },
        Some(base) => {
          let sent = (group == current)
            .then(|| self.sent.get(&base).copied())
            .flatten();
          let earlier = revisions
            .get(..index)
            .unwrap_or_default()
            .iter()
            .rev()
            .find(|earlier| {
              self
                .rows
                .get(&(path.clone(), earlier.node))
                .is_some_and(|row| row.text == base)
            })
            .map(|earlier| earlier.node);
          let held_parent = planned.entry.parents.into_iter().find(|parent| {
            *parent != NULL_NODE
              && self
                .rows
                .get(&(path.clone(), *parent))
                .is_some_and(|row| row.text == base)
              && store
                .file_entry(path, parent)
                .and_then(|entry| store.rev(&entry.link))
                .is_some_and(|rev| self.held.contains(rev))
          });
          match sent.or(earlier).or(held_parent) {
            Some(base) => Sending::Stored {
              base,
              place: row.place,
            },
            None => Sending::Whole { text: row.text },
          }
        }
      };
      if let Some(batched) = self.batch.get_mut(at) {
        batched.send = Some(send);
      }
    }
    Ok(())
  }

  /// Asks the bytes of the batch's decided revisions lacking theirs: the stored edits and wholes first, as many as
  /// `room`'s bytes hold (one at least), then the whole texts.
  fn ask_data(&mut self, room: u64) {
    let mut places = Vec::new();
    let mut asked = Vec::new();
    let mut bytes = 0_u64;
    for (at, batched) in self.batch.iter().enumerate() {
      if batched.data.is_some() {
        continue;
      }
      if let Some(Sending::Stored { place, .. }) = &batched.send {
        if !places.is_empty() && bytes.saturating_add(place.len) > room {
          break;
        }
        bytes = bytes.saturating_add(place.len);
        places.push(*place);
        asked.push(at);
      }
    }
    if !places.is_empty() {
      self.data_asked = asked;
      self.want = Some(crate::render::Want::Chunks { places });
      return;
    }
    // A whole text's length is not in its row, so one is asked at a time: what is held is the text sent next.
    let front_whole = self.batch.iter().enumerate().find_map(|(at, batched)| {
      match (&batched.data, &batched.send) {
        (None, Some(Sending::Whole { text })) => Some((at, *text)),
        _ => None,
      }
    });
    let mut texts = Vec::new();
    if let Some((at, text)) = front_whole {
      texts.push(text);
      asked.push(at);
    }
    if !texts.is_empty() {
      self.data_asked = asked;
      self.texts_most = Some(room);
      self.want = Some(crate::render::Want::Texts { texts, most: room });
    }
  }
}

/// A fetch the links cannot walk (a pull, a clone of some heads; 7.2's `cgpacker` off its link-revision fast path),
/// walked by the changesets that name each revision a unit a step (main 01a20182's order of 2026-11-22 10:40: a clone
/// of half of Mercurial's 57,054-changeset history made this changegroup whole in one step, 4.1 s in a debug build,
/// its shard killed for spinning). Each step one missing changeset, oldest first: its entry written whole, linked to
/// itself, its manifest lent (once a changeset) and the file revisions its manifest names at the paths it lists
/// planned, each linked to the first missing changeset naming it, those whose own link the client holds left out; then
/// each manifest the missing changesets name, a step each in its log's order, lent again and written whole; then the
/// file section ([`Pull`]). What it keeps between steps is the plan, never a text. Bounded by what the fetch sends,
/// never the repository.
#[derive(Debug)]
struct Named {
  /// The changesets the client holds: the common heads and their ancestors.
  held: RevSet,
  /// Where it stands.
  stage: NamedStage,
  /// Each manifest a missing changeset names: its position in its log, its node and its link, in their log's order
  /// once the changelog is written.
  manifests: Vec<(u64, Node, Node)>,
  /// The manifests planned.
  seen_manifests: BTreeSet<Node>,
  /// The file revisions planned, by path, each with its position in its log.
  files: BTreeMap<Vec<u8>, Vec<(u64, PlannedRevision)>>,
  /// The file revisions looked up.
  seen_files: BTreeSet<(Vec<u8>, Node)>,
  /// The last manifest sent in `NamedStage::Manifests` (position, node) when serving resident
  /// manifest deltas.
  last_sent: Option<(u64, Node)>,
  /// Whether changeset texts may still be resident in memory (`false` after the first away miss).
  changeset_resident: bool,
  /// Whether manifest texts may still be resident in memory (`false` after the first away miss, avoiding 500-rev
  /// dead `HgIndex::revision_text` walks on every changeset).
  manifest_resident: bool,
  /// Changeset-store state for reading let-go changelogs and manifests in batches and emitting
  /// stored manifest deltas verbatim (goal 93 C1 (1)).
  store: NamedStore,
}

/// Which changeset-store request [`NamedStore`] currently has in flight.
#[derive(Debug, Default)]
enum NamedAsk {
  #[default]
  Idle,
  /// Rows of missing changesets (`CHANGELOG_PATH`).
  ChangesetRows(Vec<Rev>),
  /// Full texts of missing changesets (`CHANGELOG_PATH`).
  ChangesetTexts(Vec<Rev>),
  /// Rows of manifests needed during `NamedStage::Changelog` (`MANIFEST_PATH`).
  PlanManifestRows(Vec<Node>),
  /// Stored chunks of consecutive chainable manifests during `NamedStage::Changelog`.
  PlanManifestChunks(Vec<Node>),
  /// One full manifest text during `NamedStage::Changelog` (first manifest or chain jump).
  PlanManifestText([u8; 32]),
  /// Rows of manifests being sent during `NamedStage::Manifests` (`MANIFEST_PATH`).
  SendManifestRows(Vec<(usize, Node)>),
  /// Stored chunks of manifests being sent during `NamedStage::Manifests`.
  SendManifestChunks(usize),
  /// Full texts of manifests being sent during `NamedStage::Manifests`.
  SendManifestTexts(usize),
}

/// One manifest revision queued for sending in `NamedStage::Manifests`.
#[derive(Debug)]
struct SendManifest {
  /// Index in `Named::manifests`.
  at: usize,
  /// Its node.
  node: Node,
  /// Its stored row from `MANIFEST_PATH`.
  row: crate::render::StoredRevision,
  /// How it will be sent: `Some(NULL_NODE)` whole via chunk, `Some(base)` delta via chunk,
  /// `None` whole via `Want::Texts`.
  send_base: Option<Node>,
  /// Whether `send_base` has been decided yet.
  decided: bool,
  /// The bytes fetched (chunk or whole text), once given.
  data: Option<Vec<u8>>,
}

/// Changeset-store batching state for [`Named`].
#[derive(Debug, Default)]
struct NamedStore {
  /// Pending request for the changeset store.
  want: Option<crate::render::Want>,
  /// What `want` asked for.
  ask: NamedAsk,
  /// Pre-fetched changeset texts, in ascending rev order.
  changesets: std::collections::VecDeque<(Rev, Vec<u8>)>,
  /// Pre-fetched manifest `StoredRevision`s for `NamedStage::Changelog`.
  manifest_rows: BTreeMap<Node, crate::render::StoredRevision>,
  /// Pre-fetched consecutive chainable manifest chunks for `NamedStage::Changelog`.
  manifest_chunks: std::collections::VecDeque<(Node, Vec<u8>)>,
  /// Rolling decoded manifest `(text_identity, full_text)` so consecutive manifest deltas apply in
  /// O(delta_len) instead of rebuilding from delta chains.
  last_manifest: Option<([u8; 32], Vec<u8>)>,
  /// Manifests already sent in `NamedStage::Manifests`, keyed by `row.text` -> sent `Node`.
  sent_manifests: BTreeMap<[u8; 32], Node>,
  /// Batch of manifests queued to send in `NamedStage::Manifests`.
  send_batch: std::collections::VecDeque<SendManifest>,
  /// Whether the store lacked a requested row or text.
  unkept: bool,
  /// Whether a `Want::Texts` exceeded the room bound.
  texts_past: bool,
  /// Room hint from the asking step.
  room: u64,
}

impl NamedStore {
  /// Handles `given` from the changeset store.
  fn give(&mut self, given: Given) {
    self.want = None;
    let ask = core::mem::take(&mut self.ask);
    match (ask, given) {
      (NamedAsk::ChangesetRows(revs), Given::Rows(rows)) => self.give_changeset_rows(revs, rows),
      (NamedAsk::ChangesetTexts(revs), Given::Texts(texts)) => {
        self.give_changeset_texts(revs, texts);
      }
      (NamedAsk::PlanManifestRows(nodes), Given::Rows(rows)) => {
        self.give_plan_manifest_rows(nodes, rows);
      }
      (NamedAsk::PlanManifestChunks(nodes), Given::Chunks(chunks)) => {
        self.give_plan_manifest_chunks(nodes, chunks);
      }
      (NamedAsk::PlanManifestText(identity), Given::Texts(mut texts)) => match texts.pop() {
        Some(text) => self.last_manifest = Some((identity, text)),
        None => self.unkept = true,
      },
      (NamedAsk::SendManifestRows(entries), Given::Rows(rows)) => {
        self.give_send_manifest_rows(entries, rows);
      }
      (NamedAsk::SendManifestChunks(count), Given::Chunks(chunks)) => {
        self.give_send_manifest_data(count, chunks);
      }
      (NamedAsk::SendManifestTexts(count), Given::Texts(texts)) => {
        self.give_send_manifest_data(count, texts);
      }
      (_, Given::TextsPast) => self.texts_past = true,
      _ => self.unkept = true,
    }
  }

  /// Stores fetched changeset rows and requests their full texts.
  fn give_changeset_rows(
    &mut self,
    revs: Vec<Rev>,
    rows: Vec<Vec<Option<crate::render::StoredRevision>>>,
  ) {
    let log = rows.into_iter().next().unwrap_or_default();
    let Some(texts): Option<Vec<[u8; 32]>> = (log.len() == revs.len())
      .then(|| log.into_iter().map(|row| row.map(|row| row.text)).collect())
      .flatten()
    else {
      self.unkept = true;
      return;
    };
    self.ask = NamedAsk::ChangesetTexts(revs);
    self.want = Some(crate::render::Want::Texts {
      texts,
      most: self.room,
    });
  }

  /// Enqueues fetched changeset texts in revision order.
  fn give_changeset_texts(&mut self, revs: Vec<Rev>, texts: Vec<Vec<u8>>) {
    if texts.len() != revs.len() {
      self.unkept = true;
      return;
    }
    self.changesets.extend(revs.into_iter().zip(texts));
  }

  /// Records fetched manifest index rows for planning.
  fn give_plan_manifest_rows(
    &mut self,
    nodes: Vec<Node>,
    rows: Vec<Vec<Option<crate::render::StoredRevision>>>,
  ) {
    let log = rows.into_iter().next().unwrap_or_default();
    if log.len() != nodes.len() {
      self.unkept = true;
      return;
    }
    for (node, row) in nodes.into_iter().zip(log) {
      match row {
        Some(row) => {
          self.manifest_rows.insert(node, row);
        }
        None => {
          self.unkept = true;
          return;
        }
      }
    }
  }

  /// Replaces the manifest planning chunk queue with fetched chunks for rolling delta planning.
  fn give_plan_manifest_chunks(&mut self, nodes: Vec<Node>, chunks: Vec<Vec<u8>>) {
    self.manifest_chunks.clear();
    if chunks.len() != nodes.len() {
      self.unkept = true;
      return;
    }
    self.manifest_chunks.extend(nodes.into_iter().zip(chunks));
  }

  /// Enqueues fetched manifest rows for the send stage.
  fn give_send_manifest_rows(
    &mut self,
    entries: Vec<(usize, Node)>,
    rows: Vec<Vec<Option<crate::render::StoredRevision>>>,
  ) {
    let log = rows.into_iter().next().unwrap_or_default();
    if log.len() != entries.len() {
      self.unkept = true;
      return;
    }
    for ((at, node), row) in entries.into_iter().zip(log) {
      let Some(row) = row else {
        self.unkept = true;
        return;
      };
      self.send_batch.push_back(SendManifest {
        at,
        node,
        row,
        send_base: None,
        decided: false,
        data: None,
      });
    }
  }

  /// Attaches fetched chunks or whole texts to the next `count` pending send-batch items.
  fn give_send_manifest_data(&mut self, count: usize, payloads: Vec<Vec<u8>>) {
    if payloads.len() != count {
      self.unkept = true;
      return;
    }
    for (item, payload) in self
      .send_batch
      .iter_mut()
      .filter(|item| item.data.is_none())
      .take(count)
      .zip(payloads)
    {
      item.data = Some(payload);
    }
  }
}

/// A resolved manifest position (and optional resident text) for [`Named::changeset`].
struct PlannedManifest {
  /// Its position in the manifest log.
  position: u64,
  /// Its resident text, when read in memory rather than staged in `NamedStore::last_manifest`.
  resident: Option<Vec<u8>>,
}

/// Where a [`Named`] walk stands.
#[derive(Clone, Copy, Debug)]
enum NamedStage {
  /// The changelog's group, from this served revision.
  Changelog(Rev),
  /// The manifests' group, from this one of [`Named::manifests`].
  Manifests(usize),
}

impl Named {
  /// Pending changeset-store request, if any.
  fn wants(&self) -> Option<&crate::render::Want> {
    self.store.want.as_ref()
  }

  /// Feeds the changeset store's reply into [`NamedStore`].
  fn give(&mut self, given: Given) {
    self.store.give(given);
  }

  /// The walk's next unit (the type's docs), what it lends and keeps charged to `room`: whether the manifests' group is
  /// written, the file section next ([`Named::pull`]).
  fn step(
    &mut self,
    store: &impl Store,
    (missing, version): (&pull::Ranges, Version),
    room: &mut Admission,
    out: &mut Vec<u8>,
  ) -> Result<bool, Refusal> {
    if self.store.texts_past {
      return Err(Refusal::PastAdmission(crate::PastAdmission {
        asked: self.store.room.saturating_add(1),
        left: self.store.room,
      }));
    }
    match self.stage {
      NamedStage::Changelog(from) => {
        if self.store.unkept {
          return Err(no_changeset_text());
        }
        let Some(rev) = missing.next_from(from) else {
          changegroup::put_end(out);
          self
            .manifests
            .sort_unstable_by_key(|(position, _, _)| *position);
          // Drop planning scratch before streaming manifests.
          self.store.changesets.clear();
          self.store.manifest_rows.clear();
          self.store.manifest_chunks.clear();
          self.store.last_manifest = None;
          self.stage = NamedStage::Manifests(0);
          return Ok(false);
        };
        if self.changeset(store, (missing, rev, version), room, out)? {
          self.stage = NamedStage::Changelog(rev.saturating_add(1));
        }
        Ok(false)
      }
      NamedStage::Manifests(at) => {
        if self.store.unkept {
          return Err(no_manifest());
        }
        let Some((position, node, link)) = self.manifests.get(at).copied() else {
          changegroup::put_end(out);
          if version.has_tree_list() {
            changegroup::put_end(out);
          }
          return Ok(true);
        };
        if self.send_manifest(store, (at, position, node, link), version, room, out)? {
          self.stage = NamedStage::Manifests(at.saturating_add(1));
        }
        Ok(false)
      }
    }
  }

  /// Requests a bounded batch of missing changeset rows starting from `rev`.
  fn ask_changeset_batch(
    &mut self,
    store: &impl Store,
    missing: &pull::Ranges,
    rev: Rev,
    room: u64,
  ) {
    self.store.changesets.clear();
    let rows_cap = store.rows_a_step().max(1);
    let mut revs = Vec::new();
    let mut nodes = Vec::new();
    let mut cursor = rev;
    while to_u64(revs.len()) < rows_cap {
      let Some(next_rev) = missing.next_from(cursor) else {
        break;
      };
      let Some(next_node) = store.node(next_rev) else {
        break;
      };
      revs.push(next_rev);
      nodes.push(next_node);
      cursor = next_rev.saturating_add(1);
    }
    self.store.room = room;
    self.store.ask = NamedAsk::ChangesetRows(revs);
    self.store.want = Some(crate::render::Want::Rows {
      logs: vec![(crate::push::intake::CHANGELOG_PATH.to_vec(), nodes)],
    });
  }

  /// Requests a bounded batch of upcoming distinct manifest rows for planning.
  fn ask_plan_manifest_rows(
    &mut self,
    store: &impl Store,
    missing: &pull::Ranges,
    (rev, manifest): (Rev, Node),
    room: u64,
  ) {
    let rows_cap = store.rows_a_step().max(1);
    let mut nodes = vec![manifest];
    let mut seen = BTreeSet::from([manifest]);
    let mut cursor = rev.saturating_add(1);
    while to_u64(nodes.len()) < rows_cap {
      let Some(next_rev) = missing.next_from(cursor) else {
        break;
      };
      if let Some(next_mf) = store.manifest_node(next_rev)
        && next_mf != NULL_NODE
        && !self.store.manifest_rows.contains_key(&next_mf)
        && seen.insert(next_mf)
      {
        nodes.push(next_mf);
      }
      cursor = next_rev.saturating_add(1);
    }
    self.store.room = room;
    self.store.ask = NamedAsk::PlanManifestRows(nodes.clone());
    self.store.want = Some(crate::render::Want::Rows {
      logs: vec![(crate::push::intake::MANIFEST_PATH.to_vec(), nodes)],
    });
  }

  /// Requests consecutive chainable manifest chunks starting from `(rev, manifest, row)`.
  fn ask_plan_manifest_chunks(
    &mut self,
    store: &impl Store,
    missing: &pull::Ranges,
    (rev, manifest, row): (Rev, Node, crate::render::StoredRevision),
    budget: u64,
  ) {
    self.store.manifest_chunks.clear();
    let mut chunk_nodes = vec![manifest];
    let mut places = vec![row.place];
    let mut bytes = row.place.len;
    let mut prev_id = row.text;
    let mut cursor = rev.saturating_add(1);
    while let Some(next_rev) = missing.next_from(cursor) {
      cursor = next_rev.saturating_add(1);
      let Some(next_mf) = store.manifest_node(next_rev) else {
        break;
      };
      if next_mf == NULL_NODE || next_mf == manifest || chunk_nodes.last() == Some(&next_mf) {
        continue;
      }
      let Some(next_row) = self.store.manifest_rows.get(&next_mf).copied() else {
        break;
      };
      let chains = next_row.base.is_none() || next_row.base == Some(prev_id);
      if !chains || bytes.saturating_add(next_row.place.len) > budget {
        break;
      }
      bytes = bytes.saturating_add(next_row.place.len);
      places.push(next_row.place);
      chunk_nodes.push(next_mf);
      prev_id = next_row.text;
    }
    self.store.room = budget;
    self.store.ask = NamedAsk::PlanManifestChunks(chunk_nodes);
    self.store.want = Some(crate::render::Want::Chunks { places });
  }

  /// Applies one queued `(queued_node, chunk)` onto `self.store.last_manifest` when its stored
  /// row is known and chains cleanly onto `last_manifest` (or is a whole text). Returns `false` if
  /// the queued entry cannot chain onto `last_manifest`.
  fn apply_queued_manifest(
    &mut self,
    queued_node: Node,
    chunk: Vec<u8>,
    room: &mut Admission,
  ) -> Result<bool, Refusal> {
    let Some(queued_row) = self.store.manifest_rows.get(&queued_node).copied() else {
      return Ok(false);
    };
    let full = match queued_row.base {
      None => chunk,
      Some(base_id) => {
        let Some((prev_id, prev_text)) = self.store.last_manifest.as_ref() else {
          return Ok(false);
        };
        if *prev_id != base_id {
          return Ok(false);
        }
        delta::apply(prev_text, &chunk, room).map_err(|_| no_manifest())?
      }
    };
    self.store.last_manifest = Some((queued_row.text, full));
    Ok(true)
  }

  /// Ensures `self.store.last_manifest` holds `manifest`'s full text for planning, asking the
  /// changeset store (returning `Ok(false)`) when rows, chunks, or a full text must be fetched.
  fn load_plan_manifest(
    &mut self,
    store: &impl Store,
    (missing, rev, manifest): (&pull::Ranges, Rev, Node),
    room: &mut Admission,
  ) -> Result<bool, Refusal> {
    let Some(row) = self.store.manifest_rows.get(&manifest).copied() else {
      self.ask_plan_manifest_rows(store, missing, (rev, manifest), room.left());
      return Ok(false);
    };
    if self
      .store
      .last_manifest
      .as_ref()
      .is_some_and(|(id, _)| *id == row.text)
    {
      return Ok(true);
    }
    // Advance through any queued chunks up to `manifest` (commits with no touched files skip
    // `load_plan_manifest`, leaving their intermediate delta in `manifest_chunks` ahead of the
    // next manifest that chains onto it). If `manifest` is not queued or the chain diverges, drop
    // the stale queue so we never loop re-fetching behind an unconsumed front entry.
    if self
      .store
      .manifest_chunks
      .iter()
      .any(|(queued, _)| *queued == manifest)
    {
      while let Some((queued, chunk)) = self.store.manifest_chunks.pop_front() {
        if !self.apply_queued_manifest(queued, chunk, room)? {
          self.store.manifest_chunks.clear();
          break;
        }
        if queued == manifest {
          return Ok(true);
        }
      }
    } else {
      self.store.manifest_chunks.clear();
    }
    let chainable = row.base.is_none()
      || self
        .store
        .last_manifest
        .as_ref()
        .is_some_and(|(id, _)| Some(*id) == row.base);
    if chainable {
      self.ask_plan_manifest_chunks(store, missing, (rev, manifest, row), room.left());
    } else {
      self.store.room = room.left();
      self.store.ask = NamedAsk::PlanManifestText(row.text);
      self.store.want = Some(crate::render::Want::Texts {
        texts: vec![row.text],
        most: room.left(),
      });
    }
    Ok(false)
  }

  /// Parses a changeset's manifest node and touched paths.
  fn parse_changeset_touched(bytes: &[u8]) -> Result<(Node, Vec<Vec<u8>>), Refusal> {
    let (manifest, touched_raw) = read_changeset(bytes)?;
    let touched = touched_raw
      .split(|byte| *byte == NEWLINE)
      .filter(|path| !path.is_empty())
      .map(<[u8]>::to_vec)
      .collect();
    Ok((manifest, touched))
  }

  /// Resolves `manifest`'s log position and ensures its text is available (either returned or in
  /// `self.store.last_manifest`) when `need_text` is true; returns `Ok(None)` when paused on the
  /// changeset store.
  fn resolve_plan_manifest(
    &mut self,
    store: &impl Store,
    (missing, rev, manifest, need_text): (&pull::Ranges, Rev, Node, bool),
    room: &mut Admission,
  ) -> Result<Option<PlannedManifest>, Refusal> {
    if !need_text {
      if let Some(entry) = store.manifest_entry(&manifest) {
        return Ok(Some(PlannedManifest {
          position: entry.position,
          resident: None,
        }));
      }
      let logged = store.manifest(&manifest, room)?.ok_or_else(no_manifest)?;
      manifest::decode(&logged.text).map_err(|_| no_manifest())?;
      return Ok(Some(PlannedManifest {
        position: logged.position,
        resident: None,
      }));
    }
    if self.manifest_resident {
      if let Some(logged) = store.manifest(&manifest, room)? {
        manifest::decode(&logged.text).map_err(|_| no_manifest())?;
        return Ok(Some(PlannedManifest {
          position: logged.position,
          resident: Some(logged.text.into_owned()),
        }));
      }
      self.manifest_resident = false;
    }
    let entry = store.manifest_entry(&manifest).ok_or_else(no_manifest)?;
    if !self.load_plan_manifest(store, (missing, rev, manifest), room)? {
      return Ok(None);
    }
    Ok(Some(PlannedManifest {
      position: entry.position,
      resident: None,
    }))
  }

  /// Plans any unseen file revisions named by `touched` in either `resident` or
  /// `self.store.last_manifest` (zero copy via disjoint field borrows).
  fn plan_touched_files(
    &mut self,
    store: &impl Store,
    touched: &[Vec<u8>],
    node: Node,
    resident: Option<&[u8]>,
    room: &mut Admission,
  ) -> Result<(), Refusal> {
    let manifest_text = resident
      .or_else(|| {
        self
          .store
          .last_manifest
          .as_ref()
          .map(|(_, text)| text.as_slice())
      })
      .unwrap_or(&[]);
    for path in touched {
      let Some(found) = manifest::find(manifest_text, path) else {
        continue;
      };
      if !self.seen_files.insert((path.clone(), found.node)) {
        continue;
      }
      let entry = store.file_entry(path, &found.node).ok_or(Refusal::Store {
        what: "a file revision",
      })?;
      room.charge_items::<(u64, PlannedRevision, Vec<u8>, Node)>(1)?;
      room.charge(to_u64(path.len()).saturating_mul(2))?;
      if !store
        .rev(&entry.link)
        .is_some_and(|rev| self.held.contains(rev))
      {
        self.files.entry(path.clone()).or_default().push((
          entry.position,
          PlannedRevision {
            node: found.node,
            entry,
            link: node,
          },
        ));
      }
    }
    Ok(())
  }

  /// Missing changeset `rev` written whole, linked to itself, and what its manifest names at the
  /// paths it lists planned. Returns `false` when paused on a changeset-store request.
  fn changeset(
    &mut self,
    store: &impl Store,
    (missing, rev, version): (&pull::Ranges, Rev, Version),
    room: &mut Admission,
    out: &mut Vec<u8>,
  ) -> Result<bool, Refusal> {
    let node = node_of(store, rev)?;
    let parent =
      |parent: Option<Rev>| parent.map_or(Ok(NULL_NODE), |parent| node_of(store, parent));
    let [first, second] = parents_of(store, rev)?;
    let parents = [parent(first)?, parent(second)?];
    let from_store = self
      .store
      .changesets
      .front()
      .is_some_and(|(queued, _)| *queued == rev);
    let (text_owned, manifest, touched) = if from_store {
      let bytes = self
        .store
        .changesets
        .front()
        .map_or(&[][..], |(_, bytes)| bytes.as_slice());
      let (manifest, touched) = Self::parse_changeset_touched(bytes)?;
      (None, manifest, touched)
    } else if self.changeset_resident
      && let Some(text) = store.changeset_text(rev, room)?
    {
      let owned = text.into_owned();
      let (manifest, touched) = Self::parse_changeset_touched(&owned)?;
      (Some(owned), manifest, touched)
    } else {
      self.changeset_resident = false;
      self.ask_changeset_batch(store, missing, rev, room.left());
      return Ok(false);
    };
    let Some(PlannedManifest {
      position: manifest_pos,
      resident: resident_manifest,
    }) = self.resolve_plan_manifest(store, (missing, rev, manifest, !touched.is_empty()), room)?
    else {
      return Ok(false);
    };
    let popped = if from_store {
      self.store.changesets.pop_front().map(|(_, bytes)| bytes)
    } else {
      text_owned
    };
    put_whole(
      (node, parents, node, 0),
      version,
      popped.as_deref().unwrap_or(&[]),
      room,
      out,
    )?;
    if self.seen_manifests.insert(manifest) {
      room.charge_items::<(u64, Node, Node)>(1)?;
      self.manifests.push((manifest_pos, manifest, node));
    }
    if !touched.is_empty() {
      self.plan_touched_files(store, &touched, node, resident_manifest.as_deref(), room)?;
    }
    Ok(true)
  }

  /// Requests a bounded batch of manifest rows starting at `at` for the send stage.
  fn ask_send_manifest_rows(&mut self, store: &impl Store, at: usize, budget: u64) {
    self.store.send_batch.clear();
    let most = usize::try_from(
      budget
        .checked_div(to_u64(core::mem::size_of::<SendManifest>()).max(1))
        .unwrap_or(budget),
    )
    .unwrap_or(usize::MAX)
    .max(1);
    let rows_cap = usize::try_from(store.rows_a_step().max(1)).unwrap_or(usize::MAX);
    let take = most
      .min(rows_cap)
      .min(self.manifests.len().saturating_sub(at))
      .max(1);
    let mut entries = Vec::with_capacity(take);
    let mut nodes = Vec::with_capacity(take);
    for offset in 0..take {
      let idx = at.saturating_add(offset);
      if let Some(&(_, mf_node, _)) = self.manifests.get(idx) {
        entries.push((idx, mf_node));
        nodes.push(mf_node);
      }
    }
    self.store.room = budget;
    self.store.ask = NamedAsk::SendManifestRows(entries);
    self.store.want = Some(crate::render::Want::Rows {
      logs: vec![(crate::push::intake::MANIFEST_PATH.to_vec(), nodes)],
    });
  }

  /// Marks each undecided item in `send_batch` as either a stored delta (against a base already
  /// sent or earlier in this batch) or a whole-text fetch.
  fn decide_send_batch(&mut self) {
    if !self.store.send_batch.iter().any(|item| !item.decided) {
      return;
    }
    let mut planned: BTreeMap<[u8; 32], Node> = BTreeMap::new();
    for item in &mut self.store.send_batch {
      if !item.decided {
        item.send_base = match item.row.base {
          None => Some(NULL_NODE),
          Some(base_id) => self
            .store
            .sent_manifests
            .get(&base_id)
            .or_else(|| planned.get(&base_id))
            .copied(),
        };
        item.decided = true;
      }
      planned.insert(item.row.text, item.node);
    }
  }

  /// Requests chunks (for stored deltas/wholes) or a full text for the front of `send_batch`.
  fn ask_send_batch_data(&mut self, budget: u64) {
    let front_chunk = self
      .store
      .send_batch
      .front()
      .is_some_and(|item| item.send_base.is_some());
    self.store.room = budget;
    if front_chunk {
      let mut places = Vec::new();
      let mut bytes = 0_u64;
      for item in self.store.send_batch.iter().filter(|i| i.data.is_none()) {
        if item.send_base.is_none()
          || (!places.is_empty() && bytes.saturating_add(item.row.place.len) > budget)
        {
          break;
        }
        bytes = bytes.saturating_add(item.row.place.len);
        places.push(item.row.place);
      }
      self.store.ask = NamedAsk::SendManifestChunks(places.len());
      self.store.want = Some(crate::render::Want::Chunks { places });
    } else if let Some(item) = self.store.send_batch.iter().find(|i| i.data.is_none()) {
      self.store.ask = NamedAsk::SendManifestTexts(1);
      self.store.want = Some(crate::render::Want::Texts {
        texts: vec![item.row.text],
        most: budget,
      });
    }
  }

  /// Sends manifest `at` (`position`, `node`, `link`) in `NamedStage::Manifests`: emits stored
  /// deltas verbatim when the base was already sent in this answer (cutting partial-clone manifest
  /// bandwidth and CPU by orders of magnitude), falling back to `Want::Texts` only at chain starts.
  /// Returns `false` when paused on a changeset-store request.
  fn send_manifest(
    &mut self,
    store: &impl Store,
    (at, position, node, link): (usize, u64, Node, Node),
    version: Version,
    room: &mut Admission,
    out: &mut Vec<u8>,
  ) -> Result<bool, Refusal> {
    let ready_front = self
      .store
      .send_batch
      .front()
      .is_some_and(|item| item.at == at);
    if self.manifest_resident && !ready_front && self.store.send_batch.is_empty() {
      if let Some(logged) = store.manifest(&node, room)? {
        put_whole(
          (node, logged.parents, link, logged.flags),
          version,
          &logged.text,
          room,
          out,
        )?;
        self.last_sent = Some((position, node));
        return Ok(true);
      }
      self.manifest_resident = false;
    }
    if !ready_front {
      self.ask_send_manifest_rows(store, at, room.left());
      return Ok(false);
    }
    self.decide_send_batch();
    if self
      .store
      .send_batch
      .front()
      .is_some_and(|item| item.data.is_none())
    {
      self.ask_send_batch_data(room.left());
      return Ok(false);
    }
    let Some(SendManifest {
      node: sent_node,
      row,
      send_base,
      data: Some(data),
      ..
    }) = self.store.send_batch.pop_front()
    else {
      return Err(no_manifest());
    };
    let FileEntry { parents, flags, .. } =
      store.manifest_entry(&sent_node).ok_or_else(no_manifest)?;
    match send_base {
      Some(base) if base != NULL_NODE => {
        put_delta_version(
          (sent_node, parents, link, flags),
          (base, &data),
          version,
          room,
          out,
        )?;
      }
      _ => put_whole((sent_node, parents, link, flags), version, &data, room, out)?,
    }
    self.store.sent_manifests.insert(row.text, sent_node);
    Ok(true)
  }

  /// The file section over what was planned, each group in its log's order.
  fn pull(self) -> Pull {
    let files = self
      .files
      .into_iter()
      .map(|(path, mut revisions)| {
        revisions.sort_unstable_by_key(|(position, _)| *position);
        (
          path,
          revisions.into_iter().map(|(_, planned)| planned).collect(),
        )
      })
      .collect();
    Pull::new(files, self.held)
  }
}

/// A revision written as a stored delta against `base` in `version`, charged to `room`.
fn put_delta_version(
  (node, parents, link, flags): (Node, [Node; 2], Node, u16),
  (base, data): (Node, &[u8]),
  version: Version,
  room: &mut Admission,
  out: &mut Vec<u8>,
) -> Result<(), Refusal> {
  room.charge(to_u64(data.len()))?;
  let revision = Revision {
    node,
    p1: parents[0],
    p2: parents[1],
    base,
    link,
    flags: if version == Version::V02 { 0 } else { flags },
    info: None,
    protocol_flags: 0,
    data,
    sidedata: None,
  };
  changegroup::put_revision(&revision, version, out).map_err(|_| unencodable("a revision"))
}

/// A revision written whole against the null base in `version`, its delta charged to `room`.
fn put_whole(
  (node, parents, link, flags): (Node, [Node; 2], Node, u16),
  version: Version,
  text: &[u8],
  room: &mut Admission,
  out: &mut Vec<u8>,
) -> Result<(), Refusal> {
  room.charge(to_u64(delta::HUNK_HEADER_LEN.saturating_add(text.len())))?;
  let revision = Revision {
    node,
    p1: parents[0],
    p2: parents[1],
    base: NULL_NODE,
    link,
    flags: if version == Version::V02 { 0 } else { flags },
    info: None,
    protocol_flags: 0,
    data: text,
    sidedata: None,
  };
  changegroup::put_revision_whole(&revision, version, out).map_err(|_| unencodable("a revision"))
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

/// A file revision a pull sends: its node, its log's row, and the changeset this changegroup links it to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PlannedRevision {
  /// The node.
  node: Node,
  /// Its log's row.
  entry: FileEntry,
  /// The changeset it is linked to in this changegroup.
  link: Node,
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

/// The markers relevant to the ancestors of `heads` (`obsstore.relevantmarkers`), sorted, each as 7.2 holds it (a
/// format 0 marker's parents are in its metadata; [`Tuple`]).
fn relevant_markers<'s>(
  store: &'s impl Store,
  heads: &[Rev],
  admission: &mut Admission,
) -> Result<Vec<Tuple<'s>>, Refusal> {
  admission.charge_items::<Tuple<'_>>(store.markers().count())?;
  let all: Vec<Tuple<'s>> = store
    .markers()
    .map(|marker| Tuple::of(&marker))
    .collect::<Result<_, _>>()
    .map_err(|_| unencodable("a marker whose timezone is past 64 bits"))?;
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
  let mut markers: Vec<Tuple<'s>> = Vec::with_capacity(found.len());
  let mut all: Vec<Option<Tuple<'s>>> = all.into_iter().map(Some).collect();
  for at in found {
    if let Some(marker) = all.get_mut(at).and_then(Option::take) {
      markers.push(marker);
    }
  }
  markers.sort_by(Tuple::order);
  Ok(markers)
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
  fn new(markers: &'m [Tuple<'_>]) -> Self {
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
