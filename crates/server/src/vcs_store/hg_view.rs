//! A Mercurial repository's served view, lent from A-662's map on its owner shard (A-680 part 2; §4.17.3 "Pull and
//! clone" items 1 and 2, "Access" last item and piece 4): the [`HgStores`] both edges reach a repository's store
//! through (the HTTP site, `crate::vcs_http::hg`; the SSH site's `hg serve --stdio`, A-679), made afresh for each step
//! from what the shard holds then, so nothing of the shard is held across a wait, as git's sites are lent theirs.
//!
//! **What a view is made of.** The repository's index and the shard's content store, from its store in the map
//! ([`super::VcsStores::mercurial_ref`]); and its register's state at that instant: its phase roots (the phases log,
//! A-642), its bookmarks (each `bookmark:<label>:` name of its namespace with the node its green's head names, A-657)
//! and whether it publishes. The view is `slates_hg::serve::indexed::Indexed` (A-680 part 1): what 7.2's `served`
//! filter serves of them. The register is the record: an imported repository's phase roots and bookmarks are served
//! once its import records them there (A-687), and every changeset is public until then.
//!
//! **Publishing.** Whether a repository publishes is its policy's `hg_publishing` (7.2's `phases.publish`, §4.17.3
//! decision 3; true by default, as 7.2's is), copied with the register's state, so a view and the push judged over it
//! read the same answer (`Repository::publishing`).
//!
//! **Not lent.** A repository whose store the map does not hold here; one an import is filling, whose index is put in
//! place when the import ends (A-675), so no client is served a history that is half there; and one whose view cannot
//! be made: a marker or a changelog row the store cannot read, or a view past the request's room, each counted
//! ([`UNMADE`]). The site answers each `503`.
//!
//! **Bounds.** The register's roots and bookmarks a view reads are copied, each list charged to the request's room
//! before it is made, as the view charges its own state there.
//!
//! **Pushes** (A-680 part 2b). The lender lands a push too ([`HgStores::push`]): on the same terms (not while an import
//! fills the store, an unmade view counted), the shard's state is handed to `crate::repository::hg_push::unbundle`,
//! which judges the push over the view [`with_view`] makes and lands it into the repository's store in the map, both in
//! one step, so nothing moves between the view the push read and its entry.

use core::mem::size_of;

use slates_db::catalog::RepositoryId;
use slates_hg::index::HgIndex;
use slates_hg::phases::{Phase, PhaseHead};
use slates_hg::push::Outcome;
use slates_hg::push::held::IndexHeld;
use slates_hg::serve::indexed::{Indexed, State};
use slates_hg::{Admission, Node, PastAdmission};

use slates_vcs::ContentStore;

use crate::repository::hg_push::{self, BOOKMARK_TAG, Pushing, bookmark_of};
use crate::state::ShardState;
use crate::vcs_http::hg::HgStores;

/// Counter: a Mercurial repository whose view could not be made (a marker or changelog row its store cannot read, or a
/// view past the request's room), answered as not served. Format: a counter name in the status report.
pub(crate) const UNMADE: &str = "vcs_store.hg_view.unmade";

/// The daemon's lender (module docs): the served views of the Mercurial repositories this shard's map holds.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct ServedViews;

impl HgStores for ServedViews {
  type Store<'s> = Indexed<'s>;
  type Hold = Generation;

  fn hold(&mut self, id: RepositoryId, room: &mut Admission) -> Option<Generation> {
    crate::state::with_state(|state| hold_in(state, id, room)).flatten()
  }

  fn lend_held<R>(
    &mut self,
    hold: &Generation,
    room: &mut Admission,
    step: impl for<'s> FnOnce(&Self::Store<'s>, &mut Admission) -> R,
  ) -> Option<R> {
    crate::state::with_state(|state| lend_held_in(state, hold, room, step)).flatten()
  }

  fn wrote(&mut self, hold: &Generation, bytes: u64) {
    hold.written.set(hold.written.get().saturating_add(bytes));
  }

  fn holds(&self, id: RepositoryId) -> bool {
    crate::state::with_state(|state| held(state, id)).unwrap_or(false)
  }

  fn lend<R>(
    &mut self,
    id: RepositoryId,
    room: &mut Admission,
    step: impl for<'s> FnOnce(&Self::Store<'s>, &mut Admission) -> R,
  ) -> Option<R> {
    crate::state::with_state(|state| lend_in(state, id, room, step)).flatten()
  }

  async fn push(
    &mut self,
    id: RepositoryId,
    pushing: Pushing<'_>,
    room: &mut Admission,
  ) -> Option<Outcome> {
    if !crate::state::with_state(|state| held(state, id)).unwrap_or(false) {
      return None;
    }
    let outcome = hg_push::unbundle(id, pushing, room).await;
    if outcome.is_none() {
      crate::state::with_state(|state| {
        crate::fleet::count_refusal_in(state, UNMADE);
      })
      .unwrap_or_default();
    }
    outcome
  }

  fn push_room(&self, id: RepositoryId) -> Option<u64> {
    crate::state::with_state(|state| crate::vcs_share::room(state, id).bytes)
  }

  async fn answer_wants(
    &mut self,
    (_, id, _): (&Generation, RepositoryId, u64),
    wants: &mut dyn slates_hg::render::StoreWants,
  ) -> bool {
    answer_store_wants(wants, id).await
  }

  fn fetch_turn(&self, id: RepositoryId) -> Option<(u64, u64)> {
    crate::state::with_state(|state| {
      (
        crate::vcs_share::room(state, id).bytes,
        crate::vcs_store::hg_import::quantum_of(state),
      )
    })
  }
}

/// Counter: the turns a fetch's streamed answer took over either edge (A-783's paced turn, [`super::paced_turn`]): a turn
/// yielding at its first quantum took one step, so a stream of hg-src was about 460,000 of them. Format: a counter name
/// in the status report.
pub(crate) const STREAM_TURNS: &str = "vcs_store.hg_stream_turns";
/// Counter: the most revisions one changeset's append walked through other branches to keep its branch's heads
/// (`slates_hg::index::BranchHeads::longest_walk`), read from each index a landing puts in place (main's order of
/// 2026-11-23: a walk down to an old head is one append's work; sliced if it runs past a step's worth). Format: a
/// counter name in the status report.
pub(crate) const BRANCH_HEADS_LONGEST_WALK: &str = "vcs.hg.branch_heads_longest_walk";

/// Notes the longest walk `index`'s branch heads took ([`BRANCH_HEADS_LONGEST_WALK`]), as a landing puts it in place.
pub(crate) fn walks_noted(state: &mut crate::state::ShardState, index: &slates_hg::index::HgIndex) {
  state.count_max(
    BRANCH_HEADS_LONGEST_WALK,
    index.branch_heads().longest_walk(),
  );
}

/// Counter: the asks a fetch's streamed answer made of the node's changeset store ([`answer_store_wants`]), each a
/// batch of rows or of chunks. Format: a counter name in the status report.
pub(crate) const STORE_ASKS: &str = "vcs_store.hg_stream_store_asks";

/// Counter: the file revisions a pull sent whole against the null base, each a text the store made from its chain since
/// its stored base was neither on the client nor sent earlier in the answer (A-767 C1 step 1b, main's ruling of
/// 2026-11-21 08:45; `slates_hg::serve::getbundle::Pull`). Format: a counter name in the status report.
pub(crate) const PULL_WHOLE_TEXTS: &str = "vcs_store.hg_pull_whole_texts";

/// Counter: the changelog's and the manifests' texts a fetch read whole from the node's changeset store, their windows
/// let go once landed (goal 93, C1 (1) P5; `slates_hg::serve::getbundle`'s `LogTexts`): a text a fetch reads to send a
/// changeset or diff a manifest, never a revision it sends whole, so counted apart from [`PULL_WHOLE_TEXTS`]. Format: a
/// counter name in the status report.
pub(crate) const PULL_LOG_TEXTS: &str = "vcs_store.hg_pull_log_texts";

/// Counter: the asks a `lookup` or a `batch` made of the node's changeset store for the heads' `.hgtags` (A-767 C1,
/// main's ruling (A) of 2026-11-21; `slates_hg::serve::lookup::TagsRead`), each a want of rows or of texts. Format: a
/// counter name in the status report.
pub(crate) const LOOKUP_STORE_ASKS: &str = "vcs_store.hg_lookup_store_asks";

/// Counter: the nanoseconds a fetch's streamed answer waited on the changeset store's answers, summed over its asks
/// ([`STORE_ASKS`]). Format: a counter name in the status report.
pub(crate) const STORE_WAIT_NS: &str = "vcs_store.hg_stream_store_wait_ns";

/// Counter: the nanoseconds a stream's count pass spent compressing chunks to size them (goal 93 perf, main's ruling of
/// 2026-11-25: each chunk compressed as stock's, at stream time until the store keeps it so; `slates_hg::revlog_chunk`).
/// Format: a counter name in the status report.
pub(crate) const COMPRESS_COUNT_NS: &str = "vcs_store.hg_stream_compress_count_ns";
/// Counter: the nanoseconds a stream's send pass spent compressing the chunks it wrote ([`COMPRESS_COUNT_NS`]'s
/// second pass). Format: a counter name in the status report.
pub(crate) const COMPRESS_SEND_NS: &str = "vcs_store.hg_stream_compress_send_ns";
/// Counter: the plain bytes of the chunks a stream's send pass compressed. Format: a counter name in the status report.
pub(crate) const COMPRESS_SEND_PLAIN_BYTES: &str = "vcs_store.hg_stream_compress_send_plain_bytes";
/// Counter: the bytes those chunks were stored as, headers in: against [`COMPRESS_SEND_PLAIN_BYTES`], the ratio the
/// stream gained. Format: a counter name in the status report.
pub(crate) const COMPRESS_SEND_STORED_BYTES: &str =
  "vcs_store.hg_stream_compress_send_stored_bytes";

/// Counter: the nanoseconds a fetch's streamed answer spent inside its turns, summed (main's hg leg at 608930048: a
/// 57k stream's 164 turns about 100 ms apart, its compression 1.6 s of 18 s; what a turn does, against what is between
/// turns). Format: a counter name in the status report.
pub(crate) const STREAM_TURN_NS: &str = "vcs_store.hg_stream_turn_ns";
/// Counter: the nanoseconds the writes of a fetch's streamed answer took, each to its peer taking its last byte, summed.
/// Format: a counter name in the status report.
pub(crate) const STREAM_WRITE_NS: &str = "vcs_store.hg_stream_write_ns";
/// Counter: the longest one such write took, kept as a maximum. Format: a counter name in the status report.
pub(crate) const STREAM_WRITE_MOST_NS: &str = "vcs_store.hg_stream_write_most_ns";
/// Counter: the writes that found the socket full and waited for the peer. Format: a counter name in the status report.
pub(crate) const STREAM_WRITE_WAITS: &str = "vcs_store.hg_stream_write_waits";

/// Counter: the nanoseconds a fetch's streamed answers took on the server, each from its beginning to its last turn,
/// summed: against [`STREAM_TURN_NS`], [`STORE_WAIT_NS`] and [`STREAM_WRITE_NS`], what lies between its turns. Format: a
/// counter name in the status report.
pub(crate) const STREAM_SPAN_NS: &str = "vcs_store.hg_stream_span_ns";

/// Counts a fetch's streamed answer's span ([`STREAM_SPAN_NS`]); nothing off a shard.
pub(crate) fn stream_spanned(ns: u64) {
  let _ = crate::state::with_state(|state| state.count(STREAM_SPAN_NS, ns));
}

/// Counter: the steps a fetch's streamed answer took in its turns, summed (each an index entry, a data chunk, a count
/// of a log, a file's head): against [`STREAM_TURN_NS`], a step's cost. Format: a counter name in the status report.
pub(crate) const STREAM_STEPS: &str = "vcs_store.hg_stream_steps";
/// Counter: the turns of a fetch's streamed answer that stopped at the pace's ceiling, no one waiting on the shard: a
/// turn as long as the ceiling allows, its work, not a wait. Format: a counter name in the status report.
pub(crate) const STREAM_TURNS_AT_CEILING: &str = "vcs_store.hg_stream_turns_at_ceiling";

/// Counts how long one turn of a fetch's streamed answer took ([`STREAM_TURN_NS`]), its steps ([`STREAM_STEPS`]) and
/// whether its pace stopped it at the ceiling ([`STREAM_TURNS_AT_CEILING`]); nothing off a shard.
pub(crate) fn turn_lasted(ns: u64, report: &slates_pace::Report) {
  let _ = crate::state::with_state(|state| {
    state.count(STREAM_TURN_NS, ns);
    state.count(STREAM_STEPS, report.units);
    if report.stop == Some(slates_pace::Stop::Ceiling) {
      state.count(STREAM_TURNS_AT_CEILING, 1);
    }
  });
}

/// Counts one write of a fetch's streamed answer: how long its peer took it ([`STREAM_WRITE_NS`], the most), and whether
/// it waited for the peer ([`STREAM_WRITE_WAITS`]); nothing off a shard.
pub(crate) fn write_taken(ns: u64, waited: bool) {
  let _ = crate::state::with_state(|state| {
    state.count(STREAM_WRITE_NS, ns);
    state.count_max(STREAM_WRITE_MOST_NS, ns);
    if waited {
      state.count(STREAM_WRITE_WAITS, 1);
    }
  });
}

/// Counts one turn of a fetch's streamed answer ([`STREAM_TURNS`]) on the shard serving it, and the chunks its thread
/// compressed since the last turn ([`COMPRESS_COUNT_NS`] and its siblings); nothing off a shard.
pub(crate) fn turn_taken() {
  let chunks = slates_hg::revlog_chunk::take_counts();
  let _ = crate::state::with_state(|state| {
    state.count(STREAM_TURNS, 1);
    for (name, by) in [
      (COMPRESS_COUNT_NS, chunks.count.ns),
      (COMPRESS_SEND_NS, chunks.send.ns),
      (COMPRESS_SEND_PLAIN_BYTES, chunks.send.plain_bytes),
      (COMPRESS_SEND_STORED_BYTES, chunks.send.stored_bytes),
    ] {
      if by > 0 {
        state.count(name, by);
      }
    }
  });
}

/// Answers what a stream's next step needs of the node's changeset store (goal 93 perf: each revision sent as the store
/// keeps it), over the shard's handle, as a task: a log's rows by node (`StoreHandle::texts_by_tool`, the `I` rows) and
/// a batch of chunks (`StoreHandle::places`, read in place). A daemon holding no store keeps no revision, so a rows'
/// want is answered none and the stream is not served (`1\n`). Whether every want was answered; false where the store
/// refused or was gone, which ends the answer.
pub(crate) async fn answer_store_wants(
  wants: &mut dyn slates_hg::render::StoreWants,
  id: RepositoryId,
) -> bool {
  answer_store_wants_on(&mut crate::vcs_http::git::ThisShard, wants, id).await
}

/// [`answer_store_wants`] over the shard's state as `lender` lends it a step at a time: this thread's (`ThisShard`), or
/// one a caller holds already (`vcs_store::HeldShard`, a recovery's).
pub(crate) async fn answer_store_wants_on<L, W>(
  lender: &mut L,
  wants: &mut W,
  id: RepositoryId,
) -> bool
where
  L: crate::vcs_http::git::ShardLender,
  W: slates_hg::render::StoreWants + ?Sized,
{
  use slates_hg::render::Want;
  let repository = u128::from_be_bytes(id.bytes);
  while let Some(want) = wants.wants().cloned() {
    let asked_at = slates_rt::futures::now_ns();
    let Some((disk, step)) =
      lender.lend(|state| (state.vcs_disk.clone(), text_bytes_a_step(state)))
    else {
      return false;
    };
    let Some(disk) = disk else {
      // A daemon holding no store keeps no revision: a rows' want is answered none, any other refused.
      let Want::Rows { logs } = want else {
        return false;
      };
      wants.give_rows(
        logs
          .iter()
          .map(|(_, nodes)| vec![None; nodes.len()])
          .collect(),
      );
      continue;
    };
    // Read before the answer, which ends the want it names.
    let logs = wants.reads_logs();
    let Some((whole, asks)) = answer_one(&disk, repository, (want, step), wants).await else {
      return false;
    };
    let waited = slates_rt::futures::now_ns().saturating_sub(asked_at);
    let owner = slates_store::owner::take_read_waits();
    let (tags, bases) = (wants.reads_tags(), wants.reads_bases());
    let _ = lender.lend(|state| {
      state.count(STORE_ASKS, 1);
      state.count(STORE_WAIT_NS, waited);
      count_owner_waits(state, &owner);
      state.count(TEXT_REQUESTS, asks.requests);
      state.count_max(TEXT_COPIED_MOST_BYTES, asks.most_copied);
      if tags {
        state.count(LOOKUP_STORE_ASKS, 1);
      } else if bases {
        state.count(INTAKE_BASE_ASKS, 1);
      } else if logs {
        state.count(PULL_LOG_TEXTS, whole);
      } else {
        state.count(PULL_WHOLE_TEXTS, whole);
      }
    });
  }
  true
}

/// Counter: the requests a fetch's store asks ([`STORE_ASKS`]) made of the node's changeset store's owner, each one
/// trip to the owner's thread and back (goal 93, main's order of 2026-11-25: the clone's store wait split by part; the
/// parts below sum to [`OWNER_TOTAL_NS`], and [`STORE_WAIT_NS`] past it is the asking shard's own work between its
/// requests). Format: a counter name in the status report.
pub(crate) const OWNER_REQUESTS: &str = "vcs_store.hg_owner_requests";

/// Counter: the nanoseconds those requests waited, each from its send to its answer's taking on the asking task.
/// Format: a counter name in the status report.
pub(crate) const OWNER_TOTAL_NS: &str = "vcs_store.hg_owner_total_ns";

/// Counter: of [`OWNER_TOTAL_NS`], the nanoseconds queued behind other requests the owner was serving when each was
/// sent (another repository's landing and its flush, another fetch's asks). Format: a counter name in the status report.
pub(crate) const OWNER_BEHIND_NS: &str = "vcs_store.hg_owner_behind_ns";

/// Counter: of [`OWNER_TOTAL_NS`], the owner thread's wake from waiting for work to taking the request. Format: a
/// counter name in the status report.
pub(crate) const OWNER_WAKE_NS: &str = "vcs_store.hg_owner_wake_ns";

/// Counter: of [`OWNER_TOTAL_NS`], the owner's service but its log reads: making texts from their chains, reading the
/// engine's rows, copying the answer. Format: a counter name in the status report.
pub(crate) const OWNER_SERVE_NS: &str = "vcs_store.hg_owner_serve_ns";

/// Counter: of [`OWNER_TOTAL_NS`], the changeset log's reads within the owner's service (`ChangesetLog::read_ns`).
/// Format: a counter name in the status report.
pub(crate) const OWNER_DISK_NS: &str = "vcs_store.hg_owner_disk_ns";

/// Counter: of [`OWNER_TOTAL_NS`], from the owner's answer to the asking task's taking it: the hop back to the asking
/// shard and that shard's own turn. Format: a counter name in the status report.
pub(crate) const OWNER_BACK_NS: &str = "vcs_store.hg_owner_back_ns";

/// Counter: the longest one request's whole wait ([`OWNER_TOTAL_NS`]'s), kept as a maximum. Format: a counter name in
/// the status report.
pub(crate) const OWNER_MOST_NS: &str = "vcs_store.hg_owner_most_ns";

/// Counter: the requests the store's owner met spinning, no wake (`slates_store::owner::StoreOwner::start`'s spin;
/// main's ruling of 2026-11-25, the 57k clone's wait 86.6% the owner's wake). Format: a counter name in the status report.
pub(crate) const OWNER_SPIN_HITS: &str = "vcs_store.hg_owner_spin_hits";
/// Counter: the requests the owner met parked, woken for them. Format: a counter name in the status report.
pub(crate) const OWNER_PARKS: &str = "vcs_store.hg_owner_parks";
/// Counter: the nanoseconds the owner spun waiting for those requests, hits and misses: the spin's price. Format: a
/// counter name in the status report.
pub(crate) const OWNER_SPIN_NS: &str = "vcs_store.hg_owner_spin_ns";

/// Counts what a want's requests of the store's owner waited on, split by part ([`OWNER_REQUESTS`] and the counters
/// after it).
fn count_owner_waits(state: &mut ShardState, owner: &slates_store::owner::AskWaits) {
  state.count(OWNER_REQUESTS, owner.asks);
  state.count(OWNER_TOTAL_NS, owner.total_ns);
  state.count(OWNER_BEHIND_NS, owner.behind_ns);
  state.count(OWNER_WAKE_NS, owner.wake_ns);
  state.count(OWNER_SERVE_NS, owner.serve_ns);
  state.count(OWNER_DISK_NS, owner.disk_ns);
  state.count(OWNER_BACK_NS, owner.back_ns);
  state.count_max(OWNER_MOST_NS, owner.most_ns);
  state.count(OWNER_SPIN_HITS, owner.spin_hits);
  state.count(OWNER_PARKS, owner.parks);
  state.count(OWNER_SPIN_NS, owner.spin_ns);
}

/// Counter: the requests a fetch's texts made of the node's changeset store's owner, each the texts of one want made
/// whole there (goal 93: a request a text, and a piece a request, before: over 100,000 for hg's 57k clone). Format: a
/// counter name in the status report.
pub(crate) const TEXT_REQUESTS: &str = "vcs_store.hg_text_requests";

/// Counter: the most bytes one request of a fetch's or a replay's texts copied on the store's owner (goal 93, main's
/// ruling of 2026-11-25): held to a step's copy budget ([`text_bytes_a_step`]), a text past it copied in slices; the
/// bytes a want past the step holds on the shard are still its one text, whole, as the changegroup sends it. Format: a
/// counter name in the status report.
pub(crate) const TEXT_COPIED_MOST_BYTES: &str = "vcs_store.hg_text_copied_most_bytes";

/// Counter: the asks a push's intake (or its replay) made of the node's changeset store for a file delta's base landed
/// before the push (A-767 C1, main's ruling (b2); `slates_hg::push::intake::Stepped::Store`), each a want of rows or of
/// texts. Format: a counter name in the status report.
pub(crate) const INTAKE_BASE_ASKS: &str = "vcs_store.hg_intake_base_asks";

/// [`answer_store_wants`] over `disk` alone, touching no shard state (a recovery's replay holds the state across its
/// waits), the owner copying at most `step` bytes a request (the replaying shard's [`text_bytes_a_step`]; goal 93,
/// main's ruling of 2026-11-25, the fetch's rule for a replay's base asks too): what it asked of the owner, all its wants
/// together; none where a want went unanswered.
pub(crate) async fn answer_wants_over(
  disk: &slates_store::owner::StoreHandle,
  repository: u128,
  wants: &mut dyn slates_hg::render::StoreWants,
  step: u64,
) -> Option<TextAsks> {
  let mut asked = TextAsks::default();
  while let Some(want) = wants.wants().cloned() {
    let (_, asks) = answer_one(disk, repository, (want, step), wants).await?;
    asked.requests = asked.requests.saturating_add(asks.requests);
    asked.most_copied = asked.most_copied.max(asks.most_copied);
  }
  Some(asked)
}

/// Answers one `want` of `repository` from `disk` into `wants`, the owner copying at most `step` bytes a request: the
/// whole texts it gave (zero for any other want) and what it asked of the owner, or none where the store refused or
/// was gone.
async fn answer_one<W: slates_hg::render::StoreWants + ?Sized>(
  disk: &slates_store::owner::StoreHandle,
  repository: u128,
  (want, step): (slates_hg::render::Want, u64),
  wants: &mut W,
) -> Option<(u64, TextAsks)> {
  use slates_hg::render::{StoredPlace, StoredRevision, Want};
  match want {
    Want::Rows { logs } => {
      let asked = logs
        .iter()
        .map(|(log, nodes)| {
          (
            log.clone(),
            nodes.iter().map(|node| node.to_vec()).collect(),
          )
        })
        .collect();
      let found = disk.texts_by_tool(repository, asked).await.ok()?;
      wants.give_rows(
        found
          .into_iter()
          .map(|log| {
            log
              .into_iter()
              .map(|row| {
                row.map(|(text, row)| StoredRevision {
                  text,
                  base: row.base,
                  place: StoredPlace {
                    payload: row.place.payload,
                    offset: row.place.offset,
                    len: row.place.len,
                  },
                })
              })
              .collect()
          })
          .collect(),
      );
      Some((0, TextAsks::default()))
    }
    Want::Chunks { places } => {
      let asked = places
        .iter()
        .map(|place| slates_store::texts::Place {
          payload: place.payload,
          offset: place.offset,
          len: place.len,
        })
        .collect();
      let chunks = disk.places(asked).await.ok()?;
      wants.give_chunks(chunks);
      Some((0, TextAsks::default()))
    }
    Want::Texts { texts, most } => {
      match texts_made(disk, repository, &texts, (most, step)).await? {
        (MadeTexts::Past, asks) => {
          wants.texts_past();
          Some((0, asks))
        }
        (MadeTexts::Made(made), asks) => {
          let whole = u64::try_from(made.len()).unwrap_or(u64::MAX);
          wants.give_texts(made);
          Some((whole, asks))
        }
      }
    }
  }
}

/// The full texts of a [`texts_made`] read: each made, or past the bytes they could hold together.
#[derive(Debug)]
pub(crate) enum MadeTexts {
  /// Each text, in the order asked.
  Made(Vec<Vec<u8>>),
  /// The texts held more than the bytes allowed: none read past them.
  Past,
}

/// What a [`texts_made`] read asked of the store's owner: its requests, and the most bytes one of them copied.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct TextAsks {
  /// The requests made of the owner.
  pub(crate) requests: u64,
  /// The most bytes one request's answer copied.
  pub(crate) most_copied: u64,
}

impl TextAsks {
  /// One request more, whose answer copied `bytes`.
  fn asked(&mut self, bytes: u64) {
    self.requests = self.requests.saturating_add(1);
    self.most_copied = self.most_copied.max(bytes);
  }
}

/// The full texts `texts` (each by its identity) of `repository`, made by the store's owner from their chains (A-767
/// C1: a fetch's let-go changelog and manifest texts, a pull's revision sent whole, a lookup's `.hgtags`), together at
/// most `most` bytes: past them the reading stops, so a hostile or huge text is never held whole ([`MadeTexts::Past`]).
/// The owner copies at most `step` bytes a request (a step's copy budget, [`text_bytes_a_step`]; goal 93, main's ruling
/// of 2026-11-25). None where the store keeps no such text. What it asked, beside them.
pub(crate) async fn texts_made(
  disk: &slates_store::owner::StoreHandle,
  repository: u128,
  texts: &[[u8; 32]],
  (most, step): (u64, u64),
) -> Option<(MadeTexts, TextAsks)> {
  let step = step.max(1);
  let mut asks = TextAsks::default();
  if most <= step {
    // The want within one step (a fetch's batch, `LogTexts::ask_batch`, holds a step's bytes): one request of the
    // store's owner for all of it (goal 93: a request a text and a piece before, over 100,000 trips to the owner for hg's
    // 57k clone), counted ([`TEXT_REQUESTS`]).
    let made = match disk.texts(repository, texts.to_vec(), most).await.ok()? {
      slates_store::owner::MadeTexts::Made(made) => made,
      slates_store::owner::MadeTexts::Past => {
        asks.asked(0);
        return Some((MadeTexts::Past, asks));
      }
      slates_store::owner::MadeTexts::Missing => return None,
    };
    let copied = made
      .iter()
      .map(|text| u64::try_from(text.len()).unwrap_or(u64::MAX))
      .fold(0, u64::saturating_add);
    asks.asked(copied);
    return Some((MadeTexts::Made(made), asks));
  }
  // Past a step: each text copied a step's slice a request, to its end (an empty slice, or one short of the step).
  let mut made = Vec::new();
  made.try_reserve_exact(texts.len()).ok()?;
  let mut total = 0_u64;
  for identity in texts {
    let mut text = Vec::new();
    loop {
      let at = u64::try_from(text.len()).ok()?;
      let slice = disk
        .text_slice(repository, *identity, (at, step))
        .await
        .ok()??;
      let len = u64::try_from(slice.len()).ok()?;
      asks.asked(len);
      total = total.saturating_add(len);
      if total > most {
        return Some((MadeTexts::Past, asks));
      }
      text.extend_from_slice(&slice);
      if len < step {
        break;
      }
    }
    made.push(text);
  }
  Some((MadeTexts::Made(made), asks))
}

/// The most bytes of texts one want of a fetch over the shard's view asks the store's owner to make and copy at once
/// (`slates_hg::serve::getbundle::Store::text_bytes_a_step`; goal 93, main's ruling of 2026-11-25): the copy budget of
/// one step on the calling shard, `copy_slice_bytes` (the machine's measured memcpy rate over the copy share of its
/// step quantum; 94,797 B on main's bench), one byte at least.
pub(crate) fn text_bytes_a_step(state: &ShardState) -> u64 {
  state.config.copy_slice_bytes().max(1)
}

/// [`held`], for the tests.
#[cfg(test)]
pub(crate) fn held_here(state: &ShardState, id: RepositoryId) -> bool {
  held(state, id)
}

/// A fetch's hold on the generation of a repository's index it began on (A-745, §4.17.1 "A fetch's answer" rule 4): a
/// fork of the index (its rows as they stood, their tables shared) and the register's roots, bookmarks and markers as they
/// stood, so every turn of the fetch's answer reads the view its first turn read, whatever lands, is re-based or is
/// rebuilt meanwhile. Given back by its drop, on the shard that holds its store; a refused release counted.
pub(crate) struct Generation {
  /// The repository.
  id: RepositoryId,
  /// The fork of its index, until it is given back.
  index: Option<HgIndex>,
  /// The register's phase roots, bookmarks and obsolescence markers as they stood.
  register: Register,
  /// The fetch counted live while its streamed answer holds this generation (A-724 piece 3d: a fetch's room is a claim
  /// of its own).
  _fetching: crate::vcs_share::Live,
  /// The bytes its answer has written so far, as its edge reports them (`HgStores::wrote`): learned as the repository's
  /// fetch demand once the answer ends.
  written: core::cell::Cell<u64>,
}

impl Generation {
  /// Gives the fork back on `state`, for a caller that holds the shard's state (the tests), which its drop cannot reach.
  #[cfg(test)]
  pub(crate) fn give_back(mut self, state: &mut ShardState) {
    if let Some(index) = self.index.take() {
      release_generation(state, index);
    }
  }
}

impl Drop for Generation {
  /// Gives the fork back and, where its answer wrote anything, answers the fetch on its repository's usage row with those
  /// bytes (A-724 piece 3d). A generation held for a batch's lookup writes nothing and teaches nothing.
  fn drop(&mut self) {
    let (id, written) = (self.id, self.written.get());
    let index = self.index.take();
    crate::state::with_state(|state| {
      if let Some(index) = index {
        release_generation(state, index);
      }
      if written > 0 {
        let now = slates_rt::futures::now_ns();
        state
          .vcs_usage
          .answered(&id, slates_vcs::Work::Fetch, written, now);
      }
    })
    .unwrap_or_default();
  }
}

/// Gives a generation's fork back, a refused release counted.
fn release_generation(state: &mut ShardState, index: HgIndex) {
  let released = super::within_budget(state, 0, |stores| {
    stores
      .content_mut()
      .map_or(Ok(()), |content| index.release(content))
  });
  if released.is_err() {
    state.count(super::RELEASE_REFUSED, 1);
  }
}

/// Counter: the run fences the generations of fetches copied as they forked a repository's index (`Index::fork` copies
/// every run's fences; goal 93, the index seal's reland). Format: a counter name in the status report.
pub(crate) const FORK_FENCES_COPIED: &str = "vcs.hg.fork_fences_copied";

/// Holds repository `id`'s generation as `state` holds it now ([`Generation`]), the register's copies charged to `room`;
/// none when it is not lent (module docs).
pub(crate) fn hold_in(
  state: &mut ShardState,
  id: RepositoryId,
  room: &mut Admission,
) -> Option<Generation> {
  if !held(state, id) {
    return None;
  }
  let register = register_of(state, id, room).ok()?;
  let (index, fences) = super::within_budget(state, 0, |stores| {
    let (content, store) = stores.mercurial(id)?;
    let fences = store.index.rows().fences();
    store.index.fork(content).ok().map(|fork| (fork, fences))
  })?;
  state.count(FORK_FENCES_COPIED, fences);
  Some(Generation {
    id,
    index: Some(index),
    register,
    _fetching: crate::vcs_share::Live::fetch(id),
    written: core::cell::Cell::new(0),
  })
}

/// Runs `step` over the served view of `generation`, made within `room`; none when its store is gone or the view
/// cannot be made, an unmade view counted.
pub(crate) fn lend_held_in<R>(
  state: &mut ShardState,
  generation: &Generation,
  room: &mut Admission,
  step: impl for<'s> FnOnce(&Indexed<'s>, &mut Admission) -> R,
) -> Option<R> {
  let index = generation.index.as_ref()?;
  let made = super::lent_state(state, |state| {
    let (content, _) = state.vcs_stores.mercurial_ref(generation.id)?;
    let read = generation.register.state();
    let view = Indexed::new(index, content, read, (text_bytes_a_step(state), room)).ok()?;
    Some(step(&view, room))
  });
  if made.is_none() {
    crate::fleet::count_refusal_in(state, UNMADE);
  }
  made
}

/// Whether repository `id`'s Mercurial store is held here with no import filling it.
fn held(state: &ShardState, id: RepositoryId) -> bool {
  state.vcs_stores.mercurial_ref(id).is_some()
    && !state.vcs_stores.importing(id)
    && !state.vcs_stores.rebuilding(id)
}

/// Runs `step` over repository `id`'s view, made within `room` from what `state` holds now; none when it is not lent
/// (module docs), an unmade view counted.
pub(crate) fn lend_in<R>(
  state: &mut ShardState,
  id: RepositoryId,
  room: &mut Admission,
  step: impl for<'s> FnOnce(&Indexed<'s>, &mut Admission) -> R,
) -> Option<R> {
  if !held(state, id) {
    return None;
  }
  let started = slates_rt::futures::now_ns();
  let made = super::lent_state(state, |state| {
    with_view(state, id, room, |view, _, room| step(view, room))
  });
  if made.is_none() {
    crate::fleet::count_refusal_in(state, UNMADE);
  }
  // The lend's lookups over the store's own index, reported to its merge rule (`super::index_upkeep`).
  super::index_upkeep::looked_up(
    state,
    id,
    slates_rt::futures::now_ns().saturating_sub(started),
  );
  made
}

/// The node `name` names in repository `id`'s served view as Mercurial resolves a name (A-575's `--from`;
/// `slates_hg::serve::lookup::resolve_name`), within `room`: none where no view is lent (module docs, "Not lent"),
/// counted as every lend counts it; the view's refusal where it could not resolve one.
pub(crate) fn resolved_in(
  state: &mut ShardState,
  id: RepositoryId,
  name: &[u8],
  room: &mut Admission,
) -> Option<Result<Option<Node>, slates_hg::serve::Refusal>> {
  let tags = match super::hg_tags::owed(state, id, name, room)? {
    Ok(super::hg_tags::Owed::Ready(tags)) => tags,
    // The verb's door reads the tags first (`super::hg_tags`); a dispatch that passed no door cannot wait for them.
    Ok(super::hg_tags::Owed::Read(_)) => {
      return Some(Err(slates_hg::serve::Refusal::Store {
        what: "a .hgtags text not read from the changeset store",
      }));
    }
    Err(refusal) => return Some(Err(refusal)),
  };
  lend_in(state, id, room, |view, room| {
    slates_hg::serve::lookup::resolve_name(view, tags.as_ref(), name, room)
  })
}

/// Runs `step` over repository `id`'s served view, made within `room` from what `shard` holds now, and what its index
/// holds as a push verifies against it (`IndexHeld`); none when its store is not in the map or the view cannot be made
/// (module docs, "Not lent"), which the caller counts.
pub(crate) fn with_view<R>(
  shard: &ShardState,
  id: RepositoryId,
  room: &mut Admission,
  step: impl for<'s> FnOnce(&Indexed<'s>, IndexHeld<'s>, &mut Admission) -> R,
) -> Option<R> {
  let (content, store) = shard.vcs_stores.mercurial_ref(id)?;
  let register = register_of(shard, id, room).ok()?;
  let read = register.state();
  let view = Indexed::new(
    &store.index,
    content,
    read,
    (text_bytes_a_step(shard), room),
  )
  .ok()?;
  Some(step(&view, IndexHeld::new(&store.index, content), room))
}

/// [`with_view`] with the texts read through `over` where it is given (A-720: a push's fork, which holds the
/// repository's revisions and the push's), the served view still the store's index as it is; `step` is handed the
/// content store too.
pub(crate) fn with_view_over<R>(
  shard: &ShardState,
  id: RepositoryId,
  over: Option<&HgIndex>,
  room: &mut Admission,
  step: impl for<'s> FnOnce(&Indexed<'s>, IndexHeld<'s>, &'s ContentStore, &mut Admission) -> R,
) -> Option<R> {
  let (content, store) = shard.vcs_stores.mercurial_ref(id)?;
  let register = register_of(shard, id, room).ok()?;
  let read = register.state();
  let view = Indexed::new(
    &store.index,
    content,
    read,
    (text_bytes_a_step(shard), room),
  )
  .ok()?;
  let held = IndexHeld::new(over.unwrap_or(&store.index), content);
  Some(step(&view, held, content, room))
}

/// `count` values of `T` charged to `room`, before they are made.
fn charge_each<T>(room: &mut Admission, count: usize) -> Result<(), PastAdmission> {
  let each = u64::try_from(size_of::<T>()).unwrap_or(u64::MAX);
  let count = u64::try_from(count).unwrap_or(u64::MAX);
  room.charge(each.saturating_mul(count))
}

/// Bookmarks: each its name and the node it names.
type BookmarkHeads = Vec<(Vec<u8>, Node)>;

/// What a repository's register holds that its served view reads, copied ([`register_of`]).
struct Register {
  /// The phase roots.
  roots: Vec<PhaseHead>,
  /// The bookmarks.
  bookmarks: BookmarkHeads,
  /// The obsolescence markers, each alone in its format, in the markers log's order.
  markers: Vec<Vec<u8>>,
  /// Whether the repository publishes (module docs, "Publishing").
  publishing: bool,
}

impl Register {
  /// The served view's state over these copies.
  fn state(&self) -> State<'_> {
    State {
      roots: &self.roots,
      bookmarks: &self.bookmarks,
      publishing: self.publishing,
      markers: &self.markers,
    }
  }
}

/// The phase roots, bookmarks and obsolescence markers repository `id`'s register holds now and whether it publishes,
/// copied and charged to `room` (module docs): a root whose phase or node Mercurial does not have, a name under the bookmarks' tag that no
/// bookmark's name is, and a bookmark whose green names no head are not read, since the register holds none of them
/// for a push it took.
fn register_of(
  state: &ShardState,
  id: RepositoryId,
  room: &mut Admission,
) -> Result<Register, PastAdmission> {
  let repositories = state.db.partition().repositories();
  charge_each::<PhaseHead>(room, repositories.phase_roots(id).count())?;
  let roots = repositories
    .phase_roots(id)
    .filter_map(|(phase, node)| {
      Some(PhaseHead {
        phase: Phase::from_number(i32::from(phase))?,
        node: Node::try_from(node).ok()?,
      })
    })
    .collect();
  charge_each::<(Vec<u8>, Node)>(room, repositories.branches(id, BOOKMARK_TAG).count())?;
  let mut bookmarks = Vec::new();
  for (name, green) in repositories.branches(id, BOOKMARK_TAG) {
    let head = super::hg_made::head_of(state, green);
    let (Some(bookmark), Some(head)) = (bookmark_of(name), head) else {
      continue;
    };
    room.charge(u64::try_from(bookmark.len()).unwrap_or(u64::MAX))?;
    bookmarks.push((bookmark, head));
  }
  charge_each::<Vec<u8>>(room, repositories.markers(id).count())?;
  let mut markers = Vec::new();
  for marker in repositories.markers(id) {
    room.charge(u64::try_from(marker.len()).unwrap_or(u64::MAX))?;
    markers.push(marker.to_vec());
  }
  // A repository whose record is gone has no store either, so no view reads this; 7.2's default is kept for it.
  let publishing = repositories
    .repository(id)
    .is_none_or(|record| record.policy.hg_publishing);
  Ok(Register {
    roots,
    bookmarks,
    markers,
    publishing,
  })
}

#[cfg(test)]
#[path = "hg_view/tests.rs"]
mod tests;
