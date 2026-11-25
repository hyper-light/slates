//! A Mercurial repository's directory imported into its store on its owner shard (A-675; §4.17.3 piece 3; §4.15; §4.3;
//! A-649 parts 3b and 4; A-662), and made durable by the register's entry it lands (A-687).
//!
//! **What it does.** A Mercurial repository's store is made with its record as an empty index (`HgStore`). An import
//! gives it the history of a repository on the host: the repository's directory is read as a base, read-only (§4.15),
//! through the server's adapter (`crate::hg_base`), and `slates-hg`'s import (`Import`) is advanced a turn at a time on
//! the repository's owner shard ([`turn`], [`run`]), each turn lent the host and the shard's content store, within the
//! shard's quantum (§4.3's step budget) on the shard's clock, with a yield between turns. Once it is done, its host is
//! closed and what it found is answered ([`Finished`]: its counts, phase roots and bookmarks), and it waits in the map,
//! holding its index, until it lands ([`land`]). Refused, abandoned, or met at its landing by a store that is no longer
//! empty, it gives back everything it held, and the store is as it was.
//!
//! **Durable** (A-687; §4.17.1 "Durability and residency"; R8). An import lands as one entry of the repository's logs
//! (A-635's append, `repository::push::push_imported`), in its caller's transaction, which records the request's
//! completion beside it (`repository::import`): the entry's objects are the import's record (`slates_hg::import::record`:
//! every revision it kept, whole, and the markers), so a recovery, or a successor that cannot read the importing host's
//! directory, replays it into the same index (the map's `replay_hg`); its state is the phase roots, each added to the
//! phases log (§4.17.3 decision 3); and each bookmark is a green bound to `bookmark:<label>:` (decision 2), made from
//! the repository's root at its head and moved to the bookmark's changeset by a work A-618's builder makes from the
//! imported index, as a push makes a bookmark's (A-657). The store takes the index between the register's guard and the
//! entry's apply ([`place_imported`]), as a git push's store commits its objects (A-662), so an entry the guard refuses
//! never reaches the store, and an apply refused after it makes the store again from the log. A line for each branch
//! head (decision 1) is not made here: owed (GAPS), so a push after an import finds the root as its first line.
//!
//! **Bounds.** An import holds, between its turns as within them, at most its room (`Admission`): the listing, the log
//! it reads (an inline revlog whole), a text being rebuilt and the text before it. The room is what the shard could admit
//! when the import starts (A-720 part 3; it was one client region's bytes, 278,528 B, which refused Mercurial's own
//! repository), a bound only: the shard is charged as retention what the import holds between turns
//! (`Import::holding`), trued up after each turn and given back when it is done, so the charge covers what the import
//! holds while it waits.
//! What the import keeps in the content store is charged as every store's is (the map's module doc): each turn is lent
//! every byte the shard could admit now, then the arena is grown from its pool by what the turn kept and the charge
//! trued up to what the content store holds. One import a repository at a time, so the imports under way or waiting to
//! land are bounded by the partition's repositories. Each holds its own host, which its caller opened for the directory
//! and which is closed when the import is done; the adapter's lend, made and ended within each turn, holds at most a
//! directory and two files. Its landing holds the record, whole, and the bookmarks' works, each within what the room
//! leaves of the others; the record is then charged to the partition's merge budget with the entry, as a git push's is.
//!
//! **Cancellation.** The import lives in the shard's map, keyed by its repository, and the task that runs it holds the
//! key alone, so a task dropped mid-import leaks nothing: the import waits in the map, holding what it held, until a
//! task runs or lands it, or its repository is freed, which gives it back ([`abandon`]).
//!
//! **Owed** (GAPS' §4.17.3 entry). The lines of an imported repository's branch heads (decision 1); a host's pending
//! fetch retried at the next turn rather than ending the import; the adapter's handles held between turns beside the
//! import; and, for a large repository, the bookmarks' works made in slices rather than whole at the landing (the record
//! is written a part a turn before it since A-732, `Stage::Writing`), and the snapshot carrying the store's tables, so a
//! recovery replays only the log's suffix.

use std::collections::BTreeMap;

use slates_db::catalog::{
  MemberOrigin, Principal, RepositoryId, StateChange, VolumeId as DbVolumeId,
};
use slates_hg::import::{Clock, Import, ImportRefused, Imported, Progress, Turn};
use slates_hg::index::HgIndex;
use slates_hg::phases::PhaseHead;
use slates_hg::push::held::IndexHeld;
use slates_hg::push::tree::{self, LayoutRefusal, Move, WorkRefused};
use slates_hg::push::verify::Verified;
use slates_hg::{Admission, Node};
use slates_ipc::protocol::{ImportFault, Refusal};
use slates_merge::push::{HEAD_ATTRIBUTE, PushRefused, ROOT};
use slates_vcs::ContentStore;
use slates_vfs::host::{HostDir, HostFs};

use super::{RELEASE_REFUSED, RepositoryStore, VcsStores, lend, settle};
use crate::hg_base::{BaseRefused, HgBase};
use crate::repository::push::{AppendRefused, BranchChange, ChangeRefusal, EntryParts};
use crate::state::ShardState;

#[cfg(test)]
pub(crate) mod tests;

/// Counter: an import started, its requirements and listing read. Format: a counter name in the status report.
pub(crate) const STARTED: &str = "vcs_store.hg_import.started";
/// Counter: a turn of an import; more turns than imports shows the imports ran a turn at a time. Format: a counter name.
pub(crate) const TURNS: &str = "vcs_store.hg_import.turns";
/// Counter: an import done, waiting to land. Format: a counter name.
pub(crate) const DONE: &str = "vcs_store.hg_import.done";
/// Counter: an import refused midway, everything it held given back. Format: a counter name.
pub(crate) const REFUSED: &str = "vcs_store.hg_import.refused";
/// Counter: an import given back before it landed: its repository freed. Format: a counter name.
pub(crate) const ABANDONED: &str = "vcs_store.hg_import.abandoned";
/// Counter: an import landed: its entry appended and its index in its store's place (A-687). Format: a counter name.
pub(crate) const LANDED: &str = "vcs_store.hg_import.landed";
/// Counter: an import whose landing was refused, its index given back (A-687). Format: a counter name.
pub(crate) const LANDING_REFUSED: &str = "vcs_store.hg_import.landing_refused";

/// A repository's import, kept in the shard's map between its turns and until it lands (the module doc).
pub(crate) struct Importing {
  /// Where it stands.
  stage: Stage,
  /// What the import's room holds of the shard's budget: charged as retention when it started, given back once it is
  /// done, refused or abandoned.
  reserved: u64,
  /// The segments of its changegroup cut and not yet landed on the changeset store (A-767 C1 step 3), oldest first: a
  /// turn's, landed before the next turn ([`next_segment`], [`segment_landed`]).
  segments: std::collections::VecDeque<slates_hg::import::emit::Segment>,
  /// Each segment handed to the store to land and not yet landed, oldest first: its file revisions' contents, let go of
  /// the content store once it is durable (A-767 C1 step 2b, [`segment_landed`]).
  landing: std::collections::VecDeque<Vec<slates_hg::filerev::FileContents>>,
  /// The file revisions' contents of every segment landed so far, let go once more once the import lands: its works
  /// bring back what they read between (A-767 C1 step 2b, as a push's run, `hg_run`).
  landed_contents: Vec<slates_hg::filerev::FileContents>,
  /// The run of segment changesets landed so far: the first's sequence and the last's.
  run: Option<(u64, u64)>,
  /// The last changeset the register named when the import began: what the repository's next sequence is counted from
  /// again where the import is given back (main's ruling, decision 2: never roll forward).
  named: u64,
  /// The import's name, derived as its segments land from their payloads' identities ([`NAME_CONTEXT`]).
  name: blake3::Hasher,
}

/// Where an import stands.
enum Stage {
  /// Under way: the import, the host its directory is read through (the import's own, closed when it is done) and the
  /// repository's directory on the host.
  Running {
    /// The import, boxed: it is by far the largest of a stage.
    import: Box<Import>,
    /// The host.
    host: Box<dyn HostFs>,
    /// The directory.
    directory: HostDir,
  },
  /// Done: its index and state, waiting to land ([`land`]) once its last segments land, and the room it ran within,
  /// which its segments' records and its run's reference carry.
  Done {
    /// What it imported.
    imported: Box<Imported>,
    /// The room it ran within.
    room: u64,
    /// Each bookmark's work, built a unit a turn once the import was done ([`built_works`]): what its landing's entry
    /// moves each bookmark's green by, never built in the landing's one step.
    works: BTreeMap<Vec<u8>, slates_merge::engine::Increment>,
  },
}

impl std::fmt::Debug for Importing {
  fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    let mut shown = formatter.debug_struct("Importing");
    match &self.stage {
      Stage::Running { import, .. } => shown
        .field("turns", &import.turns())
        .field("units", &import.units()),
      Stage::Done { room, .. } => shown.field("done", &true).field("room", room),
    };
    shown
      .field("reserved", &self.reserved)
      .field("segments", &self.segments.len())
      .field("run", &self.run)
      .finish_non_exhaustive()
  }
}

/// Why an import did not start, did not end, or did not land. Each leaves the store as it was.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum ImportFailed {
  /// No Mercurial store of the repository is held here.
  NoStore,
  /// The store holds a history: an import takes the place of the empty index its record was made with.
  NotEmpty,
  /// An import of the repository is under way.
  Busy,
  /// The shard's budget could not take the import's room: what it could admit.
  Budget {
    /// The bytes it could admit.
    available: u64,
  },
  /// The import refused: the store, its requirements, a file, or the host's own refusal.
  Refused(ImportRefused<BaseRefused>),
  /// Its landing was refused (A-687): the entry by the register's guard or the budget, a bookmark's green by the
  /// namespace's rules, or its record or a bookmark's work past what the landing may hold; the wire's refusal.
  Landing(Refusal),
}

/// Where a turn left an import.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Turned {
  /// More of the repository to import.
  More,
  /// Imported, waiting to land, and what it found beside its history.
  Done(Box<Finished>),
}

/// What a finished import found beside the history it handed its store (the module doc): its counts, and the state the
/// register's first entry is to record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Finished {
  /// The changesets.
  pub(crate) changesets: u32,
  /// The manifest revisions.
  pub(crate) manifests: u32,
  /// The file logs.
  pub(crate) file_logs: u64,
  /// The file revisions, over every file log.
  pub(crate) file_revisions: u64,
  /// The obsolescence markers, kept as the index's marker rows.
  pub(crate) markers: u64,
  /// The roots of the draft, secret, archived and internal phases, sorted by phase and node.
  pub(crate) phase_roots: Vec<PhaseHead>,
  /// Phase roots dropped, as 7.2 drops them.
  pub(crate) dropped_phase_roots: u64,
  /// The bookmarks, each its name and a changeset the changelog holds, sorted by name.
  pub(crate) bookmarks: Vec<(Vec<u8>, Node)>,
  /// Bookmarks dropped, as 7.2 drops them.
  pub(crate) dropped_bookmarks: u64,
  /// Bookmark lines passed over, as 7.2 passes them over.
  pub(crate) malformed_bookmarks: u64,
  /// The turns the import took.
  pub(crate) turns: u64,
}

impl Finished {
  /// What `imported` found, over `turns` turns; its index stays with it.
  fn of(imported: &Imported, turns: u64) -> Finished {
    Finished {
      changesets: imported.changesets,
      manifests: imported.manifests,
      file_logs: imported.file_logs,
      file_revisions: imported.file_revisions,
      markers: imported.markers,
      phase_roots: imported.phase_roots.clone(),
      dropped_phase_roots: imported.dropped_phase_roots,
      bookmarks: imported.bookmarks.clone(),
      dropped_bookmarks: imported.dropped_bookmarks,
      malformed_bookmarks: imported.malformed_bookmarks,
      turns,
    }
  }
}

/// The shard's clock (§4.3), which a turn's quantum is spent on.
struct ShardClock;

impl Clock for ShardClock {
  fn now_ns(&mut self) -> u64 {
    slates_rt::futures::now_ns()
  }
}

/// Derived: the most an import may hold at once, its room (the module doc; A-720 part 3): what its shard could admit
/// when it starts (`Store::admittable`, as a push is admitted, `crate::git_push::push_room`), never one request's room,
/// since a repository's logs, and so what reading one holds, are as large as the repository. The room is a bound; what
/// the shard is charged is what the import holds ([`Import::holding`]), trued up after each turn.
pub(crate) fn room_of(state: &ShardState) -> u64 {
  state.store.admittable()
}

/// Derived: the nanoseconds a turn may spend: the runtime's step quantum (§4.3), as the shard's other long work is cut,
/// else the configuration's step budget off the runtime.
pub(crate) fn quantum_of(state: &ShardState) -> u64 {
  slates_rt::futures::step_budget_ns().unwrap_or(state.config.runtime.step_budget_ns)
}

/// Starts repository `id`'s import from `directory` on `host` (the module doc): its store must be the empty index its
/// record was made with, and no import of it under way. The import's room is charged to the shard's budget, and the
/// repository's requirements and listing are read; nothing is kept in the store until its turns ([`turn`]). Refused,
/// nothing is kept, the room is given back and the host closed.
pub(crate) fn begin(
  state: &mut ShardState,
  id: RepositoryId,
  (mut host, directory): (Box<dyn HostFs>, HostDir),
) -> Result<(), ImportFailed> {
  takes_import(&mut state.vcs_stores, id)?;
  let room = room_of(state);
  let named = state
    .vcs_stores
    .landed_of(id)
    .ok_or(ImportFailed::NoStore)?;
  let started = Import::start(
    &mut HgBase::new(&mut *host, directory),
    Admission::new(room),
  );
  match started {
    Ok(import) => {
      let import = import.emitting(segment_bytes(state));
      let holding = import.holding();
      reserve(state, holding)?;
      let importing = Importing {
        stage: Stage::Running {
          import: Box::new(import),
          host,
          directory,
        },
        reserved: holding,
        segments: std::collections::VecDeque::new(),
        landing: std::collections::VecDeque::new(),
        landed_contents: Vec::new(),
        run: None,
        named,
        name: blake3::Hasher::new_derive_key(NAME_CONTEXT),
      };
      state.vcs_stores.imports.insert(id, importing);
      state.count(STARTED, 1);
      Ok(())
    }
    Err(refused) => Err(ImportFailed::Refused(refused)),
  }
}

/// Whether repository `id`'s store takes an import now: a Mercurial store holding no row, and no import of it under way
/// or waiting to land.
fn takes_import(stores: &mut VcsStores, id: RepositoryId) -> Result<(), ImportFailed> {
  if stores.imports.contains_key(&id) {
    return Err(ImportFailed::Busy);
  }
  match stores.entries.get_mut(&id) {
    Some(RepositoryStore::Mercurial(store)) => {
      if holds_none(&mut store.index) {
        Ok(())
      } else {
        Err(ImportFailed::NotEmpty)
      }
    }
    _ => Err(ImportFailed::NoStore),
  }
}

/// Whether `index` holds no row: the empty index a repository's record was made with, which an import's index, live or
/// replayed, takes the place of (the map's `replay_hg`).
pub(super) fn holds_none(index: &mut HgIndex) -> bool {
  index.rows().rows() == 0
}

/// Charges an import's `room` to the shard's budget as retention, the arena first grown from its pool by it.
fn reserve(state: &mut ShardState, room: u64) -> Result<(), ImportFailed> {
  state.store.make_room(room);
  if state.store.budget.charge_retention(room).is_err() {
    return Err(ImportFailed::Budget {
      available: state.store.admittable(),
    });
  }
  Ok(())
}

/// One turn of repository `id`'s import (the module doc): units of work until `quantum_ns` is spent on `clock`, at
/// least one, lent the host and the content store. `More` while there is more; once it is done, what it found, its
/// index waiting to land; refused, everything it held is given back. `None` when no import of `id` is under way.
pub(crate) fn turn(
  state: &mut ShardState,
  id: RepositoryId,
  (clock, quantum_ns): (&mut dyn Clock, u64),
) -> Option<Result<Turned, ImportFailed>> {
  let advanced = advance(state, id, Turn { clock, quantum_ns })?;
  state.count(TURNS, 1);
  Some(match advanced {
    Ok(Progress::More) => Ok(Turned::More),
    Ok(Progress::Done) => finish(state, id),
    Err(refused) => {
      end(state, id);
      state.count(REFUSED, 1);
      Err(ImportFailed::Refused(refused))
    }
  })
}

/// Derived: a segment of an import's changegroup, the least bytes one is cut at (A-767 C1 step 3): one trunk node of
/// the changeset store's memtable, what one of its flushes turns into one bundle (`store_sizing` in `slates-api`'s
/// daemon: the content plane's chunk, the unit of copy-on-write, times SplinterDB's fanout). An import holds one segment
/// at a time, and lands as many changesets as its changegroup has segments.
pub(crate) fn segment_bytes(state: &ShardState) -> usize {
  let fanout: slates_machine::Derived<usize> = slates_machine::derived!(
    8,
    "SplinterDB's target fanout, as the daemon's store_sizing sizes the store's memtable",
    ["engine-structure.md §4"]
  );
  slates_vfs::content::chunk_bytes(state.config.page)
    .get()
    .saturating_mul(fanout.get())
}

/// Counter: a segment of an import's changegroup landed on the changeset store as a changeset (A-767 C1 step 3).
/// Format: a counter name in the status report.
pub(crate) const SEGMENTS_LANDED: &str = "vcs_store.hg_import.segments_landed";
/// Counter: a segment the store refused to land, its import given back. Format: a counter name.
pub(crate) const SEGMENTS_UNLANDED: &str = "vcs_store.hg_import.segments_unlanded";
/// Counter: a segment of an import's changegroup cut because the whole file texts it kept in the content store reached
/// the segment's bound before its changegroup's bytes did (goal 93 hg, R; main 01a20182's ruling of 2026-11-24): its
/// texts let go a segment at a time, never a history's at once. Format: a counter name in the status report.
pub(crate) const SEGMENTS_CUT_BY_TEXTS: &str = "vcs_store.hg_import.segments_cut_by_texts";
/// Counter: a segment cut by its changegroup's bytes, or at the stream's end. Format: a counter name.
pub(crate) const SEGMENTS_CUT_BY_BYTES: &str = "vcs_store.hg_import.segments_cut_by_bytes";
/// Counter: the nanoseconds the import's task was held, summed, waiting for the store to land a segment it could not
/// overlap with its work: the one in flight when the next was cut, or the last (main 01a20182's order of 2026-11-24:
/// the landings' own share of an import's time). Format: a counter name in the status report.
pub(crate) const SEGMENT_LAND_WAIT_NS: &str = "vcs_store.hg_import.segment_land_wait_ns";
/// Counter: the longest of those waits, nanoseconds. Format: a counter name.
pub(crate) const SEGMENT_LAND_WAIT_MAX_NS: &str = "vcs_store.hg_import.segment_land_wait_max_ns";
/// Counter: the nanoseconds each segment's landing took, summed, from its asking to the store's answer, the import
/// waiting or not: what [`SEGMENT_LAND_WAIT_NS`] is held against. Format: a counter name.
pub(crate) const SEGMENT_LAND_NS: &str = "vcs_store.hg_import.segment_land_ns";
/// Counter: a segment's landing the store answered while the import went on with its turns, its wait overlapped with
/// the import's work (`run`'s pipelined landing). Format: a counter name.
pub(crate) const SEGMENTS_LANDED_OVERLAPPED: &str =
  "vcs_store.hg_import.segments_landed_overlapped";

/// A segment changeset to land: the store, its header, its sections, and its payload's identity.
pub(crate) type SegmentLanding = (
  slates_store::owner::StoreHandle,
  slates_store::log::Header,
  Vec<slates_store::owner::Part>,
  [u8; 32],
);

/// The next segment of repository `id`'s import to land on the changeset store (A-767 C1 step 3), taken from the queue,
/// its file revisions moved to the landings awaited ([`segment_landed`] lets them go); none when no segment waits, or
/// no store is held. Its payload is then made into chunks ([`segment_parts`]).
fn next_segment(
  state: &mut ShardState,
  id: RepositoryId,
) -> Option<(
  slates_store::owner::StoreHandle,
  slates_hg::import::emit::Segment,
)> {
  let disk = state.vcs_disk.clone()?;
  let importing = state.vcs_stores.imports.get_mut(&id)?;
  let mut segment = importing.segments.pop_front()?;
  importing
    .landing
    .push_back(core::mem::take(&mut segment.contents));
  Some((disk, segment))
}

/// `segment` of repository `id`'s import as a changeset at the repository's next sequence: its `Record` (the index rows
/// of its revisions, by identity, which never read the payload), its `Payload` its revisions' deltas as stock's revlog
/// chunks (`chunks`, made a turn's steps at a time as a push's segment's are; goal 93, main's ruling (b)) and its
/// `Texts` each revision's text, its delta parent's and its chunk's place, held as a chunk there. Until 2026-11-25 the
/// payload was the changegroup's slice, raw, so a stream compressed an imported repository at stream time. None where
/// the chunks are not one a revision.
fn segment_parts(
  state: &mut ShardState,
  id: RepositoryId,
  (disk, segment): (
    slates_store::owner::StoreHandle,
    slates_hg::import::emit::Segment,
  ),
  (chunks, places): crate::repository::hg_run::Chunks,
) -> Option<SegmentLanding> {
  if places.len() != segment.texts.len() {
    return None;
  }
  let sequence = state.vcs_stores.next_changeset(id)?;
  let payload_identity = *blake3::hash(&chunks).as_bytes();
  let texts =
    crate::repository::hg_run::texts_section(&segment.texts, (payload_identity, Some(&places)));
  let header = slates_store::log::Header {
    tool: slates_store::log::Tool::Mercurial,
    repository: u128::from_be_bytes(id.bytes),
    sequence,
    judged_over: sequence.saturating_sub(1),
    // The store's standing grant (its `G` row, R10): the landing's provenance.
    grant: disk.grant(),
    work: 0,
    time_ns: slates_rt::futures::now_ns(),
    sections: 0,
  };
  let parts = vec![
    (slates_store::log::SectionKind::Record, segment.record),
    (slates_store::log::SectionKind::Payload, chunks),
    (slates_store::log::SectionKind::Texts, texts),
  ];
  Some((disk, header, parts, payload_identity))
}

/// The next segment of repository `id`'s import made into its changeset whole, on a shard a test holds: its payload
/// chunked in one go ([`crate::repository::hg_push::chunked_whole_counted`]), else as [`landing_begun`] makes it.
#[cfg(test)]
pub(crate) fn segment_changeset(
  state: &mut ShardState,
  id: RepositoryId,
) -> Option<SegmentLanding> {
  let (disk, segment) = next_segment(state, id)?;
  let chunks =
    crate::repository::hg_push::chunked_whole_counted(state, (&segment.texts, &segment.payload))?;
  segment_parts(state, id, (disk, segment), chunks)
}

/// Repository `id`'s segment changeset `sequence` landed (`landed`) or refused (A-767 C1 step 3): landed, the run grows
/// and the import's name takes the payload's identity; refused, the import is given back, the repository's next
/// sequence counted again from the last the register names, and the store's refusal answered.
pub(crate) fn segment_landed(
  state: &mut ShardState,
  id: RepositoryId,
  (sequence, payload_identity): (u64, [u8; 32]),
  landed: bool,
) -> Result<(), ImportFailed> {
  if !landed {
    state.count(SEGMENTS_UNLANDED, 1);
    abandon(state, id);
    return Err(ImportFailed::Landing(Refusal::Overloaded {
      shard: state.shard,
    }));
  }
  let importing = state
    .vcs_stores
    .imports
    .get_mut(&id)
    .ok_or(ImportFailed::NoStore)?;
  importing.run = Some(match importing.run {
    Some((first, _)) => (first, sequence),
    None => (sequence, sequence),
  });
  importing.name.update(&payload_identity);
  let contents = importing.landing.pop_front().unwrap_or_default();
  state.count(SEGMENTS_LANDED, 1);
  // Durable on the changeset store, which serves them from here: the segment's file texts leave the content store, their
  // entries kept, so the rows still name them by identity (A-767 C1 step 2b): every window but those a home pins, as a
  // push's (`hg_run::let_go_windows`). Kept for the second let-go once it lands, since its works bring back what they
  // read in between.
  let (freed, pinned) = crate::repository::hg_run::let_go_windows(state, None, &contents);
  state.count(FILE_TEXT_BYTES_LET_GO, freed);
  state.count(FILE_TEXTS_PINNED, pinned);
  if let Some(importing) = state.vcs_stores.imports.get_mut(&id) {
    importing.landed_contents.extend(contents);
  }
  Ok(())
}

/// Counter: the bytes of an import's file texts let go of the content store once the segment carrying them was durable
/// on the changeset store (A-767 C1 step 2b), and again once it landed. Format: a counter name in the status report.
pub(crate) const FILE_TEXT_BYTES_LET_GO: &str = "vcs_store.hg_import.file_text_bytes_let_go";
/// Counter: an import's landed file window kept because a home holds it here (`vcs_store::landed_pin`). Format: a
/// counter name.
pub(crate) const FILE_TEXTS_PINNED: &str = "vcs_store.hg_import.file_texts_pinned";

/// [`land`] for a test that holds the shard's state: the segments its import cut landed first
/// ([`land_segments_blocking`]), as [`run`] lands them between turns.
#[cfg(test)]
pub(crate) fn land_imported(state: &mut ShardState, id: RepositoryId) -> Result<(), ImportFailed> {
  land_segments_blocking(state, id)?;
  built_works_whole(state, id)?;
  // A test holds the shard's state through its landing: its revisions let go at once.
  let contents = land(state, id)?;
  let (freed, pinned) = crate::repository::hg_run::let_go_windows(state, Some(id), &contents);
  state.count(FILE_TEXT_BYTES_LET_GO, freed);
  state.count(FILE_TEXTS_PINNED, pinned);
  Ok(())
}

/// Notes a landed import's file revisions `contents` for a home and lets them go, a turn at a time, once its landing is
/// durable (`repository::import`'s answer): its counters.
pub(crate) async fn let_go_landed_import(
  id: RepositoryId,
  contents: Vec<slates_hg::filerev::FileContents>,
) {
  crate::repository::hg_run::let_go_committed(
    id,
    contents,
    (FILE_TEXT_BYTES_LET_GO, FILE_TEXTS_PINNED),
  )
  .await;
}

/// [`land_imported`] for a test whose own thread waits on the node's store, never `daemon`'s shard of partition
/// `partition`: each segment the import cut is taken in one observation, landed from this thread
/// (`crate::test_store::land_blocking`) and counted in the next, as [`run`] lands them between turns; then the import is
/// landed in one more. A history that held the shard through a segment's commit outlasted the watchdog's window on a slow
/// `fdatasync` (docs/bugs/2026-11-24-a-test-creation-held-its-shard-through-the-stores-commit.md).
#[cfg(test)]
pub(crate) fn land_imported_on(
  daemon: &crate::Daemon,
  partition: usize,
  id: RepositoryId,
) -> Result<(), ImportFailed> {
  while let Some((disk, header, parts, identity)) =
    crate::daemon::observe_at(daemon, partition, move |state| segment_changeset(state, id))
  {
    let sequence = header.sequence;
    let landed = crate::test_store::land_blocking(disk, header, parts);
    crate::daemon::observe_at(daemon, partition, move |state| {
      segment_landed(state, id, (sequence, identity), landed)
    })?;
  }
  crate::daemon::observe_at(daemon, partition, move |state| {
    built_works_whole(state, id)?;
    let contents = land(state, id)?;
    let (freed, pinned) = crate::repository::hg_run::let_go_windows(state, Some(id), &contents);
    state.count(FILE_TEXT_BYTES_LET_GO, freed);
    state.count(FILE_TEXTS_PINNED, pinned);
    Ok(())
  })
}

/// Every segment repository `id`'s import has cut, landed on the changeset store, waiting on a thread of its own for
/// each (`crate::test_store::land_blocking`): [`run`]'s landing for a test that holds the shard's state.
#[cfg(test)]
pub(crate) fn land_segments_blocking(
  state: &mut ShardState,
  id: RepositoryId,
) -> Result<(), ImportFailed> {
  while let Some((disk, header, parts, identity)) = segment_changeset(state, id) {
    let sequence = header.sequence;
    let landed = crate::test_store::land_blocking(disk, header, parts);
    segment_landed(state, id, (sequence, identity), landed)?;
  }
  Ok(())
}

/// One turn of repository `id`'s import over the map, lent every byte the shard could admit now, then the arena grown
/// by what it kept and the charge trued up (the module doc). `None` when no import of `id` is under way.
fn advance(
  state: &mut ShardState,
  id: RepositoryId,
  turn: Turn<'_>,
) -> Option<Result<Progress, ImportRefused<BaseRefused>>> {
  let running = matches!(
    state.vcs_stores.imports.get(&id),
    Some(Importing {
      stage: Stage::Running { .. },
      ..
    })
  );
  if !running {
    return None;
  }
  lend(&mut state.store, &mut state.vcs_stores, 0);
  let lent = super::Lent::new(&mut state.store, &mut state.vcs_stores);
  let stores = &mut *lent.stores;
  let advanced = match (stores.content.as_mut(), stores.imports.get_mut(&id)) {
    (
      Some(content),
      Some(Importing {
        stage: Stage::Running {
          import,
          host,
          directory,
        },
        ..
      }),
    ) => {
      let mut base = HgBase::new(&mut **host, *directory);
      Some(import.advance(&mut base, content, turn))
    }
    _ => None,
  };
  let (mut by_texts, mut by_bytes) = (0_u64, 0_u64);
  if let Some(Importing {
    stage: Stage::Running { import, .. },
    segments,
    ..
  }) = stores.imports.get_mut(&id)
  {
    while let Some(segment) = import.take_segment() {
      if segment.by_texts {
        by_texts = by_texts.saturating_add(1);
      } else {
        by_bytes = by_bytes.saturating_add(1);
      }
      segments.push_back(segment);
    }
  }
  drop(lent);
  state.count(SEGMENTS_CUT_BY_TEXTS, by_texts);
  state.count(SEGMENTS_CUT_BY_BYTES, by_bytes);
  grown_and_settled(state);
  true_up_holding(state, id);
  advanced
}

/// Trues the shard's charge for repository `id`'s running import up to what it holds between turns
/// ([`Import::holding`], A-720 part 3): more charged as retention, less credited back. A charge the shard refuses is
/// counted ([`super::CHARGE_SHORT`]) and the import goes on within its room.
fn true_up_holding(state: &mut ShardState, id: RepositoryId) {
  let Some(Importing {
    stage: Stage::Running { import, .. },
    reserved,
    ..
  }) = state.vcs_stores.imports.get(&id)
  else {
    return;
  };
  let (holding, charged) = (import.holding(), *reserved);
  let short = if holding > charged {
    state
      .store
      .budget
      .charge_retention(holding.saturating_sub(charged))
      .is_err()
  } else {
    state
      .store
      .budget
      .credit_retention(charged.saturating_sub(holding));
    false
  };
  if short {
    state.count(super::CHARGE_SHORT, 1);
  } else if let Some(Importing { reserved, .. }) = state.vcs_stores.imports.get_mut(&id) {
    *reserved = holding;
  }
}

/// After a turn: the arena grown from its pool by what the content store holds past the shard's charge, then the
/// charge trued up to what it holds (the map's `settle`).
fn grown_and_settled(state: &mut ShardState) {
  let used = state
    .vcs_stores
    .content
    .as_ref()
    .map_or(0, ContentStore::used);
  state
    .store
    .make_room(used.saturating_sub(state.vcs_stores.charged));
  settle(state);
}

/// Repository `id`'s import done (the module doc): its room given back and its host closed, and what it imported kept in
/// the map, waiting to land; what it found answered.
fn finish(state: &mut ShardState, id: RepositoryId) -> Result<Turned, ImportFailed> {
  let Some(importing) = state.vcs_stores.imports.remove(&id) else {
    return Err(ImportFailed::NoStore);
  };
  let Importing {
    stage,
    reserved,
    segments,
    landing,
    landed_contents,
    run,
    named,
    name,
  } = importing;
  let import = match stage {
    Stage::Running { import, .. } => *import,
    done => {
      state.vcs_stores.imports.insert(
        id,
        Importing {
          stage: done,
          reserved,
          segments,
          landing,
          landed_contents,
          run,
          named,
          name,
        },
      );
      return Err(ImportFailed::Busy);
    }
  };
  state.store.budget.credit_retention(reserved);
  let turns = import.turns();
  let imported = import
    .into_imported()
    .ok_or(ImportFailed::Refused(ImportRefused::Ended))?;
  let finished = Finished::of(&imported, turns);
  // The room its segments' records carry and its run's reference names: what it ran within.
  let room = room_of(state);
  state.vcs_stores.imports.insert(
    id,
    Importing {
      stage: Stage::Done {
        imported: Box::new(imported),
        room,
        works: BTreeMap::new(),
      },
      reserved: 0,
      segments,
      landing,
      landed_contents,
      run,
      named,
      name,
    },
  );
  state.count(DONE, 1);
  Ok(Turned::Done(Box::new(finished)))
}

/// Puts `index` in repository `id`'s Mercurial store's place, giving back the empty index it replaces: whether every
/// hold of that one went back. A store gone, or holding a history now, keeps what it holds, and `index` comes back.
fn place(
  stores: &mut VcsStores,
  id: RepositoryId,
  index: HgIndex,
) -> Result<bool, Box<(ImportFailed, HgIndex)>> {
  let Some(content) = stores.content.as_mut() else {
    return Err(Box::new((ImportFailed::NoStore, index)));
  };
  let Some(RepositoryStore::Mercurial(store)) = stores.entries.get_mut(&id) else {
    return Err(Box::new((ImportFailed::NoStore, index)));
  };
  if !holds_none(&mut store.index) {
    return Err(Box::new((ImportFailed::NotEmpty, index)));
  }
  let empty = core::mem::replace(&mut store.index, index);
  // An imported history is on the changeset store, a segment changeset at a time (A-767 C1 step 3), so it is stored.
  let released = empty.release(content).is_ok();
  stores.index_changed(id);
  Ok(released)
}

/// Puts the index an import's entry carries in repository `id`'s store's place, between the register's guard and the
/// entry's apply (the module doc; `repository::push::push_imported`). Placed, `index` is empty; refused, the index is
/// back in `index` and the wire's refusal answered: `NotEmpty` for a store that took a history meanwhile, or
/// `ContentUnavailable` for a store gone.
pub(crate) fn place_imported(
  state: &mut ShardState,
  id: RepositoryId,
  index: &mut Option<HgIndex>,
) -> Result<(), Refusal> {
  let Some(taken) = index.take() else {
    return Err(Refusal::ContentUnavailable);
  };
  super::hg_view::walks_noted(state, &taken);
  match place(&mut state.vcs_stores, id, taken) {
    Ok(whole) => {
      if !whole {
        state.count(RELEASE_REFUSED, 1);
      }
      settle(state);
      // An imported index is built whole: its runs merged a slice at a time (goal 93's pull).
      super::index_upkeep::built(state, id);
      Ok(())
    }
    Err(refused) => {
      let (failed, back) = *refused;
      *index = Some(back);
      Err(match failed {
        ImportFailed::NotEmpty => Refusal::ImportRefused {
          fault: ImportFault::NotEmpty,
        },
        _ => Refusal::ContentUnavailable,
      })
    }
  }
}

/// Ends repository `id`'s refused import: the import gave its index back as it refused; its room is given back and its
/// host closed.
fn end(state: &mut ShardState, id: RepositoryId) {
  if let Some(Importing {
    reserved, named, ..
  }) = state.vcs_stores.imports.remove(&id)
  {
    state.store.budget.credit_retention(reserved);
    state.vcs_stores.set_landed(id, named);
  }
}

/// Gives back repository `id`'s import, under way or waiting to land, if there is one (the module doc's cancellation):
/// its index's holds and its room, its host closed. What freeing the repository does first (the map's `free`).
pub(crate) fn abandon(state: &mut ShardState, id: RepositoryId) {
  let Some(Importing {
    stage,
    reserved,
    named,
    ..
  }) = state.vcs_stores.imports.remove(&id)
  else {
    return;
  };
  state.store.budget.credit_retention(reserved);
  // Its segments landed unnamed: the next landing takes the last the register names, plus one (decision 2).
  state.vcs_stores.set_landed(id, named);
  let whole = match (state.vcs_stores.content_mut(), stage) {
    (Some(content), Stage::Running { import, .. }) => (*import).abandon(content).is_ok(),
    (Some(content), Stage::Done { imported, .. }) => {
      let Imported { index, .. } = *imported;
      index.release(content).is_ok()
    }
    (None, _) => true,
  };
  if !whole {
    state.count(RELEASE_REFUSED, 1);
  }
  state.count(ABANDONED, 1);
  settle(state);
}

/// Gives back an index a landing did not place, then settles.
fn give_back(state: &mut ShardState, index: HgIndex) {
  let whole = match state.vcs_stores.content_mut() {
    Some(content) => index.release(content).is_ok(),
    None => true,
  };
  if !whole {
    state.count(RELEASE_REFUSED, 1);
  }
  settle(state);
}

/// Lands repository `id`'s done import (A-687, the module doc's "Durable"): its entry appended through A-635's append,
/// in the caller's transaction when one is open, its index put in its store's place between the register's guard and
/// the entry's apply. Refused, nothing is recorded, and the index and everything it held are given back. `Busy` while
/// the import is still under way, `NoStore` when none is held for `id`.
pub(crate) fn land(
  state: &mut ShardState,
  id: RepositoryId,
) -> Result<Vec<slates_hg::filerev::FileContents>, ImportFailed> {
  let (mut imported, room, mut works, (run, named, name), landed_contents) =
    match state.vcs_stores.imports.remove(&id) {
      Some(Importing {
        stage: Stage::Done {
          imported,
          room,
          works,
        },
        segments,
        run: Some(run),
        named,
        name,
        landed_contents,
        ..
      }) if segments.is_empty() => (imported, room, works, (run, named, name), landed_contents),
      Some(running) => {
        state.vcs_stores.imports.insert(id, running);
        return Err(ImportFailed::Busy);
      }
      None => return Err(ImportFailed::NoStore),
    };
  // The register names the run (A-767 C1 step 3): its segments' records and payloads are the changeset store's.
  let record = slates_hg::import::record::write_run_reference(room, run);
  let name = *name.finalize().as_bytes();
  let entry = super::lent_state(state, |state| {
    landing_entry(state, id, (&*imported, &mut works), (record, name))
  });
  let mut index = Some(core::mem::take(&mut imported.index));
  let appended = entry.and_then(|(owner, changes, parts, record)| {
    crate::repository::push::push_imported(
      state,
      (&owner, id),
      changes,
      parts,
      (&mut index, record),
    )
    .map_err(landing_refusal)
  });
  if let Some(index) = index.take() {
    give_back(state, index);
  }
  match appended {
    Ok(_) => {
      state.count(LANDED, 1);
      // Landed, its file revisions are its caller's to note for a home and let go once more, a turn at a time once its
      // landing is durable (`hg_run::let_go_committed`, `let_go_landed_import`): its works brought back what they read
      // (A-767 C1 step 2b), and its index now holds them, by which a home's read brings one back (goal 93 item 2b(b)).
      Ok(landed_contents)
    }
    Err(refusal) => {
      // The run landed unnamed: the next landing takes the last the register names, plus one (decision 2).
      state.vcs_stores.set_landed(id, named);
      state.count(LANDING_REFUSED, 1);
      Err(ImportFailed::Landing(refusal))
    }
  }
}

/// What an import's entry is made of: its owner, the bookmarks' greens, the phase roots, and its record.
type Landing = (Principal, Vec<BranchChange>, EntryParts, Vec<u8>);

/// The entry repository `id`'s import lands (the module doc's "Durable"): its owner, each bookmark's green, the phase
/// roots added, and the import's record carrying `room`, the room it ran within; or the wire's refusal.
fn landing_entry(
  state: &ShardState,
  id: RepositoryId,
  (imported, works): (
    &Imported,
    &mut BTreeMap<Vec<u8>, slates_merge::engine::Increment>,
  ),
  (record, name): (Vec<u8>, [u8; 32]),
) -> Result<Landing, Refusal> {
  let _ = name;
  let repositories = state.db.partition().repositories();
  let owner = repositories
    .repository(id)
    .map(|record| record.owner.clone())
    .ok_or(Refusal::NotFound)?;
  let root = repositories
    .members_of(id)
    .find(|member| matches!(member.origin, MemberOrigin::Root))
    .map(|member| member.green)
    .ok_or(Refusal::NotFound)?;
  let changes = bookmark_greens(state, (root, &imported.bookmarks), works)?;
  // The record rides as the import's stored objects (`push_imported`), never as the parts' own.
  let parts = EntryParts {
    checks: Vec::new(),
    names: Vec::new(),
    state: phase_roots_added(&imported.phase_roots),
    objects: Vec::new(),
  };
  Ok((owner, changes, parts, record))
}

/// Format: the BLAKE3 context an import's name is derived in, over its segments' payloads' identities in order, so two
/// imports of other histories never share one: every work of its entry begins its identity with it (A-618's push name).
const NAME_CONTEXT: &str = "slates 2026-11-21 a Mercurial import's name, over its segments";

/// Each phase root as the phases log adds it (§4.17.3 decision 3): its phase's number and its node, in the import's
/// order (by phase, then node).
fn phase_roots_added(roots: &[PhaseHead]) -> Vec<StateChange> {
  roots
    .iter()
    .filter_map(|root| {
      let phase = u8::try_from(root.phase.number()).ok()?;
      Some(StateChange::PhaseRootAdded {
        phase,
        node: root.node.to_vec(),
      })
    })
    .collect()
}

/// Each bookmark a green (§4.17.3 decision 2): made from `root` at its head, as a push makes a bookmark from a line's
/// version, and moved to the bookmark's changeset by its work, built before the landing ([`built_works`]); none where
/// the root's head is that changeset already. A work its bookmark wants that was not built refuses the landing
/// `ContentUnavailable`, never built here: the landing is one step, and a work is a tree's walk.
fn bookmark_greens(
  state: &ShardState,
  (root, bookmarks): (DbVolumeId, &[(Vec<u8>, Node)]),
  works: &mut BTreeMap<Vec<u8>, slates_merge::engine::Increment>,
) -> Result<Vec<BranchChange>, Refusal> {
  let version = state.db.partition().green_head(root);
  let head = root_head(state, root)?;
  let mut changes = Vec::with_capacity(bookmarks.len());
  for (mark, node) in bookmarks {
    let work = if head == Some(*node) {
      None
    } else {
      Some(works.remove(mark).ok_or(Refusal::ContentUnavailable)?)
    };
    changes.push(BranchChange::Create {
      name: crate::repository::hg_push::bookmark_name(mark),
      from: root,
      version,
      work,
    });
  }
  Ok(changes)
}

/// The changeset `root`'s green holds as its head (`HEAD_ATTRIBUTE`), none for an empty green; refused where the green
/// is not held here.
fn root_head(state: &ShardState, root: DbVolumeId) -> Result<Option<Node>, Refusal> {
  let engine = state.greens.get(&root).ok_or(Refusal::ContentUnavailable)?;
  Ok(
    engine
      .xattr(ROOT, HEAD_ATTRIBUTE)
      .and_then(|bytes| Node::try_from(bytes).ok()),
  )
}

/// What repository `id`'s done import's bookmarks need built: the root green, its version and head, the import's room and
/// name, and each bookmark whose changeset is not the head; none where no import is done or the repository holds no
/// root.
fn wanted_works(state: &ShardState, id: RepositoryId) -> Option<WantedWorks> {
  let Some(Importing {
    stage: Stage::Done { imported, room, .. },
    name,
    ..
  }) = state.vcs_stores.imports.get(&id)
  else {
    return None;
  };
  let root = state
    .db
    .partition()
    .repositories()
    .members_of(id)
    .find(|member| matches!(member.origin, MemberOrigin::Root))
    .map(|member| member.green)?;
  let head = root_head(state, root).ok()?;
  let marks = imported
    .bookmarks
    .iter()
    .filter(|(_, node)| head != Some(*node))
    .cloned()
    .collect();
  Some(WantedWorks {
    root,
    version: state.db.partition().green_head(root),
    head,
    room: *room,
    name: *name.clone().finalize().as_bytes(),
    marks,
  })
}

/// The bookmarks' works an import's landing moves its greens by ([`wanted_works`]).
struct WantedWorks {
  /// The repository's root green, which each bookmark's green is made from.
  root: DbVolumeId,
  /// Its version.
  version: u64,
  /// Its head changeset.
  head: Option<Node>,
  /// The import's room.
  room: u64,
  /// The import's name, each work's name.
  name: [u8; 32],
  /// Each bookmark and the changeset it moves to.
  marks: Vec<(Vec<u8>, Node)>,
}

/// Counter: a unit-a-turn turn of an import's bookmark works (2b, main 01a20182's ruling of 2026-11-22 02:52: the landing
/// builds none in its one step). Format: a counter name in the status report.
pub(crate) const WORK_TURNS: &str = "vcs_store.hg_import.work_turns";

/// Repository `id`'s done import's bookmark works, each built a unit a turn under half the shard's quantum as a push's are
/// (`hg_push::built_works`), over the import's own index; a turn waiting for a text not at hand (one a landed segment
/// let go) reads it from the node's changeset store between turns and brings it back. Kept in the done import for its
/// landing. None where the shard's state is gone; refused as the work's refusal names it.
async fn built_works(id: RepositoryId) -> Option<Result<(), ImportFailed>> {
  let Some(wanted) = crate::state::with_state(|state| wanted_works(state, id)).flatten() else {
    return Some(Ok(()));
  };
  let verified = Verified::default();
  let mut admission = Admission::new(wanted.room);
  for (mark, node) in &wanted.marks {
    let moved = Move {
      name: wanted.name,
      base_version: wanted.version,
      base: wanted.head.as_ref(),
      head: node,
    };
    let begun = crate::state::with_state(|state| {
      let pace = slates_merge::pace::Pace::new(slates_rt::futures::now_ns);
      super::lent_state(state, |state| {
        done_held(state, id, |held| {
          tree::begin_work(
            &moved,
            (&verified, held),
            (admission.left(), pace),
            &mut admission,
          )
        })
      })
    })
    .flatten()?;
    let mut building = match begun {
      Ok(building) => building,
      Err(refused) => {
        return Some(Err(ImportFailed::Landing(work_refused(
          &refused,
          wanted.room,
        ))));
      }
    };
    loop {
      let advanced = crate::state::with_state(|state| {
        let quantum = state.config.step_quantum_ns() / 2;
        building
          .pace()
          .turn(slates_rt::futures::now_ns().saturating_add(quantum.max(1)));
        state.count(WORK_TURNS, 1);
        // The windows lent for the turn, as the landing's own step lends them (`super::lent_state`).
        super::lent_state(state, |state| {
          let engine = state.greens.get(&wanted.root)?;
          done_held(state, id, |held| {
            building.advance(&engine, (&verified, held))
          })
        })
      })
      .flatten()?;
      match advanced {
        None if building.ask_waited(admission.left()) => {
          if !super::hg_view::answer_store_wants(&mut building, id).await {
            return None;
          }
          let brought =
            crate::state::with_state(|state| brought_back(state, id, &mut building)).flatten();
          if brought.is_none() {
            return Some(Err(ImportFailed::Landing(Refusal::ContentUnavailable)));
          }
        }
        None => slates_rt::futures::yield_now().await,
        Some(Ok(built)) => {
          if admission
            .charge(u64::try_from(built.increment.post_state.len()).unwrap_or(u64::MAX))
            .is_err()
          {
            return Some(Err(ImportFailed::Landing(past_room(wanted.room))));
          }
          crate::state::with_state(|state| keep_work(state, id, mark.clone(), built.increment))?;
          break;
        }
        Some(Err(refused)) => {
          return Some(Err(ImportFailed::Landing(work_refused(
            &refused,
            wanted.room,
          ))));
        }
      }
    }
  }
  Some(Ok(()))
}

/// Runs `step` over repository `id`'s done import's index, its contents in the shard's content store, within a step its
/// caller has lent the windows (`super::lent_state`); none where no import is done or no content store is held.
fn done_held<T>(
  state: &ShardState,
  id: RepositoryId,
  step: impl FnOnce(&IndexHeld<'_>) -> T,
) -> Option<T> {
  let Some(Importing {
    stage: Stage::Done { imported, .. },
    ..
  }) = state.vcs_stores.imports.get(&id)
  else {
    return None;
  };
  let content = state.vcs_stores.content.as_ref()?;
  Some(step(&IndexHeld::new(&imported.index, content)))
}

/// The text `building` was given for the file revision it waited for, brought back into the content store by the
/// identities repository `id`'s done import's index names; none where nothing was given or the store refused it.
/// Called outside any `lent_state`, since its `within_budget` grows the arena before it lends the chunks (refused
/// `OutOfReach` inside one).
fn brought_back(
  state: &mut ShardState,
  id: RepositoryId,
  building: &mut tree::WorkBuild,
) -> Option<()> {
  let (path, node, text) = building.take_given()?;
  // Brought back within the room its bytes take, lent to the content store for the step (a put outside it is refused
  // `OverBudget`, as a push's works found).
  let lend = u64::try_from(text.len()).unwrap_or(u64::MAX);
  super::within_budget(state, lend, |stores| {
    let VcsStores {
      imports, content, ..
    } = stores;
    let Some(Importing {
      stage: Stage::Done { imported, .. },
      ..
    }) = imports.get(&id)
    else {
      return None;
    };
    let content = content.as_mut()?;
    slates_hg::push::held::bring_back_file((&imported.index, content), (&path, &node), &text).ok()
  })
}

/// Keeps `work`, bookmark `mark`'s, in repository `id`'s done import for its landing.
fn keep_work(
  state: &mut ShardState,
  id: RepositoryId,
  mark: Vec<u8>,
  work: slates_merge::engine::Increment,
) {
  if let Some(Importing {
    stage: Stage::Done { works, .. },
    ..
  }) = state.vcs_stores.imports.get_mut(&id)
  {
    works.insert(mark, work);
  }
}

/// Repository `id`'s done import's bookmark works built whole, for a test that holds the shard's state and lands it in
/// one call ([`land_imported`]): [`built_works`]' units at once, a text not at hand read from the node's changeset store
/// on a thread of its own (`test_store::answer_wants_blocking`) and brought back.
#[cfg(test)]
fn built_works_whole(state: &mut ShardState, id: RepositoryId) -> Result<(), ImportFailed> {
  let unavailable = || ImportFailed::Landing(Refusal::ContentUnavailable);
  let Some(wanted) = wanted_works(state, id) else {
    return Ok(());
  };
  let verified = Verified::default();
  let mut admission = Admission::new(wanted.room);
  for (mark, node) in &wanted.marks {
    let moved = Move {
      name: wanted.name,
      base_version: wanted.version,
      base: wanted.head.as_ref(),
      head: node,
    };
    let begun = super::lent_state(state, |state| {
      let pace = slates_merge::pace::Pace::new(slates_rt::futures::now_ns);
      done_held(state, id, |held| {
        tree::begin_work(
          &moved,
          (&verified, held),
          (admission.left(), pace),
          &mut admission,
        )
      })
    })
    .ok_or_else(unavailable)?;
    let mut building =
      begun.map_err(|refused| ImportFailed::Landing(work_refused(&refused, wanted.room)))?;
    loop {
      // Whole: every unit at once, a text not at hand read from the store on a thread of its own and brought back.
      building.pace().turn(u64::MAX);
      let advanced = super::lent_state(state, |state| {
        let engine = state.greens.get(&wanted.root)?;
        done_held(state, id, |held| {
          building.advance(&engine, (&verified, held))
        })
      })
      .ok_or_else(unavailable)?;
      match advanced {
        None if building.ask_waited(admission.left()) => {
          let disk = state.vcs_disk.clone().ok_or_else(unavailable)?;
          if !crate::test_store::answer_wants_blocking(
            &disk,
            u128::from_be_bytes(id.bytes),
            &mut building,
          ) {
            return Err(unavailable());
          }
          brought_back(state, id, &mut building).ok_or_else(unavailable)?;
        }
        None => {}
        Some(Ok(built)) => {
          admission
            .charge(u64::try_from(built.increment.post_state.len()).unwrap_or(u64::MAX))
            .map_err(|_| ImportFailed::Landing(past_room(wanted.room)))?;
          keep_work(state, id, mark.clone(), built.increment);
          break;
        }
        Some(Err(refused)) => {
          return Err(ImportFailed::Landing(work_refused(&refused, wanted.room)));
        }
      }
    }
  }
  Ok(())
}

/// `ImportRefused { PastRoom }` naming the import's room.
fn past_room(room: u64) -> Refusal {
  Refusal::ImportRefused {
    fault: ImportFault::PastRoom { room },
  }
}

/// The wire's refusal for a bookmark's work not made: past the room; a path or a link's target slates cannot hold, or a
/// tree that holds a name twice, which is the repository's format (`Format`); a changeset or manifest the imported
/// index does not answer, which a landed import never meets (`ContentUnavailable`).
fn work_refused(refused: &WorkRefused, room: u64) -> Refusal {
  match refused {
    WorkRefused::Layout(LayoutRefusal::PastAdmission(_))
    | WorkRefused::Push(PushRefused::PastRoom { .. }) => past_room(room),
    WorkRefused::Push(
      PushRefused::NameNotHeld { .. }
      | PushRefused::TargetNotHeld { .. }
      | PushRefused::DuplicateName { .. }
      | PushRefused::KindChanged { .. },
    ) => Refusal::ImportRefused {
      fault: ImportFault::Format,
    },
    _ => Refusal::ContentUnavailable,
  }
}

/// The wire's refusal for a landing's append refused: the entry's or the repository's own, or the first change's.
fn landing_refusal(refused: AppendRefused) -> Refusal {
  match refused {
    AppendRefused::Repository(refusal) | AppendRefused::Entry(refusal) => refusal,
    AppendRefused::Changes(changes) => changes
      .into_iter()
      .find_map(|change| match change.why {
        ChangeRefusal::Refused(refusal) => Some(refusal),
        _ => None,
      })
      .unwrap_or(Refusal::ContentUnavailable),
  }
}

/// A segment's landing on the changeset store under way while the import goes on (main 01a20182's ruling of 2026-11-24:
/// the 57k import at 1227afcb1 waited 20.3 s, about 18 % of it, for 2,087 landings awaited one by one, each its one
/// durable flush): the store's answer to come, the segment's sequence and payload identity, and when it was asked.
pub(crate) struct SegmentInFlight {
  /// Whether the store landed it, once it answers.
  answer: core::pin::Pin<Box<dyn core::future::Future<Output = bool>>>,
  /// Its changeset's sequence.
  sequence: u64,
  /// Its payload's identity.
  identity: [u8; 32],
  /// When it was asked, on the shard's clock.
  began: u64,
}

/// Repository `id`'s next segment cut and not yet handed to the store, its payload made into chunks a turn's steps at a
/// time ([`crate::repository::hg_push::chunked_paced`]) and its landing begun; none where it has none; refused, as a
/// landing the store refused is ([`segment_landed`]), where its chunks could not be made. `None` when the shard's state
/// is gone.
async fn landing_begun(id: RepositoryId) -> Option<Result<Option<SegmentInFlight>, ImportFailed>> {
  let Some((disk, segment)) = crate::state::with_state(|state| next_segment(state, id))? else {
    return Some(Ok(None));
  };
  let mut costs = super::TurnCosts::ZERO;
  let chunks =
    crate::repository::hg_push::chunked_paced((&segment.texts, &segment.payload), &mut costs)
      .await?;
  let landing = crate::state::with_state(|state| match chunks {
    Some(chunks) => segment_parts(state, id, (disk, segment), chunks).ok_or(()),
    None => Err(()),
  })?;
  let Ok((disk, header, parts, identity)) = landing else {
    return crate::state::with_state(|state| {
      segment_landed(state, id, (0, [0; 32]), false).map(|()| None)
    });
  };
  Some(Ok(Some(SegmentInFlight {
    sequence: header.sequence,
    identity,
    began: slates_rt::futures::now_ns(),
    answer: Box::pin(async move { disk.land(header, parts).await.is_ok() }),
  })))
}

/// `landing` answered `landed` (`segment_landed`): its time counted, and the time the import was held waiting for it,
/// where `waited`, else counted overlapped with the import's work.
fn landing_settled(
  id: RepositoryId,
  landing: &SegmentInFlight,
  (landed, waited): (bool, Option<u64>),
) -> Option<Result<(), ImportFailed>> {
  let took = slates_rt::futures::now_ns().saturating_sub(landing.began);
  crate::state::with_state(|state| {
    state.count(SEGMENT_LAND_NS, took);
    match waited {
      Some(waited) => {
        state.count(SEGMENT_LAND_WAIT_NS, waited);
        state.count_max(SEGMENT_LAND_WAIT_MAX_NS, waited);
      }
      None => state.count(SEGMENTS_LANDED_OVERLAPPED, 1),
    }
    segment_landed(state, id, (landing.sequence, landing.identity), landed)
  })
}

/// Waits for the landing in `flight`, if any, to answer, the import held meanwhile, and settles it.
pub(crate) async fn landed_in_flight(
  id: RepositoryId,
  flight: &mut Option<SegmentInFlight>,
) -> Option<Result<(), ImportFailed>> {
  let Some(mut landing) = flight.take() else {
    return Some(Ok(()));
  };
  let held = slates_rt::futures::now_ns();
  let landed = landing.answer.as_mut().await;
  let waited = slates_rt::futures::now_ns().saturating_sub(held);
  landing_settled(id, &landing, (landed, Some(waited)))
}

/// Yields the shard, the landing in `flight` answering meanwhile if it can: settled then, overlapped with the import's
/// work, and `flight` left empty.
async fn yielded_beside(
  id: RepositoryId,
  flight: &mut Option<SegmentInFlight>,
) -> Option<Result<(), ImportFailed>> {
  let Some(landing) = flight.as_mut() else {
    slates_rt::futures::yield_now().await;
    return Some(Ok(()));
  };
  let raced =
    slates_rt::futures::race2(landing.answer.as_mut(), slates_rt::futures::yield_now()).await;
  match (raced, flight.take()) {
    (slates_rt::futures::Either::First(landed), Some(landing)) => {
      landing_settled(id, &landing, (landed, None))
    }
    (_, taken) => {
      *flight = taken;
      Some(Ok(()))
    }
  }
}

/// Hands the store the segments repository `id`'s import has cut, oldest first, one in flight at a time (A-767 C1 step 3,
/// pipelined): before the next is handed over, the one in flight is waited for, so the segments land in their order and
/// their texts go a segment at a time, on each one's own durable answer; the last one cut is left in `flight` while the
/// next turns run. Derived: the import then holds at most two segments' file texts, the one in flight and the one being
/// written, each under its bound and the text that crosses it.
async fn segments_handed(
  id: RepositoryId,
  flight: &mut Option<SegmentInFlight>,
) -> Option<Result<(), ImportFailed>> {
  loop {
    let cut = crate::state::with_state(|state| {
      state
        .vcs_stores
        .imports
        .get(&id)
        .is_some_and(|importing| !importing.segments.is_empty())
    })?;
    if !cut {
      return Some(Ok(()));
    }
    if let Err(failed) = landed_in_flight(id, flight).await? {
      return Some(Err(failed));
    }
    match landing_begun(id).await? {
      Ok(Some(landing)) => *flight = Some(landing),
      Ok(None) => return Some(Ok(())),
      Err(failed) => return Some(Err(failed)),
    }
  }
}

/// One step of repository `id`'s import ([`run`]): a yield, the landing in `flight` answering meanwhile if it can; one
/// turn on `clock` within `quantum_ns`; then the segments it cut handed to the store, the last left in flight. What the
/// turn answered, or `None` when no import of `id` is under way or the shard's state is gone.
pub(crate) async fn step(
  id: RepositoryId,
  flight: &mut Option<SegmentInFlight>,
  (clock, quantum_ns): (&mut dyn Clock, u64),
) -> Option<Result<Turned, ImportFailed>> {
  if let Err(failed) = yielded_beside(id, flight).await? {
    return Some(Err(failed));
  }
  let turned = crate::state::with_state(|state| turn(state, id, (clock, quantum_ns)))??;
  if turned.is_ok()
    && let Err(failed) = segments_handed(id, flight).await?
  {
    return Some(Err(failed));
  }
  Some(turned)
}

/// Runs repository `id`'s import to its end on its owner shard, a turn at a time (the module doc): each [`step`] yields,
/// then runs one turn within the shard's quantum on its clock, so the shard serves its inbox and pulse between turns,
/// and a segment's landing runs beside the turns after it. What the import found once done (its last landing answered
/// first), why it ended otherwise, or `None` when no import of `id` is under way or the shard's state is gone.
pub(crate) async fn run(id: RepositoryId) -> Option<Result<Box<Finished>, ImportFailed>> {
  let mut flight = None;
  loop {
    let quantum_ns = crate::state::with_state(|state| quantum_of(state))?;
    let turned = step(id, &mut flight, (&mut ShardClock, quantum_ns)).await?;
    let landed = landed_in_flight_unless_more(id, &mut flight, &turned).await?;
    match (turned, landed) {
      (_, Err(failed)) | (Err(failed), _) => return Some(Err(failed)),
      (Ok(Turned::More), Ok(())) => {}
      (Ok(Turned::Done(finished)), Ok(())) => return finished_import(id, finished).await,
    }
  }
}

/// Waits for the landing in `flight` once `turned` ends the import, done or refused, so none is left unanswered behind
/// it; nothing while it goes on.
async fn landed_in_flight_unless_more(
  id: RepositoryId,
  flight: &mut Option<SegmentInFlight>,
  turned: &Result<Turned, ImportFailed>,
) -> Option<Result<(), ImportFailed>> {
  if matches!(turned, Ok(Turned::More)) {
    return Some(Ok(()));
  }
  landed_in_flight(id, flight).await
}

/// A finished import's last steps. Its bookmarks' works first, a unit a turn (2b, main's ruling of 2026-11-22 02:52):
/// the landing builds none. Its record names its contents by identity (A-729 part 2), so its landing commits only once
/// the shard's image holding them is published (durable before named, A-727 slice b's barrier). One whose works or
/// image fail gives the import back.
async fn finished_import(
  id: RepositoryId,
  finished: Box<Finished>,
) -> Option<Result<Box<Finished>, ImportFailed>> {
  if let Err(failed) = built_works(id).await? {
    crate::state::with_state(|state| abandon(state, id))?;
    return Some(Err(failed));
  }
  let (shard, ticket) =
    crate::state::with_state(|state| (state.shard, super::durable::content_ticket(state)))?;
  let durable =
    super::durable::content_durable(shard, ticket, crate::fleet::live_budget().max_deadline_ns())
      .await;
  if durable.is_ok() {
    return Some(Ok(finished));
  }
  crate::state::with_state(|state| abandon(state, id))?;
  Some(Err(ImportFailed::Landing(Refusal::ContentUnavailable)))
}
