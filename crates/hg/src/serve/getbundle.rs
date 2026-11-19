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
//! **Relevant markers** (`obsstore.relevantmarkers`): those naming one of the nodes as a successor, prunes naming one
//! as predecessor or parent, and then, again and again, those of each found marker's predecessor; sorted as
//! `obsutil.sortedmarkers` sorts 7.2's marker tuples: by predecessor, successors, flags, metadata, date and parents,
//! a marker without parent data ordered as one with none. 7.2 lifts a format 0 marker's date and parents out of its
//! metadata before it sorts; a format 0 marker held here sorts by its metadata as held (7.2 writes format 1).

use super::pull::{self, Outgoing};
use super::{Refusal, Repository, Rev, RevSet, node_of, parents_of};
use crate::bundle2::{Bundle2, Event, Param, Part};
use crate::capabilities::{self, Bundle2Capabilities};
use crate::changegroup::{Changegroup, Group, Revision, Version};
use crate::obsmarkers::{self, Format, Marker};
use crate::out::{Measure, Out};
use crate::read::to_u64;
use crate::{Admission, NULL_NODE, Node, bookmarks, changeset, delta, phases, pushkey, quote};
use core::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};

/// The served view with what its changesets name: their texts, and the manifest and file logs' revisions.
pub trait Store: Repository {
  /// A changeset's text, `None` past the view.
  fn changeset_text(&self, rev: Rev) -> Option<&[u8]>;

  /// The manifest revision of `node`.
  fn manifest(&self, node: &Node) -> Option<Logged<'_>>;

  /// The revision of the file at `path` whose node is `node`.
  fn file(&self, path: &[u8], node: &Node) -> Option<Logged<'_>>;

  /// The node the manifest `manifest` names for `path`, `None` where it names none.
  fn file_node(&self, manifest: &Node, path: &[u8]) -> Option<Node>;

  /// The repository's obsolescence markers, in any order.
  fn markers(&self) -> impl Iterator<Item = Marker<'_>>;
}

/// A manifest or file revision as its log keeps it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Logged<'a> {
  /// Its position in its log: what orders a group.
  pub position: u64,
  /// Its parents, the null node for each that is missing.
  pub parents: [Node; 2],
  /// The changeset its log links it to.
  pub link: Node,
  /// Its revision flags.
  pub flags: u16,
  /// Its text.
  pub text: &'a [u8],
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
pub fn getbundle(
  store: &impl Store,
  asked: &Asked<'_>,
  admission: &mut Admission,
  out: &mut Vec<u8>,
) -> Result<(), Refusal> {
  let outgoing = pull::outgoing(store, asked.heads, asked.common, admission)?;
  let mut unquoted = Vec::new();
  let caps = bundle2_capabilities(asked.bundlecaps, admission, &mut unquoted)?;
  let mut parts = Vec::new();
  parts.extend(changegroup_part(
    store, &outgoing, asked.cg, &caps, admission,
  )?);
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
  frame(&parts, admission, out)
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
/// the version first, then none where nothing is missing; else the changegroup and its count of changesets.
fn changegroup_part(
  store: &impl Store,
  outgoing: &Outgoing,
  cg: Option<&[u8]>,
  caps: &Bundle2Capabilities<'_>,
  admission: &mut Admission,
) -> Result<Option<Built>, Refusal> {
  if !flag(cg, true) || caps.iter().next().is_none() {
    return Ok(None);
  }
  let (name, version) = changegroup_version(caps)?;
  if outgoing.missing.is_empty() {
    return Ok(None);
  }
  let payload = changegroup(store, outgoing, version, admission)?;
  Ok(Some(Built {
    kind: b"CHANGEGROUP",
    mandatory: vec![(&b"version"[..], name.to_vec())],
    advisory: vec![(
      &b"nbchanges"[..],
      outgoing.missing.len().to_string().into_bytes(),
    )],
    payload,
  }))
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

/// Frames the parts as one bundle2 stream, its length charged before it is written.
fn frame(parts: &[Built], admission: &mut Admission, out: &mut Vec<u8>) -> Result<(), Refusal> {
  // Format: a payload chunk's length is a signed 32-bit field, so a chunk holds at most `i32::MAX` bytes.
  let largest = usize::try_from(i32::MAX).unwrap_or(usize::MAX);
  let mut stream = Bundle2 {
    params: Vec::new(),
    parts: Vec::with_capacity(parts.len()),
    events: Vec::new(),
  };
  for (index, built) in parts.iter().enumerate() {
    let params = built
      .mandatory
      .iter()
      .chain(&built.advisory)
      .map(|(key, value)| Param {
        key,
        value: value.as_slice(),
      })
      .collect();
    stream.parts.push(Part {
      kind: built.kind,
      id: u32::try_from(index).unwrap_or(u32::MAX),
      params,
      mandatory_params: built.mandatory.len(),
      depth: 0,
    });
    stream.events.push(Event::Open(index));
    stream
      .events
      .extend(built.payload.chunks(largest).map(Event::Chunk));
    stream.events.push(Event::Close);
  }
  let len = stream.encoded_len().map_err(|_| unencodable("a part"))?;
  admission.charge(len)?;
  stream.encode(out).map_err(|_| unencodable("a part"))
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
  /// Its full text.
  text: &'s [u8],
}

/// The changegroup of `outgoing` in `version`, encoded.
pub fn changegroup(
  store: &impl Store,
  outgoing: &Outgoing,
  version: Version,
  admission: &mut Admission,
) -> Result<Vec<u8>, Refusal> {
  let held = common_ancestors(store, &outgoing.common, admission)?;
  let changelog = changelog_entries(store, &outgoing.missing, admission)?;
  let (manifests, files) = named_revisions(store, &outgoing.missing, &held, admission)?;
  let mut groups: Vec<(&[u8], Vec<Entry<'_>>)> = Vec::with_capacity(files.len());
  groups.extend(files);
  encode_changegroup(version, &changelog, &manifests, &groups, admission)
}

/// The revisions every common revision descends from: the common revisions and all their ancestors.
fn common_ancestors(
  store: &impl Store,
  common: &[Rev],
  admission: &mut Admission,
) -> Result<RevSet, Refusal> {
  let mut held = RevSet::new(store.count(), admission)?;
  let Some(top) = common.iter().copied().max() else {
    return Ok(held);
  };
  for rev in common {
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

/// The missing changesets' entries, oldest first, each linked to itself.
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
    entries.push(Entry {
      node,
      parents: [parent(first)?, parent(second)?],
      link: node,
      flags: 0,
      text: store.changeset_text(*rev).ok_or(Refusal::Store {
        what: "a changeset's text",
      })?,
    });
  }
  Ok(entries)
}

/// A changeset's manifest node and the paths it touches.
fn read_changeset(store: &impl Store, rev: Rev) -> Result<(Node, &[u8]), Refusal> {
  let text = store.changeset_text(rev).ok_or(Refusal::Store {
    what: "a changeset's text",
  })?;
  let changeset = changeset::decode(text).map_err(|_| Refusal::Store {
    what: "a changeset's text",
  })?;
  let manifest = pushkey::hex_node(changeset.manifest)
    .ok()
    .flatten()
    .ok_or(Refusal::Store {
      what: "a changeset's manifest node",
    })?;
  Ok((manifest, changeset.files))
}

/// The manifests a changegroup carries and its file groups by path.
type Named<'s> = (Vec<Entry<'s>>, BTreeMap<&'s [u8], Vec<Entry<'s>>>);

/// Every manifest the missing changesets name and, by path, the file revisions their manifests name at the paths they
/// list: each linked to the first missing changeset that names it, the file revisions whose own link the client holds
/// left out, each group in its log's order.
fn named_revisions<'s>(
  store: &'s impl Store,
  missing: &[Rev],
  held: &RevSet,
  admission: &mut Admission,
) -> Result<Named<'s>, Refusal> {
  let mut manifests: Vec<(u64, Entry<'s>)> = Vec::new();
  let mut seen_manifests = BTreeSet::new();
  let mut files: BTreeMap<&'s [u8], Vec<(u64, Entry<'s>)>> = BTreeMap::new();
  let mut seen_files = BTreeSet::new();
  for rev in missing {
    let link = node_of(store, *rev)?;
    let (manifest, touched) = read_changeset(store, *rev)?;
    if seen_manifests.insert(manifest) {
      let logged = store.manifest(&manifest).ok_or(Refusal::Store {
        what: "a manifest revision",
      })?;
      admission.charge_items::<(u64, Entry<'_>, Node)>(1)?;
      manifests.push((logged.position, entry(manifest, link, logged)));
    }
    for path in touched
      .split(|byte| *byte == NEWLINE)
      .filter(|path| !path.is_empty())
    {
      let Some(node) = store.file_node(&manifest, path) else {
        continue;
      };
      if seen_files.insert((path, node)) {
        let logged = store.file(path, &node).ok_or(Refusal::Store {
          what: "a file revision",
        })?;
        admission.charge_items::<(u64, Entry<'_>, &[u8], Node)>(1)?;
        if !client_holds(store, held, &logged) {
          files
            .entry(path)
            .or_default()
            .push((logged.position, entry(node, link, logged)));
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
    delta::encode_full_text(entry.text, &mut deltas).map_err(|_| unencodable("a full text"))?;
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
mod tests;
