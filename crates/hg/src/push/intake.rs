//! A push's changegroup taken into a fork of its repository's index a revision at a time (A-720; §4.17.3 "Push";
//! §4.17.1's rule for a push: its working set bounded whatever its size, its content stored once as it arrives): the
//! sliced intake a server advances within its shard's step budget, in place of verifying the whole changegroup first
//! ([`super::verify::verify`], every arrived text held at once) and keeping it after ([`super::commit::fork_with`]).
//!
//! **A step** takes one unit: one revision, in 7.2's order (the changelog's group, the manifest log's, then each file
//! log's), or one of the checks after them. A revision the log holds, or the push brought earlier, is passed over, as
//! 7.2's `addgroup` passes it over. Any other is checked as verification checks it (its flags, its parents known), its
//! text rebuilt from its delta against its base (the step [`rebuilt`]: the text before it in its group where its delta
//! names that one, as 7.2's revision cache serves it, else the base's text read back from the fork), its node held to
//! its text, its text read as its log's reader reads it (a changeset's manifest, a manifest's entries, a file
//! revision's metadata block; a manifest's and a file revision's link a changeset the fork holds), and then kept: its
//! text put into the content store once and its row appended to the fork ([`super::commit::keep_one`]). The fork holds
//! the repository's revisions and the push's, so a later revision's parents and base are found there. After the logs,
//! each new changeset's manifest is held to be present, then each new manifest's changed files, a manifest a step.
//!
//! **Bounded.** What a step holds is its revision's delta, the base's text and the rebuilt text, charged to a copy of
//! the push's admission, since it is let go when the step ends; only the text before it is kept to the next step. What
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
use std::collections::{BTreeMap, BTreeSet, VecDeque};

use slates_vcs::{ContentId, ContentStore, IndexRefused};

use super::classify::ReadChangeset;
use super::commit::{Unindexed, keep_one};
use super::held::IndexHeld;
use super::verify::{Arrived, Held, LogOf, Unverified, audit};
use crate::changegroup::{Revision, Version};
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
  /// The file delta bases found in the push's cache of its own texts ([`BaseCache`]; A-767 C1, main's ruling (b2)).
  pub base_hits: u64,
  /// The file revisions rebuilt from the push's own deltas to make a base the cache did not hold ((b1), counted).
  pub base_rebuilds: u64,
  /// The file delta bases read from the node's changeset store between steps: a revision landed before the push.
  pub base_store_reads: u64,
  /// The most text bytes one step hashed and kept (A-767 C1, the push spin): a revision longer than the content
  /// store's window is taken over as many steps as it has windows ([`Pending`]), so no step's work grows with a file.
  pub longest_step_bytes: u64,
  /// The revisions taken over more than one step, a window a step ([`Pending`]).
  pub spread: u64,
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
  /// The new manifest at this position: its changed files present.
  FilesPresent(usize),
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

/// A new manifest and the entries its delta wrote: each file's path and file revision.
type ManifestWrites = (Node, Vec<(Vec<u8>, Node)>);
/// Format: the fewest bytes a manifest's line takes: a path of one byte, its NUL, a node's forty hex digits and the line's
/// end. A delta's bytes write at most one entry for each this many of them ([`Intake::bookkeeping`]).
const LEAST_MANIFEST_LINE: u64 = 1 + 1 + 40 + 1;

impl Intake {
  /// The most its steps keep charged to the push's admission from the first to the last (main 01a20182's ruling (a) of
  /// 2026-11-22 04:23: a push's need is known at its begin, so it is admitted it up front, never the learned demand that
  /// predicts an unknown one): told from its framing before any revision is taken, by the charges the steps make
  /// (`Taker::keep_taken`, `read_of`, `logged_of`). Each revision its node, its kept text and its path; each changeset
  /// its manifest's and its own nodes noted; each manifest its entry, at most one written entry for each
  /// [`LEAST_MANIFEST_LINE`] bytes of its delta and the delta's bytes for their paths; each log its name, a file's its
  /// path. A step's own working set (its delta, its base, its rebuilt text) is apart: charged to a copy and let go. Every
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
    let manifests = framed
      .revisions
      .get(framed.manifests.clone())
      .unwrap_or_default()
      .iter()
      .fold(
        kept(framed.manifests.len(), to_u64(MANIFEST_PATH.len())),
        |total, placed| {
          let data = placed.data.end.saturating_sub(placed.data.start);
          let entries = data
            .checked_div(LEAST_MANIFEST_LINE)
            .unwrap_or(data)
            .saturating_add(1);
          total
            .saturating_add(to_u64(size_of::<ManifestWrites>()))
            .saturating_add(entries.saturating_mul(to_u64(size_of::<(Vec<u8>, Node)>())))
            .saturating_add(data)
        },
      );
    let logs = to_u64(2usize.saturating_add(framed.files.len()))
      .saturating_mul(to_u64(size_of::<(LogOf, u32)>()));
    let files = framed.files.iter().fold(0u64, |total, (name, revisions)| {
      let path = name.end.saturating_sub(name.start);
      total
        .saturating_add(kept(revisions.len(), path))
        .saturating_add(path)
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
  /// The unit next.
  stage: Stage,
  /// The nodes the push brought in the log being taken, to tell a revision brought earlier from one held.
  brought: BTreeSet<Node>,
  /// Each new changeset and the manifest its text names.
  manifest_of: Vec<(Node, Node)>,
  /// Each new manifest and the entries its delta wrote (each file's path and file revision), which its files' check
  /// reads ([`Taker::files_present`]): what the push changed, never a whole manifest.
  manifests: Vec<ManifestWrites>,
  /// The text kept last, and its node: the next revision's base where its delta names it.
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
  /// Each file revision the push brought in the group being taken, by node: its place in the group, from which its
  /// text is rebuilt where a later delta names it as its base ((b1)).
  positions: BTreeMap<Node, usize>,
  /// The push's own file texts kept for later deltas' bases ((b2)), let go as each file group ends.
  cache: BaseCache,
  /// A file delta's base asked of the store, and its answer.
  asked: Option<StoreAsk>,
  /// A revision longer than one window, being taken a window a step.
  pending: Option<Box<Pending>>,
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
}

impl Cutting {
  /// Notes `kept`, revision `rev` of `log` in the fork, its text charged `charged`; cuts the segment once its slice
  /// reaches the bound.
  fn kept(&mut self, log: LogOf, rev: u32, kept: KeptText, charged: u64) {
    let open = self.open.get_or_insert_with(|| Open {
      slice: kept.place.start..kept.place.end,
      ranges: Vec::new(),
      texts: Vec::new(),
      charged: 0,
    });
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
  /// A file revision's position in its group, for a later delta's base.
  position: Option<usize>,
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

/// The push's own file texts a later delta may name as its base (A-767 C1, main's ruling (b2)): the texts kept before
/// the one kept last, oldest out first, at most the room the push was admitted (derived: what one step of the push may
/// hold, so the intake's working set is at most twice its room), let go as each file group ends and with the push. A
/// miss is rebuilt from the push's own deltas ((b1)), the one source; the cache only saves the rebuild.
#[derive(Debug, Default)]
struct BaseCache {
  /// The texts, by node.
  held: BTreeMap<Node, Vec<u8>>,
  /// Their order of arrival, oldest first.
  order: VecDeque<Node>,
  /// The bytes they hold.
  bytes: u64,
  /// The most bytes they may hold.
  bound: u64,
}

impl BaseCache {
  /// The cache of a push admitted `room` bytes.
  fn within(room: u64) -> BaseCache {
    BaseCache {
      bound: room,
      ..BaseCache::default()
    }
  }

  /// Keeps `text` as `node`'s, letting the oldest go until it fits; a text past the bound alone is not kept.
  fn put(&mut self, node: Node, text: Vec<u8>) {
    let len = to_u64(text.len());
    if len > self.bound || self.held.contains_key(&node) {
      return;
    }
    while self.bytes.saturating_add(len) > self.bound {
      let Some(oldest) = self.order.pop_front() else {
        break;
      };
      if let Some(gone) = self.held.remove(&oldest) {
        self.bytes = self.bytes.saturating_sub(to_u64(gone.len()));
      }
    }
    self.bytes = self.bytes.saturating_add(len);
    self.order.push_back(node);
    self.held.insert(node, text);
  }

  /// Lets every text go.
  fn clear(&mut self) {
    self.held.clear();
    self.order.clear();
    self.bytes = 0;
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
  /// A manifest, and the entries it writes beside its first parent's.
  Manifest {
    /// The manifest's node.
    node: Node,
    /// The entries written: each path and its file node.
    written: Vec<(Vec<u8>, Node)>,
  },
  /// A file revision: nothing to note.
  File,
}

/// What a file delta's base came to ([`Taker::file_base`]).
enum Based {
  /// Its text.
  Text(Vec<u8>),
  /// The changegroup's bytes the rebuild needs: put them in and step again.
  Wanted(Range<u64>),
  /// The store is asked: answer it and step again.
  Store,
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
          log: LogOf::File(asked.path.clone()),
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
        log: LogOf::File(asked.path.clone()),
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
  /// The intake with its cache of the push's own texts bounded at `bound` bytes ((b2)): zero leaves (b1) alone, the
  /// A/B's other arm and a test's rebuilds.
  #[cfg(test)]
  pub(crate) fn with_base_cache(mut self, bound: u64) -> Self {
    self.taker.cache = BaseCache::within(bound);
    self
  }

  /// Answers the store's want from the fork's own texts, as a test's stand-in for the node's changeset store (the tests
  /// keep no store): the base the step asked for, read back from the fork's content store.
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
          log: LogOf::File(asked.path.clone()),
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
    Ok(Intake {
      frame,
      taker: Taker {
        fork,
        stage: Stage::Changelog(0),
        brought: BTreeSet::new(),
        manifest_of: Vec::new(),
        manifests: Vec::new(),
        last: None,
        last_identity: None,
        logged: false,
        path: None,
        intaken: Intaken::default(),
        positions: BTreeMap::new(),
        cache: BaseCache::within(room),
        asked: None,
        pending: None,
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
    });
    self
  }

  /// The next segment cut, oldest first; none while the one being cut is short of its bound.
  pub fn take_segment(&mut self) -> Option<PushSegment> {
    self.taker.cutting.as_mut()?.ready.pop_front()
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
        let after = Stage::Files { file: 0, at: 0 };
        taker.take_or(next, Log::Manifest, (store, admission), after)
      }
      Stage::Files { file, at } => taker.file_step((frame, payload), (file, at), store, admission),
      Stage::ManifestsPresent(at) => taker.manifest_present(at, store),
      Stage::FilesPresent(at) => taker.files_present(at, store),
      Stage::Done => Ok(Stepped::Done),
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
    self.taker.fork.release(store)
  }
}

impl Taker {
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
    // A revision longer than a window is taken over the steps after this one ([`Pending`]), which move the stage on.
    if self.take(
      log,
      (&revision, place),
      None,
      (store, admission),
      (after, None),
    )? {
      self.stage = after;
    }
    Ok(Stepped::More)
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
    // A file delta's base other than the text kept last: rebuilt from the push's own deltas, read from its cache, or
    // read from the store between steps for a revision landed before the push (A-767 C1).
    let kept_before = self.brought.contains(&revision.node)
      || self
        .fork
        .rev(store, Log::File(&path), &revision.node)
        .is_some();
    let names_last = self
      .last
      .as_ref()
      .is_some_and(|(node, _)| *node == revision.base);
    let given = if kept_before || revision.base == NULL_NODE || names_last {
      None
    } else {
      let mut room = *admission;
      match self.file_base(
        (frame, payload),
        (&path, &revisions),
        revision.base,
        (store, &mut room),
      ) {
        Ok(Based::Text(text)) => Some(text),
        Ok(Based::Wanted(range)) => {
          self.path = Some(path);
          return Ok(Stepped::Wanted(range));
        }
        Ok(Based::Store) => {
          self.path = Some(path);
          return Ok(Stepped::Store);
        }
        Err(refused) => {
          self.path = Some(path);
          return Err(refused);
        }
      }
    };
    let after = Stage::Files {
      file,
      at: at.saturating_add(1),
    };
    let position = (!kept_before).then_some(at);
    let taken = self.take(
      Log::File(&path),
      (&revision, place),
      given,
      (store, admission),
      (after, position),
    );
    self.path = Some(path);
    // A revision longer than a window is taken over the steps after this one ([`Pending`]), which move the stage on.
    if taken? {
      if let Some(position) = position {
        self.positions.insert(revision.node, position);
      }
      self.stage = after;
    }
    Ok(Stepped::More)
  }

  /// Moves to `stage`, a new log's: what the last one brought is let go.
  fn enter(&mut self, stage: Stage) {
    self.path = None;
    self.positions.clear();
    self.cache.clear();
    self.asked = None;
    self.brought.clear();
    self.last = None;
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

  /// Takes one revision of `log` (module docs): passed over, or checked, rebuilt and kept.
  fn take(
    &mut self,
    log: Log<'_>,
    (revision, place): (&Revision<'_>, Range<u64>),
    given: Option<Vec<u8>>,
    (store, admission): (&mut ContentStore, &mut Admission),
    (after, position): (Stage, Option<usize>),
  ) -> Result<bool, Refused> {
    if self.already_brought(log, revision.node, store, admission)? {
      return Ok(true);
    }
    self.check_known(log, revision, store)?;
    let mut room = *admission;
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
    let text = rebuilt(base_text, revision, log, &mut room)?;
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
      self.begin_pending(log, revision, (text, base, place), (after, position))?;
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
      (None, None),
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
    (after, position): (Stage, Option<usize>),
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
      position,
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
      position,
      ..
    } = *pending;
    let log = log.log();
    checking.finish().map_err(|fault| Unverified::Node {
      log: log_of(log),
      node,
      fault,
    })?;
    let kept = match writing {
      Some(mut writing) => Some(
        store
          .finish_object(&mut writing)
          .map_err(|refused| Refused::Unindexed(Unindexed::from(refused)))?,
      ),
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
      (kept, Some(identity)),
      (store, admission),
    )?;
    if let Some(position) = position {
      self.positions.insert(node, position);
    }
    self.stage = after;
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
    (kept, identity): (Option<ContentId>, Option<[u8; 32]>),
    (store, admission): (&mut ContentStore, &mut Admission),
  ) -> Result<(), Refused> {
    // Every charge first, to a trial of the admission, then the keep, then every note and the trial taken: a refusal
    // anywhere leaves the intake where it was and the admission unspent, so the step is taken again whole once its room
    // has grown (main 01a20182's ruling of 2026-11-22 03:03: an intake past its admission waits, never refused).
    let mut trial = *admission;
    let read = self.read_of(log, revision, &text, store, &mut trial)?;
    let logged = self.logged_of(log, store, &mut trial)?;
    let path = match log {
      Log::Changelog => CHANGELOG_PATH,
      Log::Manifest => MANIFEST_PATH,
      Log::File(path) => path,
    };
    trial.charge_items::<Node>(1)?;
    trial.charge_items::<KeptText>(1)?;
    trial.charge(to_u64(path.len()))?;
    let mut room = *admission;
    let arrived = Arrived {
      node: revision.node,
      p1: revision.p1,
      p2: revision.p2,
      link: revision.link,
      text,
    };
    let previous = self
      .last
      .as_ref()
      .map(|(node, text)| (node, text.as_slice()));
    keep_one(
      &mut self.fork,
      store,
      log,
      (&arrived, previous),
      kept,
      &mut room,
    )?;
    *admission = trial;
    self.apply_read(read);
    self.note_logged(logged);
    self.brought.insert(revision.node);
    self.intaken.kept = self.intaken.kept.saturating_add(1);
    self.intaken.bytes = self
      .intaken
      .bytes
      .saturating_add(to_u64(arrived.text.len()));
    let identity = identity.unwrap_or_else(|| *blake3::hash(&arrived.text).as_bytes());
    let kept = KeptText {
      path: path.to_vec(),
      node: revision.node,
      identity,
      len: to_u64(arrived.text.len()),
      base,
      place,
    };
    match self.cutting.as_mut() {
      Some(cutting) => {
        let rev = u32::try_from(self.fork.len(store, log).saturating_sub(1)).unwrap_or(u32::MAX);
        let charged = to_u64(size_of::<KeptText>()).saturating_add(to_u64(path.len()));
        cutting.kept(log_of(log), rev, kept, charged);
      }
      None => self.intaken.texts.push(kept),
    }
    self.last_identity = Some((revision.node, identity));
    // The text kept before this one may be a later delta's base: the push's cache keeps it ((b2)).
    if let (Log::File(_), Some((node, text))) = (log, self.last.take()) {
      self.cache.put(node, text);
    }
    self.last = Some((revision.node, arrived.text));
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

  /// The text of file delta base `base` (A-767 C1, main's ruling (b2) over (b1)): from the push's cache where it holds
  /// it; else rebuilt from the push's own deltas, walked back from `base` through the revisions the group brought to a
  /// text in hand (the null base, the text kept last, the cache's, or a revision landed before the push, read from the
  /// store between steps), each rebuilt text kept in the cache; refused where the base is none of those.
  fn file_base<S: Source + ?Sized>(
    &mut self,
    (frame, payload): (&Frame, &S),
    (path, revisions): (&[u8], &Range<usize>),
    base: Node,
    (store, room): (&ContentStore, &mut Admission),
  ) -> Result<Based, Refused> {
    let unknown = |node: Node| {
      Refused::Unverified(Unverified::UnknownDeltaBase {
        log: LogOf::File(path.to_vec()),
        node,
        base,
      })
    };
    let mut chain: Vec<usize> = Vec::new();
    let mut at = base;
    let start = loop {
      if at == NULL_NODE {
        break Vec::new();
      }
      if let Some((node, text)) = &self.last
        && *node == at
      {
        break text.clone();
      }
      if let Some(text) = self.cache.held.get(&at) {
        self.intaken.base_hits = self.intaken.base_hits.saturating_add(1);
        break text.clone();
      }
      if let Some(index) = self.positions.get(&at).copied() {
        let revision = match frame.placed(payload, revisions, index) {
          Ok(Some((revision, _))) => revision,
          Ok(None) => return Err(unknown(at)),
          Err(wanted) => return Ok(Based::Wanted(wanted.range)),
        };
        room.charge_items::<usize>(1)?;
        chain.push(index);
        at = revision.base;
        continue;
      }
      if self.fork.rev(store, Log::File(path), &at).is_none() {
        return Err(unknown(at));
      }
      // Landed before the push: its text read from the store between steps.
      match self.asked.take() {
        Some(asked) if asked.node == at && asked.path == path => {
          if let Some(refused) = asked.refused {
            return Err(refused);
          }
          match asked.text {
            Some(text) => {
              self.intaken.base_store_reads = self.intaken.base_store_reads.saturating_add(1);
              room.charge(to_u64(text.len()))?;
              break text;
            }
            None => {
              self.asked = Some(asked);
              return Ok(Based::Store);
            }
          }
        }
        _ => {
          self.asked = Some(StoreAsk {
            path: path.to_vec(),
            node: at,
            want: Some(crate::render::Want::Rows {
              logs: vec![(path.to_vec(), vec![at])],
            }),
            most: room.left(),
            text: None,
            refused: None,
          });
          return Ok(Based::Store);
        }
      }
    };
    let mut text = start;
    while let Some(index) = chain.pop() {
      let revision = match frame.placed(payload, revisions, index) {
        Ok(Some((revision, _))) => revision,
        Ok(None) => return Err(unknown(at)),
        Err(wanted) => return Ok(Based::Wanted(wanted.range)),
      };
      room.charge(to_u64(text.len()))?;
      text = delta::apply(&text, revision.data, room).map_err(|fault| Unverified::Delta {
        log: LogOf::File(path.to_vec()),
        node: revision.node,
        fault,
      })?;
      self.intaken.base_rebuilds = self.intaken.base_rebuilds.saturating_add(1);
      self.cache.put(revision.node, text.clone());
    }
    Ok(Based::Text(text))
  }

  /// The text of `revision`'s delta base: empty for the null node, the text kept last where it is that one, else the
  /// fork's; `None` where the fork does not hold it.
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
        let written = written_entries(text, revision).map_err(|fault| Unverified::Delta {
          log: log_of(log),
          node,
          fault,
        })?;
        admission.charge_items::<(Node, Vec<(Vec<u8>, Node)>)>(1)?;
        admission.charge_items::<(Vec<u8>, Node)>(written.len())?;
        admission.charge(written.iter().fold(0_u64, |bytes, (path, _)| {
          bytes.saturating_add(to_u64(path.len()))
        }))?;
        return Ok(ReadAs::Manifest { node, written });
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
      ReadAs::Manifest { node, written } => self.manifests.push((node, written)),
      ReadAs::File => {}
    }
  }

  /// The check of the new changeset at `at`: its manifest the null node, or one the fork holds.
  fn manifest_present(&mut self, at: usize, store: &ContentStore) -> Result<Stepped, Refused> {
    let Some((changeset, manifest)) = self.manifest_of.get(at).copied() else {
      self.stage = Stage::FilesPresent(0);
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

  /// The check of the new manifest at `at`: every entry its delta wrote audited and held by the fork's file log (main's
  /// ruling of 2026-11-21 05:12 (a)). An entry its delta left as its base had it was checked when that base was: held by
  /// the fork before the push, or checked earlier in it. Its text and its first parent's were rebuilt from their chains
  /// here before, two whole texts a manifest: 57% of hg-src's first push (w-perf's profile, base 94c1696bb).
  fn files_present(&mut self, at: usize, store: &ContentStore) -> Result<Stepped, Refused> {
    let Some((_, written)) = self.manifests.get(at) else {
      self.stage = Stage::Done;
      return Ok(Stepped::Done);
    };
    for (path, node) in written {
      audit(path)?;
      if self.fork.rev(store, Log::File(path), node).is_none() {
        return Err(
          Unverified::MissingFile {
            path: path.clone(),
            node: *node,
          }
          .into(),
        );
      }
    }
    self.stage = Stage::FilesPresent(at.saturating_add(1));
    Ok(Stepped::More)
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
    self.again(log, &named, rev, store)?;
    self.at = (at, rev.saturating_add(1));
    Ok(Stepped::More)
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
    let kept = crate::import::record::kept(&self.stale, store, log, rev).ok_or(
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
      // A manifest kept as a delta against the stale fork's previous one (A-729) is appended again by its text, which
      // the fork keeps as its own delta against its previous one.
      let rebuilt = if row.base == crate::index::NONE {
        None
      } else {
        let text = self
          .stale
          .revision_text(store, log, rev)
          .ok_or(Unindexed::Index(crate::index::Refused::Unread { rev }))?;
        Some(store.put_object(&text)?)
      };
      let again = crate::index::NewRevision {
        node,
        p1: kept.p1,
        p2: kept.p2,
        link: kept.link.unwrap_or(NULL_NODE),
        flags: kept.flags,
        len: kept.len,
        bytes: rebuilt.unwrap_or(row.bytes),
        block: row.block,
      };
      let appended = self.fork.append_revisions(store, log, &[again]);
      if let Some(put) = rebuilt {
        store.release(&put)?;
      }
      appended?;
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
  base: Option<Cow<'_, [u8]>>,
  revision: &Revision<'_>,
  log: Log<'_>,
  room: &mut Admission,
) -> Result<Vec<u8>, Unverified> {
  let base = base.ok_or_else(|| Unverified::UnknownDeltaBase {
    log: log_of(log),
    node: revision.node,
    base: revision.base,
  })?;
  room.charge(to_u64(base.len()))?;
  delta::apply(&base, revision.data, room).map_err(|fault| Unverified::Delta {
    log: log_of(log),
    node: revision.node,
    fault,
  })
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

#[cfg(test)]
#[path = "intake_tests.rs"]
mod tests;
