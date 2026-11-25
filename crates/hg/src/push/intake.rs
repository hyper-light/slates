//! A push's changegroup taken into a fork of its repository's index a revision at a time (A-720; §4.17.3 "Push";
//! §4.17.1's rule for a push: its working set bounded whatever its size, its content stored once as it arrives): the
//! sliced intake a server advances within its shard's step budget, in place of verifying the whole changegroup first
//! ([`super::verify::verify`], every arrived text held at once) and keeping it after ([`super::commit::fork_with`]).
//!
//! **A step** takes one unit: one revision, in 7.2's order (the changelog's group, the manifest log's, then each file
//! log's), or one of the checks after them. A revision the log holds, or the push brought earlier, is passed over, as
//! 7.2's `addgroup` passes it over. Any other is checked as verification checks it (its flags, its parents known), its
//! text made from its delta against its base, its node held to its text, its text read as its log's reader reads it (a
//! changeset's manifest, a manifest's entries, a file revision's metadata block; a manifest's and a file revision's link
//! a changeset the fork holds), and then kept: its text put into the content store once and its row appended to the
//! fork ([`super::commit::keep_one`]). A file revision's text is made a window a step and never whole ([`patched`]: its
//! base read from the fork's rows a window at a time, each window made hashed, read and put as it is made; goal 93,
//! C1 (2)); so is a manifest's ([`manifesting`]: its base read over its chain, its entries read as it streams, its row kept
//! by [`super::manifest_row`]'s rule, the client's delta or its diff against the manifest the fork holds last, main's
//! ruling (B')); a changeset's is rebuilt whole (the step [`rebuilt`]: the text before it in its group where its delta
//! names that one, as 7.2's revision cache serves it, else the base's text read back from the fork). The fork holds
//! the repository's revisions and the push's, so a later revision's parents and base are found there. After the logs,
//! each new changeset's manifest is held to be present, then each new manifest's changed files, a manifest a step.
//!
//! **Bounded.** What a file revision's step holds of its text is two windows, the one it makes and the one of its base
//! it hashes, whatever the file; a manifest's, those and its diff's (a window and a line of each text and of its run),
//! and its base's chain's deltas (`Intaken::most_text_held`, `Intaken::most_chain_held`). A changeset's step holds its
//! delta, the base's text and the rebuilt text, charged to a copy of the push's admission, since it is let go when the
//! step ends; only the text before it is kept to the next step (owed: changesets a window at a time). What
//! the intake keeps across steps is per revision and fixed in size (a node, a changeset's manifest, a manifest's first
//! parent), charged to the admission itself. The texts are kept in the content store, charged to its budget as every
//! store's are, never held by the intake. So its working set is one revision's texts, whatever the push's size. The
//! changegroup's framing is decoded once at the start, a record of fixed size per revision (`Changegroup::decode`).
//!
//! **What (A-720's later part) changes.** The rebuild is one step on purpose: storage that keeps a revision as it
//! arrived (a delta against its base) removes it from the keeping, and leaves it only to hold the node to its text.
//!
//! **Refused**, the caller gives the fork back ([`Intake::abandon`]): the repository's index and the content store are
//! as they were. The refusals are verification's ([`Unverified`]) and the keeping's ([`Unindexed`]).

use core::ops::Range;
use std::borrow::Cow;
use std::collections::{BTreeSet, VecDeque};

use slates_vcs::{ContentId, ContentStore, IndexRefused, Merge, MergeStep};

use super::classify::ReadChangeset;
use super::commit::{Unindexed, keep_one};
use super::held::IndexHeld;
use super::verify::{Arrived, Held, LogOf, Unverified, audit};
use crate::changegroup::{Revision, Version};
use crate::filerev::FileContents;
use crate::framing::{Collect, Framed, Framing, Halt, Source, Wanted};
use crate::index::{HgIndex, Log, NONE};
use crate::read::to_u64;
use crate::{Admission, NULL_NODE, Node, changeset, delta, filerev, manifest, node};

/// Why an intake was refused; its caller gives its fork back.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Refused {
  /// Verification's refusal, as [`super::verify::verify`] refuses it.
  Unverified(Unverified),
  /// The keeping's refusal, as [`super::commit::fork_with`] refuses it.
  Unindexed(Unindexed),
}

impl Refused {
  /// Where it is the intake past its admission, by a charge, by its changegroup's framing, by a delta's text or by a
  /// keeping's own room: what was asked past what was left. Such a refusal leaves the intake as it was before the begin
  /// or the step it refused (the module doc's "Bounded"), so its driver takes that again once its room has grown (main
  /// 01a20182's ruling of 2026-11-22 03:03: an intake past its admission waits, never refused).
  pub fn past_admission(&self) -> Option<crate::PastAdmission> {
    match self {
      Refused::Unverified(
        Unverified::PastAdmission(past)
        | Unverified::Changegroup(crate::changegroup::Refusal::PastAdmission(past))
        | Unverified::Delta {
          fault: crate::delta::Refusal::PastAdmission(past),
          ..
        },
      )
      | Refused::Unindexed(Unindexed::PastAdmission(past)) => Some(*past),
      _ => None,
    }
  }
}

impl From<Unverified> for Refused {
  fn from(refused: Unverified) -> Self {
    Refused::Unverified(refused)
  }
}

impl From<Unindexed> for Refused {
  fn from(refused: Unindexed) -> Self {
    Refused::Unindexed(refused)
  }
}

impl From<crate::PastAdmission> for Refused {
  fn from(past: crate::PastAdmission) -> Self {
    Refused::Unverified(Unverified::PastAdmission(past))
  }
}

/// What an intake brought, without its texts.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Intaken {
  /// The new changesets, in the order they arrived.
  pub changesets: Vec<Node>,
  /// The changesets the repository held already, in the order they arrived: what 7.2's `changegroup.apply` calls
  /// `duprevs`.
  pub held_changesets: Vec<Node>,
  /// The revisions passed over: held already, or brought earlier in the same log.
  pub passed_over: u64,
  /// The revisions kept.
  pub kept: u64,
  /// The bytes of the texts kept.
  pub bytes: u64,
  /// Each log the push kept revisions in, in the order kept, and the number of its first: its kept revisions are the
  /// fork's from that number on, which its record is written from.
  pub logs: Vec<(LogOf, u32)>,
  /// Each revision kept, as the changeset log's `Texts` section names it (A-767 C4; w-perf's format, agreed): written as
  /// it is kept, since the intake has rebuilt and checked its text then and naming it later would rebuild every chain.
  pub texts: Vec<KeptText>,
  /// The whole texts the intake rebuilt from the fork's chains (main's ruling of 2026-11-21 05:12 (c): a push's work
  /// and memory are to grow with what it brings, not with the history under it): a delta base it did not hold as the
  /// text kept last. A new manifest's files are checked from the lines its delta wrote, rebuilding nothing.
  pub rebuilt: u64,
  /// The most delta bytes of a manifest base's chain loaded while it was read (goal 93, C1 (2), main's ruling (B)): at
  /// most the base's own length, A-729's chain bound.
  pub most_chain_held: u64,
  /// The bytes of the manifests kept whole, charged to the push's admission until it lands: those whose delta's base is
  /// not the row before them (a merge's), or whose chain passed its bound (main's ruling (B), condition 3).
  pub manifests_kept_whole_bytes: u64,
  /// The file delta bases asked of the node's changeset store between steps: a revision landed before the push whose
  /// windows were let go, brought back to be read.
  pub base_store_reads: u64,
  /// The most text bytes one step hashed and kept (A-767 C1, the push spin): a revision longer than the content
  /// store's window is taken over as many steps as it has windows ([`Pending`]), so no step's work grows with a file.
  pub longest_step_bytes: u64,
  /// The revisions taken over more than one step, a window a step ([`Pending`]).
  pub spread: u64,
  /// The fork's runs merged by the size-tiered rule as the push sealed them ([`Taker::merge_step`]).
  pub tiered_merges: u64,
  /// The most runs only the fork held where no merge was due: what a lookup of a new row walked past its parent's runs,
  /// held within [`slates_vcs::Index::tiered_due`]'s logarithm of its own tables' bytes.
  pub most_own_runs: u64,
  /// The most runs only the fork held after any step, a merge due or under way: at most one past
  /// [`Intaken::most_own_runs`]'s bound (the run a seal wrote, until the merge it made due ends; Derived in
  /// [`slates_vcs::Index::tiered_due`]).
  pub most_own_runs_in_flight: u64,
  /// The changelog's and the manifests' texts the intake asked of the node's changeset store, their windows let go once
  /// landed (goal 93, C1 (1)): a delta's base, or the manifest a new one is kept as a delta against.
  pub log_texts_asked: u64,
  /// The most bytes of whole text the intake held at once (goal 93, C1 (2): a revision rebuilt into one buffer, its base
  /// read whole, the text kept last for the next delta and the push's own bases cached): what a step's working set
  /// holds of texts, which a windowed rebuild bounds by the content store's window.
  pub most_text_held: u64,
  /// Of [`Intaken::log_texts_asked`], the parent manifest's read-back: the manifest the fork holds last, which a new
  /// manifest's row is kept as a delta against (`HgIndex::append_revisions_after`), asked where it is not also the
  /// delta's base, so what the kept form costs is told apart from what the push's own deltas cost.
  pub previous_manifests_asked: u64,
}

/// Format: the path a `Texts` entry gives the changelog's revisions: no tracked path holds a NUL (7.2's
/// `checkfilename`), so it meets no file's (agreed with w-perf, 2026-11-20).
pub const CHANGELOG_PATH: &[u8] = b"\0changelog";
/// Format: the path a `Texts` entry gives the manifest log's revisions (see [`CHANGELOG_PATH`]).
pub const MANIFEST_PATH: &[u8] = b"\0manifest";

/// One revision the intake kept, as the changeset log's `Texts` section names it (A-767 C4).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeptText {
  /// Its file's path, or [`CHANGELOG_PATH`] or [`MANIFEST_PATH`].
  pub path: Vec<u8>,
  /// Its node: Mercurial's tool id.
  pub node: Node,
  /// Its whole text's identity: the plain BLAKE3 of its bytes.
  pub identity: [u8; 32],
  /// Its whole text's length.
  pub len: u64,
  /// The identity of the text its payload delta rests on; none for a delta against the null base (a whole text).
  pub base: Option<[u8; 32]>,
  /// Where its delta lies in the payload (the changegroup as received).
  pub place: Range<u64>,
}

impl Intaken {
  /// What the kept texts were charged to the intake's admission, each as it was kept: the `Texts` section the landing
  /// writes, held as the record is, O(revisions), apart from a step's working set.
  pub fn texts_charged(&self) -> u64 {
    let each = to_u64(core::mem::size_of::<KeptText>());
    self.texts.iter().fold(0, |total, kept| {
      total
        .saturating_add(each)
        .saturating_add(to_u64(kept.path.len()))
    })
  }
}

/// Whether there is more to take.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Stepped {
  /// More units follow.
  More,
  /// Every unit is taken: [`Intake::finish`] gives the fork.
  Done,
  /// The step needs these bytes of the changegroup, which its source does not hold (`crate::framing`): nothing was
  /// taken; put them in and step again. A whole changegroup in memory never wants.
  Wanted(Range<u64>),
  /// The step needs a file delta's base from the node's changeset store, a revision landed before the push (A-767 C1):
  /// nothing was taken; answer the intake's `crate::render::StoreWants` and step again.
  Store,
}

/// The unit a step takes next.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stage {
  /// The changelog's revision at this position.
  Changelog(usize),
  /// The manifest log's revision at this position.
  Manifests(usize),
  /// The revision at `at` of the file group at `file`.
  Files {
    /// The file group.
    file: usize,
    /// The revision in it.
    at: usize,
  },
  /// The new changeset at this position: its manifest present.
  ManifestsPresent(usize),
  /// The file revisions the new manifests name that the fork lacked when each was read: present now, a window of
  /// them a step ([`Taker::files_present`]).
  FilesPresent,
  /// Everything taken.
  Done,
}

/// The changegroup's framing as places in its bytes (`crate::framing::Framed`), so the intake borrows nothing between
/// steps.
#[derive(Clone, Debug, Default)]
struct Frame(Framed);

impl Frame {
  /// The framing of `payload`, a whole changegroup of `version` in memory, each plain delta checked, within
  /// `admission`.
  fn of(payload: &[u8], version: Version, admission: &mut Admission) -> Result<Frame, Refused> {
    let mut framed = Framed::default();
    let mut framing = Framing::new(version, true);
    loop {
      let mut sink = Collect {
        framed: &mut framed,
        admission: &mut *admission,
      };
      match framing.step(payload, &mut sink) {
        Ok(true) => {}
        Ok(false) => break,
        Err(Halt::Refused(refusal)) => return Err(Unverified::Changegroup(refusal).into()),
        // A slice holds every byte of itself: a want names bytes past its end, a truncation.
        Err(Halt::Wanted(wanted)) => {
          return Err(
            Unverified::Changegroup(crate::changegroup::Refusal::Truncated(crate::Truncated {
              what: "changegroup",
              at: wanted.range.start,
              needed: wanted.range.end.saturating_sub(wanted.range.start),
              left: to_u64(payload.len()).saturating_sub(wanted.range.start),
            }))
            .into(),
          );
        }
      }
    }
    Frame::framed(framed)
  }

  /// A framing its caller walked (`crate::framing::Framing` over a source read a piece at a time): a changegroup of
  /// tree manifests refused, as verification refuses one.
  fn framed(framed: Framed) -> Result<Frame, Refused> {
    if !framed.trees.is_empty() {
      return Err(Unverified::TreeManifests.into());
    }
    Ok(Frame(framed))
  }

  /// The revision at `at` of `range`, its delta read from `payload`, with where its delta lies there (A-767 C4: the
  /// `Texts` section's place); none past the group, the bytes `payload` lacks where it holds them not.
  fn placed<'p, S: Source + ?Sized>(
    &self,
    payload: &'p S,
    range: &Range<usize>,
    at: usize,
  ) -> Result<Option<(Revision<'p>, Range<u64>)>, Wanted> {
    let Some(found) = range
      .start
      .checked_add(at)
      .filter(|index| *index < range.end)
      .and_then(|index| self.0.revisions.get(index))
    else {
      return Ok(None);
    };
    Ok(Some((found.revision(payload)?, found.data.clone())))
  }
}

/// A push's changegroup being taken into a fork of its repository's index (module docs): the changegroup's framing, as
/// places in its bytes, which the caller keeps and lends each step (a push's driver, from the bundle; a replay, from
/// the record), and what taking it has made so far.
pub struct Intake {
  /// The changegroup's framing.
  frame: Frame,
  /// What taking it has made so far.
  taker: Taker,
}

impl Intake {
  /// The most its steps keep charged to the push's admission from the first to the last (main 01a20182's ruling (a) of
  /// 2026-11-22 04:23: a push's need is known at its begin, so it is admitted it up front, never the learned demand that
  /// predicts an unknown one): told from its framing before any revision is taken, by the charges the steps make
  /// (`Taker::keep_taken`, `read_of`, `logged_of`). Each revision its node, its kept text and its path; each changeset
  /// its manifest's and its own nodes noted; each file revision once more, its path and node, for the new manifests
  /// that await it (`Taker::awaited`, once across the push however many snapshots name it); each log its name, a file's
  /// its path, as its group opens and as its log is noted. A step's own working set (its delta, its base, its rebuilt
  /// text, a manifest's written entries) is apart: charged to a copy and let go. Every
  /// sum saturates, so a count past what a `u64` holds reads as the most there is, past any repository's ceiling, and its
  /// push is refused, never admitted a wrapped need.
  pub fn bookkeeping(&self) -> u64 {
    let framed = &self.frame.0;
    let each = to_u64(size_of::<Node>().saturating_add(size_of::<KeptText>()));
    let kept = |count: usize, path: u64| to_u64(count).saturating_mul(each.saturating_add(path));
    let changelog = framed.changelog.len();
    let changesets = kept(changelog, to_u64(CHANGELOG_PATH.len())).saturating_add(
      to_u64(changelog).saturating_mul(to_u64(size_of::<(Node, Node)>().saturating_mul(2))),
    );
    let manifests = kept(framed.manifests.len(), to_u64(MANIFEST_PATH.len()));
    let logs = to_u64(2usize.saturating_add(framed.files.len()))
      .saturating_mul(to_u64(size_of::<(LogOf, u32)>()));
    // Each file revision once more for the new manifests awaiting it ([`Taker::awaited`]): its path and node, however
    // many snapshots name it.
    // And, cut into segments, its contents with its path until its segment lands ([`PushSegment::contents`]).
    let awaited =
      to_u64(size_of::<(Vec<u8>, Node)>()).saturating_add(to_u64(size_of::<FileContents>()));
    let files = framed.files.iter().fold(0u64, |total, (name, revisions)| {
      let path = name.end.saturating_sub(name.start);
      total
        .saturating_add(kept(revisions.len(), path))
        .saturating_add(
          to_u64(revisions.len()).saturating_mul(awaited.saturating_add(path.saturating_mul(2))),
        )
        // Its path twice: as its group opens (`Taker::files`) and as its log is noted (`logged_of`).
        .saturating_add(path.saturating_mul(2))
    });
    changesets
      .saturating_add(manifests)
      .saturating_add(logs)
      .saturating_add(files)
  }

  /// The least room one of its steps takes, told from its framing (main 01a20182's order of 2026-11-22 05:04): the widest
  /// text a delta against the null base carries, its one hunk's insert (`delta::whole_text_start`, a whole refused as
  /// `NotWhole` otherwise), since a step rebuilds such a text whole and charges it before it keeps any of it. Zero where
  /// none comes whole. A push whose keeping and this are past its repository's ceiling could never be taken there.
  pub fn widest_full_text(&self) -> u64 {
    self
      .frame
      .0
      .revisions
      .iter()
      .filter(|placed| placed.base == NULL_NODE)
      .map(|placed| {
        placed
          .data
          .end
          .saturating_sub(placed.data.start)
          .saturating_sub(to_u64(delta::HUNK_HEADER_LEN))
      })
      .max()
      .unwrap_or(0)
  }
}

/// What an intake has made so far: its fork, where it stands, and what it keeps across steps.
struct Taker {
  /// The fork: the repository's revisions, and the push's as they are kept.
  fork: HgIndex,
  /// The buffers a revision's text is rebuilt into ([`Spares`]; goal 93, no allocation by design).
  spares: Spares,
  /// The unit next.
  stage: Stage,
  /// The nodes the push brought in the log being taken, to tell a revision brought earlier from one held.
  brought: BTreeSet<Node>,
  /// Each new changeset and the manifest its text names.
  manifest_of: Vec<(Node, Node)>,
  /// The file revisions the new manifests' written entries name (each file's path and node) that the fork lacked when
  /// each was read, each once across the push, however many snapshots name it (main 01a20182's order of 2026-11-22
  /// 05:13): what the files' check reads once every file is taken ([`Taker::files_present`]).
  awaited: BTreeSet<(Vec<u8>, Node)>,
  /// The file revisions the push's framing brings: the most [`Taker::awaited`] may name, since a file revision neither
  /// held nor brought is missing.
  bringing: u64,
  /// The changeset's or manifest's text kept last, and its node: the next revision's base where its delta names it. A file
  /// revision's base is read from the fork's rows ([`patched`]).
  last: Option<(Node, Vec<u8>)>,
  /// The identity of the text kept last, so the next revision's base is named without hashing it again.
  last_identity: Option<(Node, [u8; 32])>,
  /// Whether the log being taken has kept a revision yet.
  logged: bool,
  /// The path of the file group being taken, read at its opening, so each later step reads one range of the
  /// changegroup (its revision's delta) and a source read forward never wants bytes it let go.
  path: Option<Vec<u8>>,
  /// What was brought.
  intaken: Intaken,
  /// A file delta's base asked of the store, and its answer.
  asked: Option<StoreAsk>,
  /// The changelog's and the manifest log's texts the store gave for the unit being taken (goal 93, C1 (1): those
  /// logs' texts let go once landed): its delta's base and, for a manifest, the one before it that its row is kept as a
  /// delta against; two at most, given up once the unit is taken whole ([`Taker::log_texts_ready`]).
  logs_given: Vec<(Node, Vec<u8>)>,
  /// A changeset or manifest longer than one window, being taken a window a step.
  pending: Option<Box<Pending>>,
  /// A file revision being taken a window a step, its text never whole ([`patched`]).
  patched: Option<Box<patched::Patched>>,
  /// A manifest being taken a window a step, its text never whole ([`manifesting`]).
  manifesting: Option<Box<manifesting::Manifesting>>,
  /// The base of the manifest begun again because a window of its chain could not be read: asked of the store at its
  /// next begin ([`manifesting`]).
  base_asked: Option<Node>,
  /// The merge of the fork's newest runs the size-tiered rule named, a table a step until it ends
  /// ([`Taker::merge_step`]).
  merging: Option<Merge>,
  /// The segments its kept texts are cut into as they are taken, where its driver asks for them ([`Intake::cutting`]).
  cutting: Option<Cutting>,
}

/// A push's kept texts cut into segments as they are taken (main 01a20182's ruling (b1) of 2026-11-22 04:44: a push
/// lands as a run of segment changesets, as an import does): each segment the revisions whose deltas lie in a slice of
/// at least `bound` bytes of the changegroup, so the push holds one segment's texts at a time, never the whole list.
struct Cutting {
  /// The least bytes of the changegroup a segment is cut at.
  bound: u64,
  /// The segment being cut: where its slice of the changegroup begins and ends, each log's kept revisions in it, their
  /// texts, and what those texts charged.
  open: Option<Open>,
  /// The segments cut and not yet taken, oldest first.
  ready: VecDeque<PushSegment>,
  /// The revisions' contents kept so far, all told, taken or not: what a push's pins count from
  /// ([`Intake::kept_newest`]).
  kept_total: u64,
}

impl Cutting {
  /// Notes `kept`, revision `rev` of `log` in the fork, its text charged `charged`; cuts the segment once its slice
  /// reaches the bound.
  fn kept(
    &mut self,
    (log, rev): (LogOf, u32),
    kept: KeptText,
    (charged, contents): (u64, Option<FileContents>),
  ) {
    let open = self.open.get_or_insert_with(|| Open {
      slice: kept.place.start..kept.place.end,
      ranges: Vec::new(),
      texts: Vec::new(),
      charged: 0,
      contents: Vec::new(),
    });
    if let Some(contents) = contents {
      open.contents.push(contents);
      self.kept_total = self.kept_total.saturating_add(1);
    }
    open.slice.end = open.slice.end.max(kept.place.end);
    match open.ranges.last_mut() {
      Some((last, _, end)) if *last == log && *end == rev => *end = rev.saturating_add(1),
      _ => open.ranges.push((log, rev, rev.saturating_add(1))),
    }
    open.charged = open.charged.saturating_add(charged);
    open.texts.push(kept);
    if open.slice.end.saturating_sub(open.slice.start) >= self.bound {
      self.cut();
    }
  }

  /// Cuts the segment being cut into the ready ones, its texts' places made its slice's own.
  fn cut(&mut self) {
    let Some(open) = self.open.take() else {
      return;
    };
    let from = open.slice.start;
    let texts = open
      .texts
      .into_iter()
      .map(|kept| KeptText {
        place: kept.place.start.saturating_sub(from)..kept.place.end.saturating_sub(from),
        ..kept
      })
      .collect();
    self.ready.push_back(PushSegment {
      slice: open.slice,
      ranges: open.ranges,
      texts,
      charged: open.charged,
      contents: open.contents,
    });
  }
}

/// A segment being cut ([`Cutting`]).
struct Open {
  /// Its slice of the changegroup.
  slice: Range<u64>,
  /// Each log's kept revisions in it, from the first to the one past the last, in the order they were kept.
  ranges: Vec<(LogOf, u32, u32)>,
  /// Their texts, places within the whole changegroup until it is cut.
  texts: Vec<KeptText>,
  /// What their texts charged the push's admission.
  charged: u64,
  /// Its file revisions as the content store keeps them.
  contents: Vec<FileContents>,
}

/// A segment of a push's kept revisions, cut ([`Intake::take_segment`]): its slice of the changegroup (its changeset's
/// `Payload`), each log's kept revisions in it (its `Record`'s rows, `record::Writer::begin_segment`), their texts with
/// places within the slice (its `Texts`), and what those texts charged the push's admission, which its driver gives back
/// once the segment is landed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PushSegment {
  /// Its slice of the changegroup.
  pub slice: Range<u64>,
  /// Each log's kept revisions in it, from the first to the one past the last.
  pub ranges: Vec<(LogOf, u32, u32)>,
  /// Their texts, each place within the slice.
  pub texts: Vec<KeptText>,
  /// What their texts charged the push's admission.
  pub charged: u64,
  /// Its file revisions as the content store keeps them, each by its path and node: what its driver lets go of the
  /// content store once the segment is durable on the node's changeset store, which then serves them (main 01a20182's
  /// order of 2026-11-22 14:00, the arena a whole-history push left held), and notes for a home's read once its entry
  /// commits (goal 93 item 2b(b)).
  pub contents: Vec<FileContents>,
}

/// The writer of the `Record` of `segment` (main's ruling (b1)): record v8 of a push's source, each log's kept
/// revisions in it read by identity from the intake's fork a step at a time ([`Intake::record_step`]), `room` the room
/// the intake began within; what a replay appends after the rows the segments before it appended, no text rebuilt.
/// Written a step at a time because a segment's whole record in one call was one shard step: 316 ms in a release build
/// on the big Studio over Mercurial's own push (r499, goal 93), past the slice rule. Refused where its logs cannot be
/// held.
fn segment_writer(
  segment: &PushSegment,
  room: u64,
) -> Result<crate::import::record::Writer, crate::import::record::WriteRefused> {
  use crate::import::record::{Source, WriteRefused, Writer};
  let mut ranges = Vec::new();
  ranges
    .try_reserve(segment.ranges.len())
    .map_err(|_| WriteRefused::Allocation)?;
  ranges.extend(
    segment
      .ranges
      .iter()
      .map(|(log, first, end)| (log_named(log), *first, *end)),
  );
  Writer::begin_segment(Source::Push, &ranges, room, false)
}

/// A revision longer than the content store's window taken over as many steps as it has windows (A-767 C1, the push
/// spin w-balance found: a 64 MiB file revision hashed with sha1dc, hashed again for its identity and kept, in one step,
/// spun its shard past the watchdog's window): rebuilt in the step that met it; then each step hashes one window of
/// its text into its node's check and its identity, and keeps the window of a file's bytes through the store's
/// streamed put ([`ContentStore::put_window`]); the last checks the node and keeps the row. Derived: a window a step,
/// the content store's own window, so a step's work is bounded by the store's unit, whatever the file.
struct Pending {
  /// Its log; a file's path owned.
  log: PendingLog,
  /// Its node, first and second parents and link, the dataless revision its log's reader reads.
  meta: (Node, Node, Node, Node),
  /// Its delta base's identity, none for a whole.
  base: Option<[u8; 32]>,
  /// Where it lies in the payload, past a whole's hunk.
  place: Range<u64>,
  /// Its whole text.
  text: Vec<u8>,
  /// The text's bytes hashed so far.
  at: usize,
  /// Its node's check.
  checking: node::Checking,
  /// Its identity's hash.
  identity: blake3::Hasher,
  /// Where a file's bytes begin in its text, past its metadata block; none for a changeset or a manifest.
  bytes_from: Option<usize>,
  /// A file's bytes being kept.
  writing: Option<slates_vcs::Writing>,
  /// The stage after it.
  after: Stage,
}

/// The log of a [`Pending`] revision.
enum PendingLog {
  /// The changelog.
  Changelog,
  /// The manifest log.
  Manifest,
  /// A file log, by its path.
  File(Vec<u8>),
}

impl PendingLog {
  /// The log it names.
  fn log(&self) -> Log<'_> {
    match self {
      PendingLog::Changelog => Log::Changelog,
      PendingLog::Manifest => Log::Manifest,
      PendingLog::File(path) => Log::File(path),
    }
  }
}

/// Shape: the most buffers [`Spares`] keeps: a step rebuilds one text, and a file group's end lets go of a few.
const SPARES: usize = 4;

/// The push's buffers a revision's text is rebuilt into (goal 93, no allocation by design; w-perf's attribution of
/// Mercurial's own push: `intake::rebuilt` allocated 3.76 GB freed within a second, 1.07 GB of it in the band glibc
/// keeps): every text the intake lets go (the cache's, as it evicts and as each file group ends; a changeset's or a
/// manifest's text once the next is kept, since no later delta rests on it; a base read whole for one step), at most
/// [`SPARES`] of them and the cache's bound in bytes, so the intake's working set stays its step's and twice its room. A
/// rebuild takes the largest, so a text no longer than one let go allocates nothing.
#[derive(Debug, Default)]
struct Spares {
  /// The buffers, each emptied.
  held: Vec<Vec<u8>>,
  /// Their capacities' sum.
  bytes: u64,
  /// The most bytes they may hold.
  bound: u64,
}

impl Spares {
  /// The spares of a push admitted `room` bytes.
  fn within(room: u64) -> Spares {
    Spares {
      bound: room,
      ..Spares::default()
    }
  }

  /// The largest buffer held, emptied; a new one where none is.
  fn take(&mut self) -> Vec<u8> {
    let largest = self
      .held
      .iter()
      .enumerate()
      .max_by_key(|(_, buffer)| buffer.capacity())
      .map(|(at, _)| at);
    match largest {
      Some(at) => {
        let buffer = self.held.swap_remove(at);
        self.bytes = self.bytes.saturating_sub(to_u64(buffer.capacity()));
        buffer
      }
      None => Vec::new(),
    }
  }

  /// Keeps `buffer` for a later rebuild where it fits beside the rest, in place of the smallest held where it is larger
  /// and the count is full; else lets it go.
  fn give(&mut self, mut buffer: Vec<u8>) {
    let capacity = to_u64(buffer.capacity());
    if capacity == 0 {
      return;
    }
    if self.held.len() >= SPARES {
      let smallest = self
        .held
        .iter()
        .enumerate()
        .min_by_key(|(_, held)| held.capacity())
        .map(|(at, held)| (at, held.capacity()));
      match smallest {
        Some((at, held)) if to_u64(held) < capacity => {
          let gone = self.held.swap_remove(at);
          self.bytes = self.bytes.saturating_sub(to_u64(gone.capacity()));
        }
        _ => return,
      }
    }
    if self.bytes.saturating_add(capacity) > self.bound {
      return;
    }
    buffer.clear();
    self.bytes = self.bytes.saturating_add(capacity);
    self.held.push(buffer);
  }
}

/// A file delta's base asked of the node's changeset store: its log's rows by node, then its text by identity, within
/// the room it was asked in.
#[derive(Debug)]
struct StoreAsk {
  /// The file's path.
  path: Vec<u8>,
  /// The base's node.
  node: Node,
  /// What is asked next.
  want: Option<crate::render::Want>,
  /// The most bytes its text may hold.
  most: u64,
  /// The text, once given.
  text: Option<Vec<u8>>,
  /// Why it could not be read, if it could not.
  refused: Option<Refused>,
}

/// What a revision taken a window a step hands its keep ([`Taker::keep_taken`]) in place of what its whole text would
/// tell: its bytes' or text's object kept already, its identity hashed, its whole length, a manifest row's stored fields
/// (main's ruling (B): its client's delta, or its text whole), and what reading it found. Default for a whole text.
#[derive(Default)]
struct Streamed {
  /// The object kept already: a file revision's bytes, a manifest's delta or text.
  kept: Option<ContentId>,
  /// Its identity, hashed as it streamed.
  identity: Option<[u8; 32]>,
  /// Its whole length, where the text handed is not it.
  len: Option<u64>,
  /// A manifest row's delta base, delta length and chain, where its content is kept already.
  stored: Option<crate::index::Stored>,
  /// What reading it found as it streamed.
  read: Option<ReadAs>,
}

/// What reading a revision's text as its log's reader reads it found ([`Taker::read_of`]), noted once it is kept
/// ([`Taker::apply_read`]).
enum ReadAs {
  /// A changeset, and the manifest it names.
  Changeset {
    /// The changeset's node.
    node: Node,
    /// Its manifest's node.
    manifest: Node,
  },
  /// A manifest, and the file revisions its written entries name that the fork lacks and none before awaited.
  Manifest {
    /// Each path and its file node.
    awaited: Vec<(Vec<u8>, Node)>,
  },
  /// A file revision: nothing to note.
  File,
}

/// The log a text asked of the store is kept under (a file's path, or [`CHANGELOG_PATH`] or [`MANIFEST_PATH`]), as a
/// refusal names it.
fn log_of_path(path: &[u8]) -> LogOf {
  if path == CHANGELOG_PATH {
    LogOf::Changelog
  } else if path == MANIFEST_PATH {
    LogOf::Manifests
  } else {
    LogOf::File(path.to_vec())
  }
}

impl crate::render::StoreWants for Intake {
  fn wants(&self) -> Option<&crate::render::Want> {
    self
      .taker
      .asked
      .as_ref()
      .and_then(|asked| asked.want.as_ref())
  }

  fn give_rows(&mut self, rows: Vec<Vec<Option<crate::render::StoredRevision>>>) {
    let Some(asked) = self.taker.asked.as_mut() else {
      return;
    };
    match rows
      .into_iter()
      .next()
      .and_then(|log| log.into_iter().next())
      .flatten()
    {
      Some(row) => {
        asked.want = Some(crate::render::Want::Texts {
          texts: vec![row.text],
          most: asked.most,
        });
      }
      None => {
        asked.want = None;
        asked.refused = Some(Refused::Unverified(Unverified::UnknownDeltaBase {
          log: log_of_path(&asked.path),
          node: asked.node,
          base: asked.node,
        }));
      }
    }
  }

  fn give_chunks(&mut self, _chunks: Vec<Vec<u8>>) {
    if let Some(asked) = self.taker.asked.as_mut() {
      asked.want = None;
    }
  }

  fn give_texts(&mut self, texts: Vec<Vec<u8>>) {
    let Some(asked) = self.taker.asked.as_mut() else {
      return;
    };
    asked.want = None;
    asked.text = texts.into_iter().next();
    if asked.text.is_none() {
      asked.refused = Some(Refused::Unverified(Unverified::UnknownDeltaBase {
        log: log_of_path(&asked.path),
        node: asked.node,
        base: asked.node,
      }));
    }
  }

  fn reads_bases(&self) -> bool {
    true
  }

  fn texts_past(&mut self) {
    if let Some(asked) = self.taker.asked.as_mut() {
      asked.want = None;
      asked.refused = Some(Refused::from(crate::PastAdmission {
        asked: asked.most.saturating_add(1),
        left: asked.most,
      }));
    }
  }
}

impl Intake {
  /// Answers the store's want from the fork's own texts, as a test's stand-in for the node's changeset store (the tests
  /// keep no store): the base the step asked for, read back from the fork's content store.
  #[cfg(test)]
  /// The text a step asked of the store, by its log's path and its node, if any: the tests' stand-in for the node's
  /// changeset store reads it.
  #[cfg(test)]
  pub(crate) fn asked_text(&self) -> Option<(Vec<u8>, Node)> {
    self
      .taker
      .asked
      .as_ref()
      .map(|asked| (asked.path.clone(), asked.node))
  }

  /// The tests' stand-in for the node's changeset store answering the text asked with `text`, none where it keeps none.
  #[cfg(test)]
  pub(crate) fn answer_text(&mut self, text: Option<Vec<u8>>) {
    let Some(asked) = self.taker.asked.as_mut() else {
      return;
    };
    asked.want = None;
    match text {
      Some(text) => asked.text = Some(text),
      None => {
        asked.refused = Some(Refused::Unverified(Unverified::UnknownDeltaBase {
          log: log_of_path(&asked.path),
          node: asked.node,
          base: asked.node,
        }));
      }
    }
  }

  #[cfg(test)]
  pub(crate) fn answer_from_fork(&mut self, store: &ContentStore) {
    let Some(asked) = self.taker.asked.as_mut() else {
      return;
    };
    let held = IndexHeld::new(&self.taker.fork, store);
    asked.want = None;
    match held.file_text(&asked.path, &asked.node) {
      Some(text) => asked.text = Some(text.into_owned()),
      None => {
        asked.refused = Some(Refused::Unverified(Unverified::UnknownDeltaBase {
          log: log_of_path(&asked.path),
          node: asked.node,
          base: asked.node,
        }));
      }
    }
  }
}

impl Intake {
  /// Starts taking a changegroup of `version`, `payload` its bytes, into a fork of `index` (module docs), its framing
  /// decoded within `admission`. Every step is lent the same bytes.
  pub fn begin(
    (version, payload): (Version, &[u8]),
    index: &mut HgIndex,
    store: &mut ContentStore,
    admission: &mut Admission,
  ) -> Result<Self, Refused> {
    let frame = Frame::of(payload, version, admission)?;
    Intake::with_frame(frame, index, store, admission.left())
  }

  /// Starts taking a changegroup its caller framed (`crate::framing::Framing`, over a source read a piece at a time,
  /// each revision charged as it was placed) into a fork of `index`; every step is lent the same changegroup's bytes,
  /// as far as its source holds them ([`Stepped::Wanted`]).
  pub fn begin_framed(
    framed: Framed,
    index: &mut HgIndex,
    store: &mut ContentStore,
    room: u64,
  ) -> Result<Self, Refused> {
    Intake::with_frame(Frame::framed(framed)?, index, store, room)
  }

  /// [`Intake::begin`]'s fork made over `frame`.
  fn with_frame(
    frame: Frame,
    index: &mut HgIndex,
    store: &mut ContentStore,
    room: u64,
  ) -> Result<Self, Refused> {
    let fork = index.fork(store).map_err(Unindexed::Fork)?;
    let bringing = frame.0.files.iter().fold(0_u64, |total, (_, revisions)| {
      total.saturating_add(to_u64(revisions.len()))
    });
    Ok(Intake {
      frame,
      taker: Taker {
        fork,
        stage: Stage::Changelog(0),
        brought: BTreeSet::new(),
        manifest_of: Vec::new(),
        awaited: BTreeSet::new(),
        bringing,
        last: None,
        spares: Spares::within(room),
        last_identity: None,
        logged: false,
        path: None,
        intaken: Intaken::default(),
        asked: None,
        logs_given: Vec::new(),
        pending: None,
        patched: None,
        manifesting: None,
        base_asked: None,
        merging: None,
        cutting: None,
      },
    })
  }

  /// The intake, its kept texts cut into segments of at least `bound` bytes of the changegroup as they are taken
  /// ([`Intake::take_segment`]), never held whole (main's ruling (b1)).
  pub fn cutting(mut self, bound: u64) -> Self {
    self.taker.cutting = Some(Cutting {
      bound: bound.max(1),
      open: None,
      ready: VecDeque::new(),
      kept_total: 0,
    });
    self
  }

  /// The contents of every revision kept and not yet taken in a segment: the one being cut's, then those of the segments
  /// cut and not taken, oldest first (goal 93: what a push pins against a landed segment's let-go, `super::pins`). Empty
  /// where the intake cuts no segments.
  pub fn kept_contents(&self) -> impl Iterator<Item = &FileContents> + '_ {
    self.taker.cutting.iter().flat_map(|cutting| {
      cutting
        .open
        .iter()
        .flat_map(|open| open.contents.iter())
        .chain(
          cutting
            .ready
            .iter()
            .flat_map(|segment| segment.contents.iter()),
        )
    })
  }

  /// The revisions' contents kept so far, all told, taken in a segment or not: none where the intake cuts no segments.
  pub fn kept_total(&self) -> u64 {
    self
      .taker
      .cutting
      .as_ref()
      .map_or(0, |cutting| cutting.kept_total)
  }

  /// The contents of the `count` revisions kept last, newest first, among those not yet taken in a segment: what a slice
  /// kept since a pin last looked, in time of its own count (main's 57k push at 2dd25afe6: every kept revision looked at
  /// after every slice, its CPU the push's revisions times its slices).
  pub fn kept_newest(&self, count: u64) -> impl Iterator<Item = &FileContents> + '_ {
    let count = usize::try_from(count).unwrap_or(usize::MAX);
    self
      .taker
      .cutting
      .iter()
      .flat_map(|cutting| {
        cutting
          .open
          .iter()
          .flat_map(|open| open.contents.iter().rev())
          .chain(
            cutting
              .ready
              .iter()
              .rev()
              .flat_map(|segment| segment.contents.iter().rev()),
          )
      })
      .take(count)
  }

  /// The next segment cut, oldest first; none while the one being cut is short of its bound.
  pub fn take_segment(&mut self) -> Option<PushSegment> {
    self.taker.cutting.as_mut()?.ready.pop_front()
  }

  /// The writer of the `Record` of `segment`, cut from this intake (main's ruling (b1); [`segment_writer`]): stepped by
  /// [`Intake::record_step`] over its fork, a turn's steps at a time, then finished (`Writer::finish`).
  pub fn record_writer(
    &self,
    segment: &PushSegment,
    room: u64,
  ) -> Result<crate::import::record::Writer, crate::import::record::WriteRefused> {
    segment_writer(segment, room)
  }

  /// One step of `writer`, a segment's record, over this intake's fork: whether more steps follow. Refused where a row
  /// cannot be read or written.
  pub fn record_step(
    &self,
    writer: &mut crate::import::record::Writer,
    store: &ContentStore,
  ) -> Result<bool, crate::import::record::WriteRefused> {
    writer.step(&self.taker.fork, store)
  }

  /// Cuts the segment being cut whatever its length: the push's last, once every unit is taken.
  pub fn cut_rest(&mut self) {
    if let Some(cutting) = self.taker.cutting.as_mut() {
      cutting.cut();
    }
  }

  /// Takes the next unit of the changegroup whose bytes are `payload`, the ones it began with (module docs). Refused,
  /// the caller gives the fork back ([`Intake::abandon`]).
  pub fn step<S: Source + ?Sized>(
    &mut self,
    payload: &S,
    store: &mut ContentStore,
    admission: &mut Admission,
  ) -> Result<Stepped, Refused> {
    let Intake { frame, taker } = self;
    if taker.pending.is_some() {
      return taker.advance_pending(store, admission);
    }
    if taker.patched.is_some() {
      return taker.advance_patched((frame, payload), store, admission);
    }
    if taker.manifesting.is_some() {
      return taker.advance_manifest((frame, payload), store, admission);
    }
    if let Some(stepped) = taker.merge_step(store)? {
      return Ok(stepped);
    }
    match taker.stage {
      Stage::Changelog(at) => {
        let next = match frame.placed(payload, &frame.0.changelog, at) {
          Ok(next) => next,
          Err(wanted) => return Ok(Stepped::Wanted(wanted.range)),
        };
        taker.take_or(
          next,
          Log::Changelog,
          (store, admission),
          Stage::Manifests(0),
        )
      }
      Stage::Manifests(at) => {
        let next = match frame.placed(payload, &frame.0.manifests, at) {
          Ok(next) => next,
          Err(wanted) => return Ok(Stepped::Wanted(wanted.range)),
        };
        taker.manifest_stage((frame, payload), (next, at), (store, admission))
      }
      Stage::Files { file, at } => taker.file_step((frame, payload), (file, at), store, admission),
      Stage::ManifestsPresent(at) => taker.manifest_present(at, store),
      Stage::FilesPresent => taker.files_present(store),
      Stage::Done => Ok(Stepped::Done),
    }
  }

  /// Whether a merge of the fork's runs is under way ([`Taker::merge_step`]): the next step cuts one of its tables. What
  /// a caller timing its steps reads to tell a merge's cost from a revision's.
  pub fn merging(&self) -> bool {
    self.taker.merging.is_some()
  }

  /// The kind of unit the next step takes, as [`Intake::step`] chooses it: `pending` (a revision longer than a window,
  /// a window more), `merge` (a table of the fork's merge), else the stage's (`changelog`, `manifest`, `file`,
  /// `manifests-present`, `files-present`, `done`). What a caller timing its steps reads to tell which unit a long one
  /// took.
  pub fn unit(&self) -> &'static str {
    if self.taker.pending.is_some()
      || self.taker.patched.is_some()
      || self.taker.manifesting.is_some()
    {
      return "pending";
    }
    if self.taker.merging.is_some() {
      return "merge";
    }
    match self.taker.stage {
      Stage::Changelog(_) => "changelog",
      Stage::Manifests(_) => "manifest",
      Stage::Files { .. } => "file",
      Stage::ManifestsPresent(_) => "manifests-present",
      Stage::FilesPresent => "files-present",
      Stage::Done => "done",
    }
  }

  /// The fork and what was brought, once every unit is taken; the intake itself (boxed: it is large) while there is
  /// more.
  pub fn finish(self) -> Result<(HgIndex, Intaken), Box<Self>> {
    if self.taker.stage == Stage::Done {
      Ok((self.taker.fork, self.taker.intaken))
    } else {
      Err(Box::new(self))
    }
  }

  /// Gives the fork back: the repository's index and the content store as they were before the intake began.
  pub fn abandon(mut self, store: &mut ContentStore) -> Result<(), IndexRefused> {
    if let Some(writing) = self
      .taker
      .pending
      .take()
      .and_then(|pending| pending.writing)
    {
      store.abandon_object(writing);
    }
    // A revision taken a window a step gives back its object being put, and a manifest the base the store gave it.
    if let Some(patched) = self.taker.patched.take() {
      patched::give_back(store, *patched);
    }
    if let Some(manifesting) = self.taker.manifesting.take() {
      manifesting::give_back(store, *manifesting);
    }
    self.taker.fork.release(store)
  }
}

impl Taker {
  /// Takes manifest `next`, at `at` of its group, a window a step ([`manifesting`]); or moves to the files where the
  /// group has no more.
  fn manifest_stage<S: Source + ?Sized>(
    &mut self,
    (frame, payload): (&Frame, &S),
    (next, at): (Option<(Revision<'_>, Range<u64>)>, usize),
    (store, admission): (&mut ContentStore, &mut Admission),
  ) -> Result<Stepped, Refused> {
    let Some((revision, place)) = next else {
      self.enter(Stage::Files { file: 0, at: 0 });
      return Ok(Stepped::More);
    };
    let after = Stage::Manifests(at.saturating_add(1));
    if self.begin_manifest((&revision, place), (at, after), (store, admission))? {
      self.stage = after;
      return Ok(Stepped::More);
    }
    // Its first window made in this same step, so a manifest of one window is taken in one step as before.
    self.advance_manifest((frame, payload), store, admission)
  }

  /// Takes `next` of `log`, or moves to `after` where its group has no more.
  fn take_or(
    &mut self,
    next: Option<(Revision<'_>, Range<u64>)>,
    log: Log<'_>,
    (store, admission): (&mut ContentStore, &mut Admission),
    after: Stage,
  ) -> Result<Stepped, Refused> {
    let Some((revision, place)) = next else {
      self.enter(after);
      return Ok(Stepped::More);
    };
    let after = match self.stage {
      Stage::Changelog(at) => Stage::Changelog(at.saturating_add(1)),
      Stage::Manifests(at) => Stage::Manifests(at.saturating_add(1)),
      other => other,
    };
    if let Some(stepped) = self.log_texts_ready(log, &revision, store)? {
      return Ok(stepped);
    }
    // A revision longer than a window is taken over the steps after this one ([`Pending`]), which move the stage on.
    if self.take(log, (&revision, place), None, (store, admission), after)? {
      self.stage = after;
      self.logs_given.clear();
    }
    Ok(Stepped::More)
  }

  /// Whether the texts a changelog's or a manifest's revision's step reads are at hand (goal 93, C1 (1): those logs'
  /// texts let go once landed, main's ruling of 2026-11-23 20:53): its delta's base, and for a manifest the one the fork
  /// holds last, which its row is kept as a delta against (`HgIndex::append_revisions_after`). One the fork holds whose
  /// text is not at hand is asked of the node's changeset store ([`StoreAsk`] under its log's path) and the step taken
  /// again once given, nothing taken meanwhile: `Store`; the text given is kept for the unit ([`Taker::logs_given`]).
  /// None where the step may go on. Refused where the store gave nothing for the one asked.
  fn log_texts_ready(
    &mut self,
    log: Log<'_>,
    revision: &Revision<'_>,
    store: &ContentStore,
  ) -> Result<Option<Stepped>, Refused> {
    let path = match log {
      Log::Changelog => CHANGELOG_PATH,
      Log::Manifest => MANIFEST_PATH,
      Log::File(_) => return Ok(None),
    };
    if let Some(asked) = self.asked.take() {
      if asked.path != path {
        self.asked = Some(asked);
      } else if let Some(refused) = asked.refused {
        return Err(refused);
      } else if let Some(text) = asked.text {
        self.logs_given.push((asked.node, text));
      } else {
        self.asked = Some(asked);
        return Ok(Some(Stepped::Store));
      }
    }
    if self.fork.rev(store, log, &revision.node).is_some() {
      return Ok(None);
    }
    let previous = match log {
      Log::Manifest => self
        .fork
        .len(store, log)
        .checked_sub(1)
        .and_then(|rev| self.fork.revision(store, log, rev))
        .map(|row| row.node),
      _ => None,
    };
    let wanted = [Some(revision.base), previous]
      .into_iter()
      .flatten()
      .filter(|node| *node != NULL_NODE)
      .filter(|node| !self.has_text(node))
      .find_map(|node| {
        let (len, at_hand) = crate::push::held::log_text_at_hand((&self.fork, store), log, &node)?;
        (!at_hand).then_some((node, len))
      });
    let Some((node, most)) = wanted else {
      return Ok(None);
    };
    if previous == Some(node) && revision.base != node {
      self.intaken.previous_manifests_asked =
        self.intaken.previous_manifests_asked.saturating_add(1);
    }
    self.asked = Some(StoreAsk {
      path: path.to_vec(),
      node,
      want: Some(crate::render::Want::Rows {
        logs: vec![(path.to_vec(), vec![node])],
      }),
      most,
      text: None,
      refused: None,
    });
    self.intaken.log_texts_asked = self.intaken.log_texts_asked.saturating_add(1);
    Ok(Some(Stepped::Store))
  }

  /// Whether the text of `node` is the one kept last or one the store gave for the unit ([`Taker::logs_given`]).
  fn has_text(&self, node: &Node) -> bool {
    self.last.as_ref().is_some_and(|(last, _)| last == node)
      || self.logs_given.iter().any(|(given, _)| given == node)
  }

  /// One step of the file groups: a group opened (its path read, audited and kept, charged to `admission`), then its
  /// revisions a step each; after the last group, the checks.
  fn file_step<S: Source + ?Sized>(
    &mut self,
    (frame, payload): (&Frame, &S),
    (file, at): (usize, usize),
    store: &mut ContentStore,
    admission: &mut Admission,
  ) -> Result<Stepped, Refused> {
    let Some((name, revisions)) = frame.0.files.get(file).cloned() else {
      self.enter(Stage::ManifestsPresent(0));
      return Ok(Stepped::More);
    };
    let Some(path) = self.path.take() else {
      let path = match payload.bytes(name) {
        Ok(path) => path,
        Err(wanted) => return Ok(Stepped::Wanted(wanted.range)),
      };
      audit(path)?;
      admission.charge(to_u64(path.len()))?;
      self.path = Some(path.to_vec());
      return Ok(Stepped::More);
    };
    let next = match frame.placed(payload, &revisions, at) {
      Ok(next) => next,
      Err(wanted) => {
        self.path = Some(path);
        return Ok(Stepped::Wanted(wanted.range));
      }
    };
    let Some((revision, place)) = next else {
      self.enter(Stage::Files {
        file: file.saturating_add(1),
        at: 0,
      });
      return Ok(Stepped::More);
    };
    // Taken a window a step, its base read from the fork's rows ([`patched`]): begun, then its first window made in this
    // same step, so a revision of one window is taken in one step as before.
    let after = Stage::Files {
      file,
      at: at.saturating_add(1),
    };
    let begun = self.begin_patched(
      (&path, &revision, place),
      (&revisions, at, after),
      (store, admission),
    );
    self.path = Some(path);
    if begun? {
      self.stage = after;
      return Ok(Stepped::More);
    }
    self.advance_patched((frame, payload), store, admission)
  }

  /// Moves to `stage`, a new log's: what the last one brought is let go.
  fn enter(&mut self, stage: Stage) {
    self.path = None;
    self.asked = None;
    self.brought.clear();
    if let Some((_, text)) = self.last.take() {
      self.spares.give(text);
    }
    self.last_identity = None;
    self.logged = false;
    self.stage = stage;
  }

  /// The name `log` takes among the intake's logs, once per log, its name charged to `admission`; nothing is noted
  /// until [`Taker::note_logged`] takes it, so a refusal after it leaves the intake as it was.
  fn logged_of(
    &self,
    log: Log<'_>,
    store: &ContentStore,
    admission: &mut Admission,
  ) -> Result<Option<(LogOf, u32)>, Refused> {
    if self.logged {
      return Ok(None);
    }
    let named = log_of(log);
    admission.charge_items::<(LogOf, u32)>(1)?;
    if let LogOf::File(path) = &named {
      admission.charge(to_u64(path.len()))?;
    }
    Ok(Some((named, self.fork.len(store, log))))
  }

  /// Notes the log [`Taker::logged_of`] charged, where it named one.
  fn note_logged(&mut self, logged: Option<(LogOf, u32)>) {
    if let Some(logged) = logged {
      self.intaken.logs.push(logged);
      self.logged = true;
    }
  }

  /// Whether revision `node` of `log` is passed over: brought already by this push, or held already by the fork (a
  /// changeset held is charged and named among the held).
  fn already_brought(
    &mut self,
    log: Log<'_>,
    node: Node,
    store: &ContentStore,
    admission: &mut Admission,
  ) -> Result<bool, Refused> {
    let held = !self.brought.contains(&node) && self.fork.rev(store, log, &node).is_some();
    if !self.brought.contains(&node) && !held {
      return Ok(false);
    }
    self.intaken.passed_over = self.intaken.passed_over.saturating_add(1);
    if held && log == Log::Changelog {
      admission.charge_items::<Node>(1)?;
      self.intaken.held_changesets.push(node);
    }
    Ok(true)
  }

  /// A step of the fork's merge (goal 93, the index seal's reland): a table of the merge the size-tiered rule named
  /// ([`slates_vcs::Index::tiered_due`]), or its begin, between revisions; none where none is due. Every append seals
  /// the fork's tail when due (`HgIndex::insert`), each seal a run, and a lookup walks every run: unmerged, a push's
  /// runs grew a seal at a time and each new row's lookup with them (arena-14: shard 0 spun past 1 s in a segment step).
  /// Merged so, the fork's own runs stay within the rule's logarithm of its own bytes, a table's work a step (each step puts
  /// one table or gives one back). A push refused or given back drops its merge with the fork, whose release gives back
  /// what the merge put.
  fn merge_step(&mut self, store: &mut ContentStore) -> Result<Option<Stepped>, Refused> {
    let as_refused = |refused: IndexRefused| {
      Refused::Unindexed(Unindexed::Index(crate::index::Refused::Index(refused)))
    };
    if let Some(mut merge) = self.merging.take() {
      match self
        .fork
        .rows()
        .merge_step(store, &mut merge)
        .map_err(as_refused)?
      {
        MergeStep::More => self.merging = Some(merge),
        MergeStep::Done(_) => {
          self.intaken.tiered_merges = self.intaken.tiered_merges.saturating_add(1)
        }
        MergeStep::GivenUp => {}
      }
      return Ok(Some(Stepped::More));
    }
    let own = to_u64(self.fork.rows().own_runs());
    self.intaken.most_own_runs_in_flight = self.intaken.most_own_runs_in_flight.max(own);
    let Some(runs) = self.fork.rows().tiered_due() else {
      // No merge due: the runs are at the rule's invariant, where its bound holds.
      self.intaken.most_own_runs = self.intaken.most_own_runs.max(own);
      return Ok(None);
    };
    let begun = self
      .fork
      .rows()
      .merge_begin(store, runs)
      .map_err(as_refused)?;
    Ok(begun.map(|merge| {
      self.merging = Some(merge);
      Stepped::More
    }))
  }

  /// Takes one revision of `log` (module docs): passed over, or checked, rebuilt and kept.
  fn take(
    &mut self,
    log: Log<'_>,
    (revision, place): (&Revision<'_>, Range<u64>),
    given: Option<Vec<u8>>,
    (store, admission): (&mut ContentStore, &mut Admission),
    after: Stage,
  ) -> Result<bool, Refused> {
    if self.already_brought(log, revision.node, store, admission)? {
      return Ok(true);
    }
    self.check_known(log, revision, store)?;
    let mut room = *admission;
    let into = self.spares.take();
    let base_rebuilt = given.is_none() && revision.base != NULL_NODE;
    let base_text = match given {
      Some(text) => Some(Cow::Owned(text)),
      None => self.base_text(log, revision, store),
    };
    let base_rebuilt = base_rebuilt && matches!(base_text, Some(Cow::Owned(_)));
    let base = match (&base_text, &self.last_identity) {
      _ if revision.base == NULL_NODE => None,
      (_, Some((node, identity))) if *node == revision.base => Some(*identity),
      (Some(text), _) => Some(*blake3::hash(text).as_bytes()),
      (None, _) => None,
    };
    let text = rebuilt(base_text.as_deref(), revision, log, (&mut room, into))?;
    let held = to_u64(text.len())
      .saturating_add(base_text.as_deref().map_or(0, |base| to_u64(base.len())))
      .saturating_add(self.last.as_ref().map_or(0, |(_, last)| to_u64(last.len())));
    // A base read whole for this step alone is a later rebuild's buffer.
    if let Some(Cow::Owned(base)) = base_text {
      self.spares.give(base);
    }
    self.intaken.most_text_held = self.intaken.most_text_held.max(held);
    if base_rebuilt {
      self.intaken.rebuilt = self.intaken.rebuilt.saturating_add(1);
    }
    // A whole is kept as its raw text (the store's contract, A-767 C1): its place past the one hunk it arrived in.
    let place = if base.is_none() {
      let start = delta::whole_text_start(revision.data).ok_or_else(|| Unverified::NotWhole {
        log: log_of(log),
        node: revision.node,
      })?;
      place.start.saturating_add(to_u64(start))..place.end
    } else {
      place
    };
    let window = store.window();
    if text.len() > window {
      self.begin_pending(log, revision, (text, base, place), after)?;
      return Ok(false);
    }
    self.intaken.longest_step_bytes = self.intaken.longest_step_bytes.max(to_u64(text.len()));
    node::check(&revision.node, &revision.p1, &revision.p2, &text).map_err(|fault| {
      Unverified::Node {
        log: log_of(log),
        node: revision.node,
        fault,
      }
    })?;
    self.keep_taken(
      log,
      revision,
      (text, base, place),
      Streamed::default(),
      (store, admission),
    )?;
    Ok(true)
  }

  /// Begins taking a revision longer than one window over the steps that follow ([`Pending`]): its text rebuilt, its
  /// place, and where a file's bytes begin past its metadata block.
  fn begin_pending(
    &mut self,
    log: Log<'_>,
    revision: &Revision<'_>,
    (text, base, place): (Vec<u8>, Option<[u8; 32]>, Range<u64>),
    after: Stage,
  ) -> Result<(), Refused> {
    let (pending_log, bytes_from) = match log {
      Log::Changelog => (PendingLog::Changelog, None),
      Log::Manifest => (PendingLog::Manifest, None),
      Log::File(path) => {
        let split = filerev::split(&text).map_err(|fault| Unverified::FileRevision {
          path: path.to_vec(),
          node: revision.node,
          fault,
        })?;
        let from = text.len().saturating_sub(split.bytes.len());
        (PendingLog::File(path.to_vec()), Some(from))
      }
    };
    self.intaken.spread = self.intaken.spread.saturating_add(1);
    self.pending = Some(Box::new(Pending {
      log: pending_log,
      meta: (revision.node, revision.p1, revision.p2, revision.link),
      base,
      place,
      text,
      at: 0,
      checking: node::Checking::new(&revision.node, &revision.p1, &revision.p2),
      identity: blake3::Hasher::new(),
      bytes_from,
      writing: None,
      after,
    }));
    Ok(())
  }

  /// One step of a [`Pending`] revision: its next window hashed into its node's check and its identity, and kept where
  /// it is a file's bytes; after its last, its node checked and its row kept, and the stage moved on.
  fn advance_pending(
    &mut self,
    store: &mut ContentStore,
    admission: &mut Admission,
  ) -> Result<Stepped, Refused> {
    let window = store.window();
    let Some(pending) = self.pending.as_mut() else {
      return Ok(Stepped::More);
    };
    let end = pending.text.len().min(pending.at.saturating_add(window));
    let piece = pending.text.get(pending.at..end).unwrap_or_default();
    pending.checking.update(piece);
    pending.identity.update(piece);
    if let Some(from) = pending.bytes_from {
      let start = pending.at.max(from);
      if let Some(bytes) = pending
        .text
        .get(start..end)
        .filter(|bytes| !bytes.is_empty())
      {
        let writing = pending.writing.get_or_insert_with(|| store.begin_object());
        store
          .put_window(writing, bytes)
          .map_err(|refused| Refused::Unindexed(Unindexed::from(refused)))?;
      }
    }
    self.intaken.longest_step_bytes = self
      .intaken
      .longest_step_bytes
      .max(to_u64(end.saturating_sub(pending.at)));
    pending.at = end;
    if pending.at < pending.text.len() {
      return Ok(Stepped::More);
    }
    let Some(pending) = self.pending.take() else {
      return Ok(Stepped::More);
    };
    let Pending {
      log,
      meta: (node, p1, p2, link),
      base,
      place,
      text,
      checking,
      identity,
      writing,
      after,
      ..
    } = *pending;
    let log = log.log();
    // Taken out of the intake, the writer's windows are this step's to give back on any refusal: the intake's own give
    // back (`Intake::abandon`) no longer reaches them.
    if let Err(fault) = checking.finish() {
      if let Some(writing) = writing {
        store.abandon_object(writing);
      }
      return Err(
        Unverified::Node {
          log: log_of(log),
          node,
          fault,
        }
        .into(),
      );
    }
    let kept = match writing {
      Some(mut writing) => match store.finish_object(&mut writing) {
        Ok(kept) => Some(kept),
        // A refused finish leaves the windows it acquired with the writer for its caller to give back.
        Err(refused) => {
          store.abandon_object(writing);
          return Err(Refused::Unindexed(Unindexed::from(refused)));
        }
      },
      None => None,
    };
    let revision = Revision {
      node,
      p1,
      p2,
      base: NULL_NODE,
      link,
      flags: 0,
      info: None,
      protocol_flags: 0,
      data: &[],
      sidedata: None,
    };
    let identity = *identity.finalize().as_bytes();
    self.keep_taken(
      log,
      &revision,
      (text, base, place),
      Streamed {
        kept,
        identity: Some(identity),
        ..Streamed::default()
      },
      (store, admission),
    )?;
    self.stage = after;
    self.logs_given.clear();
    Ok(Stepped::More)
  }

  /// Keeps a revision whose node is checked (module docs): read as its log reads it, its row appended into the fork
  /// (`kept` its file bytes where they were streamed already), its `Texts` entry named (`identity` where it was hashed
  /// already), and it the text kept last.
  fn keep_taken(
    &mut self,
    log: Log<'_>,
    revision: &Revision<'_>,
    (text, base, place): (Vec<u8>, Option<[u8; 32]>, Range<u64>),
    streamed: Streamed,
    (store, admission): (&mut ContentStore, &mut Admission),
  ) -> Result<(), Refused> {
    let Streamed {
      kept,
      identity,
      len: whole,
      stored,
      read,
    } = streamed;
    // Every charge first, to a trial of the admission, then the keep, then every note and the trial taken: a refusal
    // anywhere leaves the intake where it was and the admission unspent, so the step is taken again whole once its room
    // has grown (main 01a20182's ruling of 2026-11-22 03:03: an intake past its admission waits, never refused).
    let mut trial = *admission;
    let read = match read {
      // Read as it streamed ([`patched`]'s manifests): what noting it holds charged here, as `awaited_of` charges it.
      Some(read) => {
        if let ReadAs::Manifest { awaited } = &read {
          trial.charge_items::<(Vec<u8>, Node)>(awaited.len())?;
          for (path, _) in awaited {
            trial.charge(to_u64(path.len()))?;
          }
        }
        read
      }
      None => self.read_of(log, revision, &text, store, &mut trial)?,
    };
    let logged = self.logged_of(log, store, &mut trial)?;
    let path = match log {
      Log::Changelog => CHANGELOG_PATH,
      Log::Manifest => MANIFEST_PATH,
      Log::File(path) => path,
    };
    trial.charge_items::<Node>(1)?;
    trial.charge_items::<KeptText>(1)?;
    trial.charge(to_u64(path.len()))?;
    // A file revision taken a window a step carries its framing alone: its length is the whole text's.
    let len = whole.unwrap_or_else(|| to_u64(text.len()));
    let mut room = *admission;
    let arrived = Arrived {
      node: revision.node,
      p1: revision.p1,
      p2: revision.p2,
      link: revision.link,
      text,
    };
    // The manifest the fork holds last, which this one's row is kept as a delta against: the text kept last, else one
    // the store gave for the unit (`Taker::log_texts_ready`).
    let held_last = match log {
      Log::Manifest => self
        .fork
        .len(store, log)
        .checked_sub(1)
        .and_then(|rev| self.fork.revision(store, log, rev))
        .map(|row| row.node),
      _ => None,
    };
    let previous = held_last
      .and_then(|held_last| self.logs_given.iter().find(|(node, _)| *node == held_last))
      .or(self.last.as_ref())
      .map(|(node, text)| (node, text.as_slice()));
    keep_one(
      &mut self.fork,
      store,
      log,
      (&arrived, previous),
      (kept, whole, stored),
      &mut room,
    )?;
    *admission = trial;
    self.apply_read(read);
    self.note_logged(logged);
    self.brought.insert(revision.node);
    self.intaken.kept = self.intaken.kept.saturating_add(1);
    self.intaken.bytes = self.intaken.bytes.saturating_add(len);
    let identity = identity.unwrap_or_else(|| *blake3::hash(&arrived.text).as_bytes());
    let kept = KeptText {
      path: path.to_vec(),
      node: revision.node,
      identity,
      len,
      base,
      place,
    };
    match self.cutting.as_mut() {
      Some(cutting) => {
        let rev = self.fork.len(store, log).saturating_sub(1);
        // Every log's: a file revision's bytes and block, a changeset's text and a manifest's row object, the
        // changelog's and the manifests' under their logs' paths (goal 93, C1 (1) P5: let go once the push's entry
        // commits, never before; `hg_run::let_go_windows`).
        let contents = match log {
          Log::Changelog => self.fork.changeset(store, rev).map(|row| FileContents {
            path: CHANGELOG_PATH.to_vec(),
            node: revision.node,
            bytes: row.text,
            block: None,
          }),
          Log::Manifest | Log::File(_) => {
            self.fork.revision(store, log, rev).map(|row| FileContents {
              path: path.to_vec(),
              node: revision.node,
              bytes: row.bytes,
              block: row.block,
            })
          }
        };
        let charged = to_u64(size_of::<KeptText>())
          .saturating_add(to_u64(path.len()))
          .saturating_add(contents.as_ref().map_or(0, |contents| {
            to_u64(size_of::<FileContents>()).saturating_add(to_u64(contents.path.len()))
          }));
        cutting.kept((log_of(log), rev), kept, (charged, contents));
      }
      None => self.intaken.texts.push(kept),
    }
    self.last_identity = Some((revision.node, identity));
    // A changeset's or a manifest's text is the next one's base (each rests on the one before); the one before it is no
    // later delta's, so its buffer is the next rebuild's. A file revision's later bases are read from the fork's rows
    // ([`patched`]), so its framing is kept by no one.
    if let Some((_, text)) = self.last.take() {
      self.spares.give(text);
    }
    match log {
      Log::Changelog => self.last = Some((revision.node, arrived.text)),
      // A file revision's and a manifest's later bases are read from the fork's rows ([`patched`]).
      _ => self.spares.give(arrived.text),
    }
    Ok(())
  }

  /// Refuses a revision with a flag, or one whose parent the fork does not hold.
  fn check_known(
    &self,
    log: Log<'_>,
    revision: &Revision<'_>,
    store: &ContentStore,
  ) -> Result<(), Unverified> {
    if revision.flags != 0 {
      return Err(Unverified::Flags {
        log: log_of(log),
        node: revision.node,
        flags: revision.flags,
      });
    }
    let unknown = [revision.p1, revision.p2]
      .into_iter()
      .find(|parent| *parent != NULL_NODE && self.fork.rev(store, log, parent).is_none());
    match unknown {
      Some(parent) => Err(Unverified::UnknownParent {
        log: log_of(log),
        node: revision.node,
        parent,
      }),
      None => Ok(()),
    }
  }

  /// The text of `revision`'s delta base, a changeset's or a manifest's (a file revision's is read in ranges, [`patched`]):
  /// empty for the null node, the text kept last where it is that one, one the store gave for the unit, else the fork's;
  /// `None` where the fork does not hold it.
  fn base_text<'s>(
    &'s self,
    log: Log<'_>,
    revision: &Revision<'_>,
    store: &'s ContentStore,
  ) -> Option<Cow<'s, [u8]>> {
    if revision.base == NULL_NODE {
      return Some(Cow::Borrowed(&[]));
    }
    if let Some((node, text)) = &self.last
      && *node == revision.base
    {
      return Some(Cow::Borrowed(text.as_slice()));
    }
    if let Some((_, text)) = self
      .logs_given
      .iter()
      .find(|(node, _)| *node == revision.base)
    {
      return Some(Cow::Borrowed(text.as_slice()));
    }
    let held = IndexHeld::new(&self.fork, store);
    let text = match log {
      Log::Changelog => held.changeset_text(&revision.base),
      Log::Manifest => held.manifest_text(&revision.base),
      Log::File(path) => held.file_text(path, &revision.base),
    }?;
    Some(Cow::Owned(text.into_owned()))
  }

  /// Reads `text` as `log`'s reader reads it, charging `admission` what noting it will hold: a changeset's manifest; a
  /// manifest's or a file revision's link a changeset the fork holds, a manifest's entries read, a file revision's block
  /// split. Nothing is noted until [`Taker::apply_read`] takes what it found.
  fn read_of(
    &self,
    log: Log<'_>,
    revision: &Revision<'_>,
    text: &[u8],
    store: &ContentStore,
    admission: &mut Admission,
  ) -> Result<ReadAs, Refused> {
    let node = revision.node;
    if log == Log::Changelog {
      let manifest = changeset::decode(text)
        .and_then(|read| read.manifest_node())
        .map_err(|fault| Unverified::Changeset { node, fault })?;
      admission.charge_items::<(Node, Node)>(2)?;
      return Ok(ReadAs::Changeset { node, manifest });
    }
    if self
      .fork
      .rev(store, Log::Changelog, &revision.link)
      .is_none()
    {
      return Err(
        Unverified::UnknownLink {
          log: log_of(log),
          node,
          link: revision.link,
        }
        .into(),
      );
    }
    match log {
      Log::File(path) => {
        filerev::split(text).map_err(|fault| Unverified::FileRevision {
          path: path.to_vec(),
          node,
          fault,
        })?;
      }
      _ => {
        manifest::decode(text).map_err(|fault| Unverified::Manifest { node, fault })?;
        return self.awaited_of(text, revision, (store, admission));
      }
    }
    Ok(ReadAs::File)
  }

  /// Notes what [`Taker::read_of`] read and charged: a changeset's manifest, a manifest's entries written.
  fn apply_read(&mut self, read: ReadAs) {
    match read {
      ReadAs::Changeset { node, manifest } => {
        self.manifest_of.push((node, manifest));
        self.intaken.changesets.push(node);
      }
      ReadAs::Manifest { awaited } => self.awaited.extend(awaited),
      ReadAs::File => {}
    }
  }

  /// The check of the new changeset at `at`: its manifest the null node, or one the fork holds.
  fn manifest_present(&mut self, at: usize, store: &ContentStore) -> Result<Stepped, Refused> {
    let Some((changeset, manifest)) = self.manifest_of.get(at).copied() else {
      self.stage = Stage::FilesPresent;
      return Ok(Stepped::More);
    };
    if manifest != NULL_NODE && self.fork.rev(store, Log::Manifest, &manifest).is_none() {
      return Err(
        Unverified::MissingManifest {
          changeset,
          manifest,
        }
        .into(),
      );
    }
    self.stage = Stage::ManifestsPresent(at.saturating_add(1));
    Ok(Stepped::More)
  }

  /// The files' check: each file revision the new manifests awaited ([`Taker::awaited`]) the fork holds now, given
  /// back as it is checked, its paths' bytes at most the content store's window a step (the store's unit, as a long
  /// revision is taken); every file taken, a revision still missing is refused.
  fn files_present(&mut self, store: &ContentStore) -> Result<Stepped, Refused> {
    let window = to_u64(store.window());
    let mut bytes = 0_u64;
    while bytes < window {
      let Some((path, node)) = self.awaited.pop_first() else {
        self.stage = Stage::Done;
        return Ok(Stepped::Done);
      };
      if self.fork.rev(store, Log::File(&path), &node).is_none() {
        return Err(Unverified::MissingFile { path, node }.into());
      }
      bytes = bytes.saturating_add(to_u64(path.len()));
    }
    Ok(Stepped::More)
  }

  /// What a new manifest's text `text` awaits ([`ReadAs::Manifest`]): of the entries its delta wrote
  /// (`written_entries`, a step's working set, charged to a copy of `admission`), each path audited, the file revisions
  /// the fork lacks and none before awaited, charged to `admission` as [`Taker::awaited`] keeps them. Refused where the
  /// manifest names a path that may not stand, or where what is awaited would pass what the push brings: a file
  /// revision neither held nor brought is missing.
  fn awaited_of(
    &self,
    text: &[u8],
    revision: &Revision<'_>,
    (store, admission): (&ContentStore, &mut Admission),
  ) -> Result<ReadAs, Refused> {
    let written = written_entries(text, revision).map_err(|fault| Unverified::Delta {
      log: LogOf::Manifests,
      node: revision.node,
      fault,
    })?;
    let mut working = *admission;
    working.charge_items::<(Vec<u8>, Node)>(written.len())?;
    let mut awaited = Vec::new();
    for (path, node) in written {
      audit(&path)?;
      let held = self.fork.rev(store, Log::File(&path), &node).is_some();
      if held || self.awaited.contains(&(path.clone(), node)) {
        continue;
      }
      if to_u64(self.awaited.len().saturating_add(awaited.len())) >= self.bringing {
        return Err(Unverified::MissingFile { path, node }.into());
      }
      admission.charge_items::<(Vec<u8>, Node)>(1)?;
      admission.charge(to_u64(path.len()))?;
      awaited.push((path, node));
    }
    Ok(ReadAs::Manifest { awaited })
  }
}

/// A push's fork re-based onto its repository's index as it is now (A-720 part 2b), when another push landed after the
/// fork was made: the fork's rows name their parents and links by number, so they hold only over the index they were
/// forked from. Each revision the push kept is appended again, a row a step, to a fork of the current index, from its
/// row in the stale fork (its node, parents and link by node, its flags, its length and the content it names: nothing
/// rebuilt, put or hashed again); one the index holds already (the other push brought it) is passed over, a changeset
/// among them counted held. Bookkeeping only: the push's verdict is still judged at its landing (D-27).
pub struct Rebase {
  /// The stale fork, read from.
  stale: HgIndex,
  /// The fork of the index as it is now, appended to.
  fork: HgIndex,
  /// The logs to re-append, each from its first kept number in the stale fork.
  logs: Vec<(LogOf, u32)>,
  /// The log and the stale number next.
  at: (usize, u32),
  /// Whether the log being re-appended has appended a row yet.
  logged: bool,
  /// What the push brings over the index as it is now.
  intaken: Intaken,
  /// A manifest's text not at hand, asked of the node's changeset store ([`Rebase::texts_ready`]).
  asked: Option<crate::push::held::TextAsk>,
  /// The manifests' texts the store gave for the row being re-appended: the one its chain rests on and the one the fork
  /// holds last, two at most, given up once the row is appended (goal 93, C1 (1); main's ruling of 2026-11-24 02:55:
  /// one way to read a text not at hand, everywhere).
  given: Vec<(Node, Vec<u8>)>,
}

impl crate::render::StoreWants for Rebase {
  fn wants(&self) -> Option<&crate::render::Want> {
    self
      .asked
      .as_ref()
      .and_then(crate::render::StoreWants::wants)
  }

  fn give_rows(&mut self, rows: Vec<Vec<Option<crate::render::StoredRevision>>>) {
    if let Some(asked) = self.asked.as_mut() {
      asked.give_rows(rows);
    }
  }

  fn give_chunks(&mut self, chunks: Vec<Vec<u8>>) {
    if let Some(asked) = self.asked.as_mut() {
      asked.give_chunks(chunks);
    }
  }

  fn give_texts(&mut self, texts: Vec<Vec<u8>>) {
    if let Some(asked) = self.asked.as_mut() {
      asked.give_texts(texts);
    }
  }
}

impl Rebase {
  /// Starts re-basing `stale` (what `intaken` names) onto a fork of `index`. Refused, `stale` comes back (boxed with the
  /// refusal: an index is large).
  pub fn begin(
    stale: HgIndex,
    intaken: &Intaken,
    index: &mut HgIndex,
    store: &mut ContentStore,
  ) -> Result<Self, Box<(Unindexed, HgIndex)>> {
    let fork = match index.fork(store) {
      Ok(fork) => fork,
      Err(refused) => return Err(Box::new((Unindexed::Fork(refused), stale))),
    };
    let logs = intaken.logs.clone();
    // The texts a push kept name its revisions' contents and their places in its payload, whatever index they are
    // re-based onto, so they carry over as they are.
    let intaken = Intaken {
      held_changesets: intaken.held_changesets.clone(),
      passed_over: intaken.passed_over,
      texts: intaken.texts.clone(),
      ..Intaken::default()
    };
    Ok(Rebase {
      stale,
      fork,
      logs,
      at: (0, 0),
      logged: false,
      intaken,
      asked: None,
      given: Vec::new(),
    })
  }

  /// Re-appends the next row (module docs); the caller gives both forks back on a refusal ([`Rebase::abandon`]).
  pub fn step(&mut self, store: &mut ContentStore) -> Result<Stepped, Unindexed> {
    let (at, rev) = self.at;
    let Some((named, first)) = self.logs.get(at).cloned() else {
      return Ok(Stepped::Done);
    };
    let log = log_named(&named);
    let rev = rev.max(first);
    if rev >= self.stale.len(store, log) {
      self.at = (at.saturating_add(1), 0);
      self.logged = false;
      return Ok(Stepped::More);
    }
    if let Some(stepped) = self.texts_ready(log, rev, store)? {
      return Ok(stepped);
    }
    self.again(log, &named, rev, store)?;
    self.given.clear();
    self.at = (at, rev.saturating_add(1));
    Ok(Stepped::More)
  }

  /// The ask of the node's changeset store the re-base waits on, by its path and node, if any: the tests' stand-in for
  /// the store reads it.
  #[cfg(test)]
  pub(crate) fn asked_text(&self) -> Option<(Vec<u8>, Node)> {
    self.asked.as_ref().map(|asked| {
      let (path, node) = asked.asked();
      (path.to_vec(), node)
    })
  }

  /// The tests' stand-in for the node's changeset store answering the ask with `text`, none where it keeps none.
  #[cfg(test)]
  pub(crate) fn answer_text(&mut self, text: Option<Vec<u8>>) {
    if let Some(asked) = self.asked.as_mut() {
      asked.answered(text);
    }
  }

  /// Whether the manifests' texts re-appending the stale fork's manifest `rev` reads are at hand (goal 93, C1 (1)): the
  /// one its chain rests on whose object was let go ([`crate::push::held::text_wanted`]), and the one the fork holds
  /// last, which the fork keeps it as a delta against. One not at hand is asked of the node's changeset store and the
  /// step taken again once given, nothing re-appended meanwhile: `Store`. None where the step may go on; refused where
  /// the store gave no text for the one asked.
  fn texts_ready(
    &mut self,
    log: Log<'_>,
    rev: u32,
    store: &ContentStore,
  ) -> Result<Option<Stepped>, Unindexed> {
    if log != Log::Manifest {
      return Ok(None);
    }
    if let Some(asked) = self.asked.take() {
      if crate::render::StoreWants::wants(&asked).is_some() {
        self.asked = Some(asked);
        return Ok(Some(Stepped::Store));
      }
      let (node, text) = asked
        .taken()
        .map(|(_, node, text)| (node, text))
        .ok_or(Unindexed::Index(crate::index::Refused::Unread { rev }))?;
      self.given.push((node, text));
    }
    let rests_on = crate::push::held::text_wanted((&self.stale, store), log, rev);
    let held_last = self
      .fork
      .len(store, log)
      .checked_sub(1)
      .and_then(|last| self.fork.revision(store, log, last))
      .and_then(|row| {
        let (len, at_hand) =
          crate::push::held::log_text_at_hand((&self.fork, store), log, &row.node)?;
        (!at_hand).then_some((row.node, len))
      });
    let wanted = [rests_on, held_last]
      .into_iter()
      .flatten()
      .find(|(node, _)| !self.given.iter().any(|(given, _)| given == node));
    let Some((node, most)) = wanted else {
      return Ok(None);
    };
    if held_last.is_some_and(|(last, _)| last == node)
      && rests_on.is_none_or(|(rests, _)| rests != node)
    {
      self.intaken.previous_manifests_asked =
        self.intaken.previous_manifests_asked.saturating_add(1);
    }
    self.asked = Some(crate::push::held::TextAsk::new(
      MANIFEST_PATH.to_vec(),
      node,
      most,
    ));
    self.intaken.log_texts_asked = self.intaken.log_texts_asked.saturating_add(1);
    Ok(Some(Stepped::Store))
  }

  /// The fork of the index as it is now, holding the push, and what it brings over that index; the stale fork given
  /// back.
  pub fn finish(self, store: &mut ContentStore) -> Result<(HgIndex, Intaken), IndexRefused> {
    let Rebase {
      stale,
      fork,
      intaken,
      ..
    } = self;
    match stale.release(store) {
      Ok(()) => Ok((fork, intaken)),
      Err(refused) => Err(fork.release(store).err().unwrap_or(refused)),
    }
  }

  /// Gives both forks back.
  pub fn abandon(self, store: &mut ContentStore) -> Result<(), IndexRefused> {
    let stale = self.stale.release(store);
    let fork = self.fork.release(store);
    stale.and(fork)
  }

  /// Appends the stale fork's revision `rev` of `log` to the fork, by node; passed over where the fork holds it.
  fn again(
    &mut self,
    log: Log<'_>,
    named: &LogOf,
    rev: u32,
    store: &mut ContentStore,
  ) -> Result<(), Unindexed> {
    let kept = crate::import::record::kept_over(&self.stale, store, log, rev, &self.given).ok_or(
      Unindexed::Index(crate::index::Refused::MissingLink { node: NULL_NODE }),
    )?;
    let node = kept.node;
    if self.fork.rev(store, log, &node).is_some() {
      self.intaken.passed_over = self.intaken.passed_over.saturating_add(1);
      if log == Log::Changelog {
        self.intaken.held_changesets.push(node);
      }
      return Ok(());
    }
    if !self.logged {
      self
        .intaken
        .logs
        .push((named.clone(), self.fork.len(store, log)));
      self.logged = true;
    }
    if log == Log::Changelog {
      let row = self.stale.changeset(store, rev).ok_or(Unindexed::Index(
        crate::index::Refused::MissingLink { node },
      ))?;
      let branch = row.branch.to_vec();
      let again = crate::index::NewChangeset {
        node,
        p1: kept.p1,
        p2: kept.p2,
        flags: kept.flags,
        len: kept.len,
        text: row.text,
        manifest: row.manifest,
        closed: row.closed,
        branch: &branch,
      };
      self.fork.append_changesets(store, &[again])?;
      self.intaken.changesets.push(node);
    } else {
      let row = self
        .stale
        .revision(store, log, rev)
        .ok_or(Unindexed::Index(crate::index::Refused::MissingLink {
          node,
        }))?;
      let again = crate::index::NewRevision {
        node,
        p1: kept.p1,
        p2: kept.p2,
        link: kept.link.unwrap_or(NULL_NODE),
        flags: kept.flags,
        len: kept.len,
        bytes: row.bytes,
        block: row.block,
      };
      // Kept by the intake's own rule (`push::manifest_row`, main's ruling (B')), so a re-based fork's rows are a fresh
      // intake's: as stored where its delta rests on the manifest the fork holds last and the chain allows; else its text
      // rebuilt over the stale fork's chain and kept as its diff against that last one, or whole.
      // A file revision is appended as stored: its bytes and block, whole.
      if log != Log::Manifest {
        self
          .fork
          .append_stored(store, log, &[(again, super::manifest_row::WHOLE)])?;
        self.intaken.kept = self.intaken.kept.saturating_add(1);
        return Ok(());
      }
      let last = super::manifest_row::last(&self.fork, store);
      let base = (row.base != crate::index::NONE)
        .then(|| {
          self
            .stale
            .revision(store, log, row.base)
            .map(|base| base.node)
        })
        .flatten();
      let as_sent = base
        .and_then(|base| super::manifest_row::as_sent(last.as_ref(), &base, row.stored, kept.len));
      match as_sent {
        Some(stored) => {
          self.fork.append_stored(store, log, &[(again, stored)])?;
        }
        None => {
          let text = self
            .stale
            .revision_text_over(store, log, rev, &self.given)
            .ok_or(Unindexed::Index(crate::index::Refused::Unread { rev }))?;
          super::manifest_row::kept_from_text(&mut self.fork, store, (again, &text), &self.given)?;
        }
      }
    }
    self.intaken.kept = self.intaken.kept.saturating_add(1);
    Ok(())
  }
}

/// The index's log `named` names.
fn log_named(named: &LogOf) -> Log<'_> {
  match named {
    LogOf::Changelog => Log::Changelog,
    LogOf::Manifests => Log::Manifest,
    LogOf::File(path) => Log::File(path),
  }
}

/// The changesets `intaken` brought as their rows in `fork` keep them (A-720 part 2): node, parents, branch and close,
/// for classifying the push without its texts; each charged to `admission`. `None` where the fork lacks one.
pub fn read_changesets(
  fork: &HgIndex,
  store: &ContentStore,
  intaken: &Intaken,
  admission: &mut Admission,
) -> Result<Option<Vec<ReadChangeset>>, crate::PastAdmission> {
  admission.charge_items::<ReadChangeset>(intaken.changesets.len())?;
  let mut read = Vec::with_capacity(intaken.changesets.len());
  let node_of = |number: u32| match number {
    NONE => Some(NULL_NODE),
    number => fork.changeset(store, number).map(|row| row.node),
  };
  for node in &intaken.changesets {
    let Some(row) = fork
      .rev(store, Log::Changelog, node)
      .and_then(|rev| fork.changeset(store, rev))
    else {
      return Ok(None);
    };
    admission.charge(to_u64(row.branch.len()))?;
    let (Some(p1), Some(p2)) = (node_of(row.p1), node_of(row.p2)) else {
      return Ok(None);
    };
    read.push(ReadChangeset {
      node: *node,
      p1,
      p2,
      branch: row.branch.to_vec(),
      closes: row.closed,
    });
  }
  Ok(Some(read))
}

/// The step that rebuilds a revision's text: its delta applied to its base's text (module docs: the step a later
/// storage removes from the keeping), charged to `room`.
fn rebuilt(
  base: Option<&[u8]>,
  revision: &Revision<'_>,
  log: Log<'_>,
  (room, mut into): (&mut Admission, Vec<u8>),
) -> Result<Vec<u8>, Unverified> {
  let base = base.ok_or_else(|| Unverified::UnknownDeltaBase {
    log: log_of(log),
    node: revision.node,
    base: revision.base,
  })?;
  room.charge(to_u64(base.len()))?;
  // Into `into`'s buffer, its capacity kept: `apply_into` clears it and reserves only what it lacks.
  delta::apply_into(base, revision.data, &mut into, room).map_err(|fault| Unverified::Delta {
    log: log_of(log),
    node: revision.node,
    fault,
  })?;
  Ok(into)
}

/// The entries of manifest `text` its delta wrote (main's ruling of 2026-11-21 05:12 (a)): the whole lines each hunk's
/// bytes fall on in the new text, every line where the revision came whole or against the null base. O(the lines the
/// delta touched), never the whole manifest: a delta that rewrote nothing in a line leaves it out, and a hunk cut
/// mid-line takes the whole line in. `text` is the delta applied to its base, so the hunks are known to apply.
fn written_entries(
  text: &[u8],
  revision: &Revision<'_>,
) -> Result<Vec<(Vec<u8>, Node)>, delta::Refusal> {
  let entries_of = |lines: &[u8]| -> Vec<(Vec<u8>, Node)> {
    lines
      .split(|byte| *byte == b'\n')
      .filter_map(manifest::parse)
      .map(|entry| (entry.path.to_vec(), entry.node))
      .collect()
  };
  if revision.is_full_text() || revision.base == NULL_NODE {
    return Ok(entries_of(text));
  }
  let line_start = |at: usize| {
    text
      .get(..at)
      .and_then(|before| before.iter().rposition(|byte| *byte == b'\n'))
      .map_or(0, |newline| newline.saturating_add(1))
  };
  let line_end = |at: usize| {
    text
      .get(at..)
      .and_then(|after| after.iter().position(|byte| *byte == b'\n'))
      .map_or(text.len(), |newline| {
        at.saturating_add(newline).saturating_add(1)
      })
  };
  let mut written = Vec::new();
  let (mut produced, mut base_at, mut covered) = (0_usize, 0_usize, 0_usize);
  for hunk in delta::hunks(revision.data) {
    let hunk = hunk?;
    let start = usize::try_from(hunk.start).unwrap_or(usize::MAX);
    let end = usize::try_from(hunk.end).unwrap_or(usize::MAX);
    let new_start = produced.saturating_add(start.saturating_sub(base_at));
    let new_end = new_start.saturating_add(hunk.data.len());
    produced = new_end;
    base_at = end;
    if new_start >= text.len() {
      continue;
    }
    let lo = line_start(new_start).max(covered);
    let hi = line_end(new_end.saturating_sub(1).max(new_start));
    if lo < hi {
      written.extend(entries_of(text.get(lo..hi).unwrap_or_default()));
      covered = hi;
    }
  }
  Ok(written)
}

/// Verification's name of `log`.
fn log_of(log: Log<'_>) -> LogOf {
  match log {
    Log::Changelog => LogOf::Changelog,
    Log::Manifest => LogOf::Manifests,
    Log::File(path) => LogOf::File(path.to_vec()),
  }
}

#[path = "intake_patched.rs"]
mod patched;

#[path = "intake_manifest.rs"]
mod manifesting;

#[cfg(test)]
#[path = "intake_tests.rs"]
mod tests;
