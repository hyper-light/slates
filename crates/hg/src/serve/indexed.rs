//! A repository's served view over its index (A-680 part 1; §4.17.3 "Pull and clone" items 1 and 2, decisions 3 and
//! 4, "Access" last item, "The repository's index"): the [`Repository`] and [`Store`] that read serving answers from,
//! made of what one repository holds at one position of its register. The changesets are its index's changelog rows
//! (A-588 part 2); the texts and markers are their contents in the shard's content store (A-581; the index's `M` rows,
//! A-649); and its mutable state is the register's (A-642): the phase roots (`StateLog::Phases`), the bookmarks (each
//! `bookmark:` name's green and the node its head names, A-657), and whether it publishes ([`State`]).
//!
//! **The filter** is 7.2's `served`, the view `hg serve` answers every peer from (`wireprotov1server.getdispatchrepo`
//! with `server.view` at its default; `repoview.computeunserved` \[C\], read from 7.2's source on 2026-11-19). A
//! changeset is left out when it is hidden (the `visible` filter) or its phase is secret, archived or internal
//! (`served.hidden`: `phases.remotehiddenphases`). It is hidden when three things hold. It may be hidden
//! (`hideablerevs`): it is obsolete, a draft or secret changeset that some marker names as its predecessor, a prune
//! among them (`obsolete._computeobsoleteset` over `obsstore.successors`), or it is archived or internal
//! (`phases.localhiddenphases`). No bookmark pins it (`pinnedrevs`): a repository here has no working copy, local
//! tags or merge state, so its bookmarks are its only pins, every one the register holds, as 7.2 pins every bookmark
//! it holds. And no visible changeset that is not public descends from it (`computehidden`, whose `_revealancestors`
//! reveals each hidden ancestor of one through hidden ones). So a secret changeset is never served, and neither is
//! its bookmark, since every answer lists only a bookmark whose changeset the view holds; and an obsolete draft that a
//! visible draft or secret changeset descends from stays, as it does in 7.2.
//!
//! **Numbered densely.** The view numbers what it serves from 0 to `count() - 1`, oldest first, as [`Repository`]
//! asks. A changeset's parents are served whenever it is: a secret parent would make it secret, a public one has only
//! public parents, which are never hidden, and one that is neither reveals a hidden parent. So each parent still comes
//! before its child. 7.2 keeps the changelog's numbers in a filtered view, and the wire carries nodes, never numbers,
//! so the renumbering shows nowhere on the wire.
//!
//! **Read from the first changeset that is not public.** Every changeset below the lowest phase root is public, so it
//! is served and numbered as the changelog numbers it (7.2's `computeimpactable` starts at the same place for the same
//! reason). The filter walks only from there: forward once to take each changeset's phase as the highest of its
//! parents' and of any root at itself (`phases.py`), and back once to reveal what a visible descendant keeps. The view
//! keeps the changesets it serves from there, with their phases, and those it leaves out, with why, all charged to the
//! request's room before they are made. A changeset's node, parents, branch and text are read from its row when
//! asked. So making a view costs the changesets that are not public, never the whole history, and an answer that
//! reads a few nodes (`known`, `listkeys`) reads only those.
//!
//! **Draft roots** (A-691). `listkeys phases` lists the register's draft roots whose phase is still draft, read past
//! the filter, as 7.2's `listphases` reads them from the unfiltered changelog (`phases.py` 1049-1058), so a hidden
//! draft root is listed though the view does not serve it. They are taken where the forward walk has every changeset's
//! phase from the lowest root on: a draft root a root of a higher phase reaches is not draft any more and is left out,
//! and one whose node the changelog lacks was dropped with the others.
//!
//! **Texts lent** ([`Store`]). A changeset's or manifest's text, or a file revision's bytes, is lent in place where the
//! store holds it as one window (an object no longer than the store's window is that window, `slates_vcs::content`),
//! and copied otherwise, its length charged to the room first. A file revision with a metadata block is put back
//! together as 7.2 stores it (the block between its markers, then the bytes), so it is always copied. The markers are
//! lent when the view is made, each read whole and checked. [`Indexed::lends`] counts which way each text went, so a
//! test can tell the fast path ran.
//!
//! **What it refuses, and drops.** A phase root whose node the changelog does not hold is dropped and counted, as 7.2's
//! `_readroots` drops one. A changelog row the walk cannot read, or one naming a parent at or after itself, refuses the
//! view, as does a marker it cannot read (7.2 aborts on an obsstore it cannot read). Where an answer asks a node or
//! parents of a row the index cannot read, the view answers as the graph's fault ([`Refusal::Graph`]).

use core::cell::Cell;
use std::borrow::Cow;

use slates_vcs::{ContentId, ContentStore};

use super::getbundle::{Linked, Logged, Store};
use super::{Refusal, Repository, Rev};
use crate::filerev::MARKER;
use crate::index::{ChangesetRow, HgIndex, Log, NONE};
use crate::obsmarkers::{self, Marker};
use crate::phases::{Phase, PhaseHead};
use crate::read::to_u64;
use crate::{Admission, NULL_NODE, Node, PastAdmission};

/// A repository's mutable state at one position of its register (A-642), as the served view reads it: what the
/// server hands the view from the register's logs.
#[derive(Clone, Copy, Debug)]
pub struct State<'a> {
  /// The phase roots (`StateLog::Phases`): each a phase and the node of the changeset it roots, in any order. A public
  /// root changes nothing; one whose node the changelog does not hold is dropped.
  pub roots: &'a [PhaseHead],
  /// The bookmarks: each `bookmark:` name's label and the node its green's head names, in any order.
  pub bookmarks: &'a [(Vec<u8>, Node)],
  /// Whether the repository publishes (7.2's `phases.publish`).
  pub publishing: bool,
}

/// Why a changeset the changelog holds is not served: one of the two filters 7.2's `served` joins.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LeftOut {
  /// Hidden (`visible`): obsolete, archived or internal, pinned by no bookmark, and no visible changeset that is not
  /// public descends from it.
  Hidden,
  /// Not hidden, and secret, archived or internal (`served.hidden`).
  Secret,
}

/// What the filter found, counted: the view's non-vacuity counters.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Census {
  /// Changesets left out as hidden.
  pub hidden: u32,
  /// Changesets left out as secret, archived or internal, and not hidden.
  pub secret: u32,
  /// Changesets that may be hidden, kept visible by a bookmark.
  pub pinned: u32,
  /// Changesets that may be hidden and no bookmark pins, kept visible by a visible descendant that is not public.
  pub revealed: u32,
  /// Phase roots dropped: their nodes are not the changelog's.
  pub dropped_roots: u32,
}

/// How the view's texts were lent.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Lends {
  /// Texts lent in place: the store held each as one window.
  pub in_place: u64,
  /// Texts copied: longer than a window, or a file revision's block and bytes put back together.
  pub copied: u64,
}

/// A served changeset from the first that is not public on: its changelog revision and its phase.
#[derive(Clone, Copy, Debug)]
struct Kept {
  /// Its changelog revision.
  rev: u32,
  /// Its phase: public or draft.
  phase: Phase,
}

/// A changeset from the first that is not public on, as the walks found it.
#[derive(Clone, Copy, Debug)]
struct Walked {
  /// Its parents' changelog revisions, [`NONE`] for none.
  parents: [u32; 2],
  /// Its phase.
  phase: Phase,
  /// Whether it may be hidden (`hideablerevs`) and no bookmark pins it.
  candidate: bool,
  /// Whether it may be hidden and a bookmark pins it.
  pinned: bool,
  /// Whether the walk back reached it from a visible descendant that is not public.
  reached: bool,
}

impl Walked {
  /// Why it is left out, if it is: hidden first, as 7.2's listing names it.
  fn left_out(&self) -> Option<LeftOut> {
    if self.candidate && !self.reached {
      return Some(LeftOut::Hidden);
    }
    (self.phase > Phase::Draft).then_some(LeftOut::Secret)
  }
}

/// A repository's served view over its index (module docs).
#[derive(Debug)]
pub struct Indexed<'a> {
  /// The index.
  index: &'a HgIndex,
  /// The store its rows' contents live in.
  store: &'a ContentStore,
  /// The register's state the view was made at.
  state: State<'a>,
  /// The changelog's first revision the filter may leave out: every revision below it is public, served, and
  /// numbered as the changelog numbers it.
  first: u32,
  /// The served changesets from `first` on, oldest first: served revision `first + i` is `tail[i]`.
  tail: Vec<Kept>,
  /// The changesets from `first` on that are left out, oldest first, each with why.
  left_out: Vec<(u32, LeftOut)>,
  /// The register's draft roots whose phase is still draft, by node, sorted, hidden ones among them (module docs).
  draft_roots: Vec<Node>,
  /// Each obsolescence marker, alone in its format, lent.
  markers: Vec<Cow<'a, [u8]>>,
  /// What the filter found.
  census: Census,
  /// How the texts were lent.
  lends: Cell<Lends>,
}

impl<'a> Indexed<'a> {
  /// The served view of `index`, its rows' contents in `store`, at the register's `state` (module docs): every
  /// changeset the changelog holds that 7.2's `served` filter keeps, numbered densely from 0, oldest first. The
  /// filter's state and the view's are charged to `room` before they are made. Refused past `room`; for a marker the
  /// store cannot read; and for a changelog row from the first changeset that is not public on that the index cannot
  /// read, or that names a parent at or after itself (`Graph` then names the changelog's revision).
  pub fn new(
    index: &'a HgIndex,
    store: &'a ContentStore,
    state: State<'a>,
    room: &mut Admission,
  ) -> Result<Indexed<'a>, Refusal> {
    let lends = Cell::new(Lends::default());
    let markers = lend_markers(index, store, &lends, room)?;
    let predecessors = predecessors(&markers, room)?;
    let pins = pins(state.bookmarks, room)?;
    let len = index.len(store, Log::Changelog);
    let (roots, dropped_roots) = rooted(index, store, state.roots, room)?;
    let first = roots.first().map_or(len, |(rev, _)| (*rev).min(len));
    let marked = Marked {
      predecessors: &predecessors,
      pins: &pins,
    };
    let walked = walk(index, store, (first, len), &roots, marked, room)?;
    let draft_roots = still_draft(index, store, state.roots, (&walked, first), room)?;
    let (tail, left_out, mut census) = filter(walked, (first, len), room)?;
    census.dropped_roots = dropped_roots;
    Ok(Indexed {
      index,
      store,
      state,
      first,
      tail,
      left_out,
      draft_roots,
      markers,
      census,
      lends,
    })
  }

  /// Why the changeset `node` is not served; `None` for one it serves, or one the changelog does not hold.
  pub fn left_out(&self, node: &Node) -> Option<LeftOut> {
    let rev = self.index.rev(self.store, Log::Changelog, node)?;
    let at = self
      .left_out
      .binary_search_by_key(&rev, |(rev, _)| *rev)
      .ok()?;
    self.left_out.get(at).map(|(_, why)| *why)
  }

  /// What the filter found.
  pub fn census(&self) -> Census {
    self.census
  }

  /// How the view's texts were lent so far.
  pub fn lends(&self) -> Lends {
    self.lends.get()
  }

  /// The changelog revision of served revision `rev`.
  fn changelog_rev(&self, rev: Rev) -> Option<u32> {
    if rev < self.first {
      return Some(rev);
    }
    let offset = usize::try_from(rev.checked_sub(self.first)?).ok()?;
    self.tail.get(offset).map(|kept| kept.rev)
  }

  /// The served revision of changelog revision `rev`, if the view serves it.
  fn served_rev(&self, rev: u32) -> Option<Rev> {
    if rev < self.first {
      return Some(rev);
    }
    let offset = self.tail.binary_search_by_key(&rev, |kept| kept.rev).ok()?;
    self.first.checked_add(u32::try_from(offset).ok()?)
  }

  /// The changelog row of served revision `rev`.
  fn row(&self, rev: Rev) -> Option<ChangesetRow<'a>> {
    self.index.changeset(self.store, self.changelog_rev(rev)?)
  }

  /// The node of revision `rev` of `log`: the null node for none.
  fn log_node(&self, log: Log<'_>, rev: u32) -> Option<Node> {
    if rev == NONE {
      return Some(NULL_NODE);
    }
    self
      .index
      .revision(self.store, log, rev)
      .map(|row| row.node)
  }

  /// The revision of `node` in the manifest log or a file log, with its text lent; `None` where the log or the store
  /// does not hold it whole.
  fn logged(
    &self,
    log: Log<'_>,
    node: &Node,
    room: &mut Admission,
  ) -> Result<Option<Logged<'a>>, PastAdmission> {
    let Some(rev) = self.index.rev(self.store, log, node) else {
      return Ok(None);
    };
    let Some(row) = self
      .index
      .revision(self.store, log, rev)
      .filter(|row| row.node == *node)
    else {
      return Ok(None);
    };
    let link = self
      .index
      .changeset(self.store, row.link)
      .map(|changeset| changeset.node);
    let (Some(p1), Some(p2), Some(link)) =
      (self.log_node(log, row.p1), self.log_node(log, row.p2), link)
    else {
      return Ok(None);
    };
    let text = match row.block {
      // A manifest kept as a delta (A-729): its text rebuilt from its chain, charged to the room as a copy is.
      None if row.base != NONE => {
        room.charge(row.len)?;
        let rebuilt = self.index.revision_text(self.store, log, rev);
        if rebuilt.is_some() {
          count_lend(&self.lends, false);
        }
        rebuilt.map(Cow::Owned)
      }
      None => lend(self.store, &row.bytes, row.len, &self.lends, room)?,
      Some(block) => framed(self.store, [&block, &row.bytes], row.len, &self.lends, room)?,
    };
    Ok(text.map(|text| Logged {
      position: u64::from(rev),
      parents: [p1, p2],
      link,
      flags: row.flags,
      text,
    }))
  }
}

impl Repository for Indexed<'_> {
  fn count(&self) -> Rev {
    let tail = u32::try_from(self.tail.len()).unwrap_or(u32::MAX);
    self.first.saturating_add(tail)
  }

  fn node(&self, rev: Rev) -> Option<Node> {
    self.row(rev).map(|row| row.node)
  }

  fn rev(&self, node: &Node) -> Option<Rev> {
    self.served_rev(self.index.rev(self.store, Log::Changelog, node)?)
  }

  /// A row the index cannot read, or a parent the view does not serve (neither happens to an index the view was made
  /// over), is answered as a parent at the revision itself, which every walk refuses as the graph's fault.
  fn parents(&self, rev: Rev) -> [Option<Rev>; 2] {
    let Some(row) = self.row(rev) else {
      return [Some(rev), None];
    };
    [row.p1, row.p2].map(|parent| (parent != NONE).then(|| self.served_rev(parent).unwrap_or(rev)))
  }

  fn branch(&self, rev: Rev) -> &[u8] {
    self.row(rev).map(|row| row.branch).unwrap_or_default()
  }

  fn closes(&self, rev: Rev) -> bool {
    self.row(rev).is_some_and(|row| row.closed)
  }

  fn phase(&self, rev: Rev) -> Phase {
    rev
      .checked_sub(self.first)
      .and_then(|offset| usize::try_from(offset).ok())
      .and_then(|offset| self.tail.get(offset))
      .map_or(Phase::Public, |kept| kept.phase)
  }

  fn publishing(&self) -> bool {
    self.state.publishing
  }

  fn bookmarks(&self) -> impl Iterator<Item = (&[u8], Node)> {
    self
      .state
      .bookmarks
      .iter()
      .map(|(name, node)| (name.as_slice(), *node))
  }

  fn draft_roots(&self) -> impl Iterator<Item = Node> {
    self.draft_roots.iter().copied()
  }

  fn tags_file(&self, rev: Rev, admission: &mut Admission) -> Result<Option<Node>, Refusal> {
    let row = self.row(rev).ok_or(Refusal::Graph { rev })?;
    if row.manifest == NULL_NODE {
      return Ok(None);
    }
    let manifest = self.text_of(Log::Manifest, &row.manifest, admission)?;
    Ok(crate::manifest::find(&manifest, TAGS_PATH).map(|entry| entry.node))
  }

  fn tags_text(&self, node: &Node, admission: &mut Admission) -> Result<Vec<u8>, Refusal> {
    let text = self.text_of(Log::File(TAGS_PATH), node, admission)?;
    let split = crate::filerev::split(&text).map_err(|_| Refusal::Store { what: "tags" })?;
    Ok(split.bytes.to_vec())
  }
}

/// Format: the path of the file Mercurial keeps its global tags in.
const TAGS_PATH: &[u8] = b".hgtags";

impl Indexed<'_> {
  /// The whole text of `node` in `log` (the manifest log or `.hgtags`'s), charged to `admission` by its length before it
  /// is read; refused as the store's fault where the index lacks it.
  fn text_of(
    &self,
    log: Log<'_>,
    node: &Node,
    admission: &mut Admission,
  ) -> Result<Vec<u8>, Refusal> {
    let missing = Refusal::Store { what: "tags" };
    let rev = self.index.rev(self.store, log, node).ok_or(missing)?;
    let row = self.index.revision(self.store, log, rev).ok_or(missing)?;
    if row.node != *node {
      return Err(missing);
    }
    admission.charge(row.len)?;
    self
      .index
      .revision_text(self.store, log, rev)
      .ok_or(missing)
  }
}

impl Store for Indexed<'_> {
  fn changeset_text(
    &self,
    rev: Rev,
    room: &mut Admission,
  ) -> Result<Option<Cow<'_, [u8]>>, PastAdmission> {
    let Some(row) = self.row(rev) else {
      return Ok(None);
    };
    lend(self.store, &row.text, row.len, &self.lends, room)
  }

  fn manifest(
    &self,
    node: &Node,
    room: &mut Admission,
  ) -> Result<Option<Logged<'_>>, PastAdmission> {
    self.logged(Log::Manifest, node, room)
  }

  fn file(
    &self,
    path: &[u8],
    node: &Node,
    room: &mut Admission,
  ) -> Result<Option<Logged<'_>>, PastAdmission> {
    self.logged(Log::File(path), node, room)
  }

  fn markers(&self) -> impl Iterator<Item = Marker<'_>> {
    self.markers.iter().flat_map(|payload| {
      obsmarkers::decode(payload)
        .ok()
        .into_iter()
        .flat_map(|markers| markers.iter())
    })
  }

  fn manifest_linked(&self, position: u64) -> Option<Linked> {
    let rev = u32::try_from(position).ok()?;
    let row = self.index.revision(self.store, Log::Manifest, rev)?;
    Some(Linked {
      node: row.node,
      link: self.served_rev(row.link),
    })
  }

  fn file_linked_from(&self, (path, position): (&[u8], u64)) -> Option<(Vec<u8>, u64, Linked)> {
    let rev = u32::try_from(position).unwrap_or(u32::MAX);
    let (found, number, row) = self.index.file_revision_from(self.store, (path, rev))?;
    let linked = Linked {
      node: row.node,
      link: self.served_rev(row.link),
    };
    Some((found, u64::from(number), linked))
  }

  fn holds_manifest(&self, node: &Node) -> bool {
    self.index.rev(self.store, Log::Manifest, node).is_some()
  }

  fn serves_every_changeset(&self) -> bool {
    self.count() == self.index.len(self.store, Log::Changelog)
  }

  /// A stream is served unless a secret root the changelog holds makes a changeset secret (7.2's
  /// `allowservergeneration` over `phases.hassecret`); hidden changesets are streamed, as 7.2 streams them.
  fn streams(&self) -> bool {
    !self.state.roots.iter().any(|root| {
      root.phase == Phase::Secret
        && self
          .index
          .rev(self.store, Log::Changelog, &root.node)
          .is_some()
    })
  }

  fn revlogs(&self) -> Option<(&HgIndex, &ContentStore)> {
    self.streams().then_some((self.index, self.store))
  }
}

/// The refusal of a marker the store cannot read.
fn unreadable_marker() -> Refusal {
  Refusal::Store {
    what: "an obsolescence marker",
  }
}

/// Every marker the index holds, in its order, each lent whole and checked (module docs).
fn lend_markers<'a>(
  index: &HgIndex,
  store: &'a ContentStore,
  lends: &Cell<Lends>,
  room: &mut Admission,
) -> Result<Vec<Cow<'a, [u8]>>, Refusal> {
  let count = index.markers_len(store);
  room.charge_items::<Cow<'_, [u8]>>(usize::try_from(count).unwrap_or(usize::MAX))?;
  let mut markers = Vec::with_capacity(usize::try_from(count).unwrap_or_default());
  for number in 0..count {
    let content = index.marker(store, number).ok_or_else(unreadable_marker)?;
    let len = store.windows(&content).fold(0_u64, |total, window| {
      total.saturating_add(to_u64(window.len()))
    });
    let lent = lend(store, &content, len, lends, room)?.ok_or_else(unreadable_marker)?;
    obsmarkers::decode(&lent).map_err(|_| unreadable_marker())?;
    markers.push(lent);
  }
  Ok(markers)
}

/// The nodes the markers name as predecessors, sorted: a draft or secret changeset whose node is among them is
/// obsolete (`obsstore.successors` is keyed by every marker's predecessor, a prune's too). A predecessor of another
/// length (a SHA-256 marker's) names no changeset here.
fn predecessors(markers: &[Cow<'_, [u8]>], room: &mut Admission) -> Result<Vec<Node>, Refusal> {
  let all = || {
    markers.iter().flat_map(|payload| {
      obsmarkers::decode(payload)
        .ok()
        .into_iter()
        .flat_map(|markers| markers.iter())
    })
  };
  room.charge_items::<Node>(all().count())?;
  let mut nodes: Vec<Node> = all()
    .filter_map(|marker| Node::try_from(marker.predecessor).ok())
    .collect();
  nodes.sort_unstable();
  nodes.dedup();
  Ok(nodes)
}

/// The nodes the bookmarks name, sorted: each pins its changeset visible (`pinnedrevs`).
fn pins(bookmarks: &[(Vec<u8>, Node)], room: &mut Admission) -> Result<Vec<Node>, Refusal> {
  room.charge_items::<Node>(bookmarks.len())?;
  let mut nodes: Vec<Node> = bookmarks.iter().map(|(_, node)| *node).collect();
  nodes.sort_unstable();
  nodes.dedup();
  Ok(nodes)
}

/// The phase roots above public whose nodes the changelog holds, each its changelog revision and phase, sorted; and
/// how many were dropped, their nodes not the changelog's (7.2's `_readroots` drops them).
fn rooted(
  index: &HgIndex,
  store: &ContentStore,
  roots: &[PhaseHead],
  room: &mut Admission,
) -> Result<(Vec<(u32, Phase)>, u32), Refusal> {
  room.charge_items::<(u32, Phase)>(roots.len())?;
  let mut held = Vec::with_capacity(roots.len());
  let mut dropped = 0_u32;
  for root in roots.iter().filter(|root| root.phase != Phase::Public) {
    match index.rev(store, Log::Changelog, &root.node) {
      Some(rev) => held.push((rev, root.phase)),
      None => dropped = dropped.saturating_add(1),
    }
  }
  held.sort_unstable();
  Ok((held, dropped))
}

/// The register's draft roots whose phase the forward walk found still draft, by node, sorted, each once (module docs;
/// 7.2's `listphases`, `phases.py` 1049-1058: each root of `_phaseroots[draft]`, kept where `phase(root) <= draft`):
/// read from every changeset the walk saw, `walked` from `first` on, so a hidden one is among them. One whose node the
/// changelog lacks is not (`rooted` dropped and counted it). Charged to `room` before the list is made.
fn still_draft(
  index: &HgIndex,
  store: &ContentStore,
  roots: &[PhaseHead],
  (walked, first): (&[Walked], u32),
  room: &mut Admission,
) -> Result<Vec<Node>, Refusal> {
  let kept = |root: &&PhaseHead| {
    root.phase == Phase::Draft
      && index
        .rev(store, Log::Changelog, &root.node)
        .and_then(|rev| walked.get(offset_of(rev, first)?))
        .is_some_and(|this| this.phase == Phase::Draft)
  };
  room.charge_items::<Node>(roots.iter().filter(kept).count())?;
  let mut nodes: Vec<Node> = roots.iter().filter(kept).map(|root| root.node).collect();
  nodes.sort_unstable();
  nodes.dedup();
  Ok(nodes)
}

/// What marks a changeset for the filter: the markers' predecessors and the bookmarks' nodes, each sorted.
#[derive(Clone, Copy)]
struct Marked<'m> {
  /// The nodes markers name as predecessors.
  predecessors: &'m [Node],
  /// The nodes bookmarks name.
  pins: &'m [Node],
}

impl Marked<'_> {
  /// The changeset `node` of `phase`, with `parents`, as the filter marks it before the walk back: whether it may be
  /// hidden (obsolete, a draft or secret one some marker names as its predecessor; or archived or internal), and
  /// whether a bookmark pins it.
  fn walked(&self, node: &Node, parents: [u32; 2], phase: Phase) -> Walked {
    let obsolete = matches!(phase, Phase::Draft | Phase::Secret)
      && self.predecessors.binary_search(node).is_ok();
    let hideable = obsolete || matches!(phase, Phase::Archived | Phase::Internal);
    let pinned = hideable && self.pins.binary_search(node).is_ok();
    Walked {
      parents,
      phase,
      candidate: hideable && !pinned,
      pinned,
      reached: false,
    }
  }
}

/// The offset from `first` of changelog revision `rev`, where the walks hold it: `None` for none, and below `first`.
fn offset_of(rev: u32, first: u32) -> Option<usize> {
  if rev == NONE {
    return None;
  }
  usize::try_from(rev.checked_sub(first)?).ok()
}

/// The highest phase of `parents` as the forward walk found them: public for none, and below `first`.
fn inherited(walked: &[Walked], first: u32, parents: [u32; 2]) -> Phase {
  parents
    .into_iter()
    .filter_map(|parent| walked.get(offset_of(parent, first)?))
    .map(|parent| parent.phase)
    .max()
    .unwrap_or(Phase::Public)
}

/// A changelog row's parents, refused as the graph's fault where one is not before the row's revision `rev`.
fn parents_before(row: &ChangesetRow<'_>, rev: u32) -> Result<[u32; 2], Refusal> {
  let parents = [row.p1, row.p2];
  if parents
    .iter()
    .any(|parent| *parent != NONE && *parent >= rev)
  {
    return Err(Refusal::Graph { rev });
  }
  Ok(parents)
}

/// The forward walk from `first` to `len` (module docs): each changeset's phase the highest of its parents' and of the
/// roots at itself, and whether it may be hidden; refused for a row the index cannot read, or a parent not before its
/// child.
fn walk(
  index: &HgIndex,
  store: &ContentStore,
  (first, len): (u32, u32),
  roots: &[(u32, Phase)],
  marked: Marked<'_>,
  room: &mut Admission,
) -> Result<Vec<Walked>, Refusal> {
  let span = usize::try_from(len.saturating_sub(first)).unwrap_or(usize::MAX);
  room.charge_items::<Walked>(span)?;
  let mut walked: Vec<Walked> = Vec::with_capacity(span);
  let mut roots = roots.iter().peekable();
  for rev in first..len {
    let row = index.changeset(store, rev).ok_or(Refusal::Store {
      what: "a changelog row",
    })?;
    let parents = parents_before(&row, rev)?;
    let mut phase = inherited(&walked, first, parents);
    while let Some((_, rooted)) = roots.next_if(|(root, _)| *root <= rev) {
      phase = phase.max(*rooted);
    }
    walked.push(marked.walked(&row.node, parents, phase));
  }
  Ok(walked)
}

/// The walk back (module docs): every parent of a visible changeset that is not public is reached, newest first, so a
/// hidden changeset reached is revealed and reaches its own parents in turn, as 7.2's `_revealancestors` reveals them.
fn reveal(walked: &mut [Walked], first: u32) {
  for offset in (0..walked.len()).rev() {
    let Some(this) = walked.get(offset).copied() else {
      continue;
    };
    if this.phase == Phase::Public || (this.candidate && !this.reached) {
      continue;
    }
    for parent in this.parents {
      if let Some(slot) = offset_of(parent, first).and_then(|at| walked.get_mut(at)) {
        slot.reached = true;
      }
    }
  }
}

/// Counts one changeset into `census`: whether a bookmark kept it visible or a descendant revealed it, and why it is
/// left out, if it is.
fn count_into(census: &mut Census, this: &Walked) {
  census.pinned = census.pinned.saturating_add(u32::from(this.pinned));
  let revealed = this.candidate && this.reached;
  census.revealed = census.revealed.saturating_add(u32::from(revealed));
  let count = match this.left_out() {
    None => return,
    Some(LeftOut::Hidden) => &mut census.hidden,
    Some(LeftOut::Secret) => &mut census.secret,
  };
  *count = count.saturating_add(1);
}

/// The view's tail, the changesets left out (each with its number), and the census.
type Filtered = (Vec<Kept>, Vec<(u32, LeftOut)>, Census);

/// The view's tail, the changesets left out, and the census, from the walked changesets once the walk back has
/// revealed what it reaches: what is neither hidden nor secret is served. Both lists are charged before they are made.
fn filter(
  mut walked: Vec<Walked>,
  (first, len): (u32, u32),
  room: &mut Admission,
) -> Result<Filtered, Refusal> {
  reveal(&mut walked, first);
  let left = walked
    .iter()
    .filter(|this| this.left_out().is_some())
    .count();
  let kept = walked.len().saturating_sub(left);
  room.charge_items::<Kept>(kept)?;
  room.charge_items::<(u32, LeftOut)>(left)?;
  let mut tail = Vec::with_capacity(kept);
  let mut left_out = Vec::with_capacity(left);
  let mut census = Census::default();
  for (rev, this) in (first..len).zip(&walked) {
    count_into(&mut census, this);
    match this.left_out() {
      None => tail.push(Kept {
        rev,
        phase: this.phase,
      }),
      Some(why) => left_out.push((rev, why)),
    }
  }
  Ok((tail, left_out, census))
}

/// Counts one text lent, in place or copied.
fn count_lend(lends: &Cell<Lends>, in_place: bool) {
  let mut now = lends.get();
  if in_place {
    now.in_place = now.in_place.saturating_add(1);
  } else {
    now.copied = now.copied.saturating_add(1);
  }
  lends.set(now);
}

/// Appends `part` to `bytes` unless it would take them past `capacity`; whether it did.
fn append_within(bytes: &mut Vec<u8>, part: &[u8], capacity: usize) -> bool {
  let fits = bytes
    .len()
    .checked_add(part.len())
    .is_some_and(|total| total <= capacity);
  if fits {
    bytes.extend_from_slice(part);
  }
  fits
}

/// The parts of a text, in order, copied into one buffer of exactly `len` bytes, `len` charged to `room` first; `None`
/// where they do not come to exactly `len`.
fn copied<P: AsRef<[u8]>>(
  parts: impl Iterator<Item = P>,
  len: u64,
  room: &mut Admission,
) -> Result<Option<Vec<u8>>, PastAdmission> {
  room.charge(len)?;
  let Ok(capacity) = usize::try_from(len) else {
    return Ok(None);
  };
  let mut bytes = Vec::with_capacity(capacity);
  for part in parts {
    if !append_within(&mut bytes, part.as_ref(), capacity) {
      return Ok(None);
    }
  }
  Ok((bytes.len() == capacity).then_some(bytes))
}

/// The object `root`, `len` bytes long: lent in place where the store holds it as one window, else copied with `len`
/// charged to `room` first; `None` where the store does not hold it whole at that length.
fn lend<'a>(
  store: &'a ContentStore,
  root: &ContentId,
  len: u64,
  lends: &Cell<Lends>,
  room: &mut Admission,
) -> Result<Option<Cow<'a, [u8]>>, PastAdmission> {
  let mut windows = store.windows(root);
  let Some(first) = windows.next() else {
    return Ok(None);
  };
  if windows.next().is_none() {
    if to_u64(first.len()) != len {
      return Ok(None);
    }
    count_lend(lends, matches!(first, Cow::Borrowed(_)));
    return Ok(Some(first));
  }
  let bytes = copied(store.windows(root), len, room)?;
  if bytes.is_some() {
    count_lend(lends, false);
  }
  Ok(bytes.map(Cow::Owned))
}

/// A file revision's text as 7.2 stores it, put back together: its metadata block `block` between its markers, then
/// its bytes `bytes`, `len` in all, copied with `len` charged to `room` first; `None` where the store does not hold
/// both whole at that length.
fn framed<'a>(
  store: &'a ContentStore,
  [block, bytes]: [&ContentId; 2],
  len: u64,
  lends: &Cell<Lends>,
  room: &mut Admission,
) -> Result<Option<Cow<'a, [u8]>>, PastAdmission> {
  let parts = core::iter::once(Cow::Borrowed(MARKER))
    .chain(store.windows(block))
    .chain(core::iter::once(Cow::Borrowed(MARKER)))
    .chain(store.windows(bytes));
  let text = copied(parts, len, room)?;
  if text.is_some() {
    count_lend(lends, false);
  }
  Ok(text.map(Cow::Owned))
}

#[cfg(test)]
#[path = "indexed_tests.rs"]
mod tests;
