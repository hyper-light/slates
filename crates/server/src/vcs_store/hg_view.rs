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
//! **Publishing.** A repository's record carries no `phases.publish` yet, so every repository publishes, as a
//! Mercurial repository does by default (§4.17.3 decision 3).
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

/// Decided (module docs, "Publishing"): whether a repository publishes, until its record carries 7.2's `phases.publish`;
/// a push lands on the same terms (`hg_push::unbundle`).
pub(crate) const PUBLISHING: bool = true;

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

/// Counter: the asks a fetch's streamed answer made of the node's changeset store ([`answer_store_wants`]), each a
/// batch of rows or of chunks. Format: a counter name in the status report.
pub(crate) const STORE_ASKS: &str = "vcs_store.hg_stream_store_asks";

/// Counter: the file revisions a pull sent whole against the null base, each a text the store made from its chain since
/// its stored base was neither on the client nor sent earlier in the answer (A-767 C1 step 1b, main's ruling of
/// 2026-11-21 08:45; `slates_hg::serve::getbundle::Pull`). Format: a counter name in the status report.
pub(crate) const PULL_WHOLE_TEXTS: &str = "vcs_store.hg_pull_whole_texts";

/// Counter: the asks a `lookup` or a `batch` made of the node's changeset store for the heads' `.hgtags` (A-767 C1,
/// main's ruling (A) of 2026-11-21; `slates_hg::serve::lookup::TagsRead`), each a want of rows or of texts. Format: a
/// counter name in the status report.
pub(crate) const LOOKUP_STORE_ASKS: &str = "vcs_store.hg_lookup_store_asks";

/// Counter: the nanoseconds a fetch's streamed answer waited on the changeset store's answers, summed over its asks
/// ([`STORE_ASKS`]). Format: a counter name in the status report.
pub(crate) const STORE_WAIT_NS: &str = "vcs_store.hg_stream_store_wait_ns";

/// Counts one turn of a fetch's streamed answer ([`STREAM_TURNS`]) on the shard serving it; nothing off a shard.
pub(crate) fn turn_taken() {
  let _ = crate::state::with_state(|state| state.count(STREAM_TURNS, 1));
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
  use slates_hg::render::Want;
  let repository = u128::from_be_bytes(id.bytes);
  while let Some(want) = wants.wants().cloned() {
    let asked_at = slates_rt::futures::now_ns();
    let disk = crate::state::with_state(|state| state.vcs_disk.clone()).flatten();
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
    let Some(whole) = answer_one(&disk, repository, want, wants).await else {
      return false;
    };
    let waited = slates_rt::futures::now_ns().saturating_sub(asked_at);
    let (tags, bases) = (wants.reads_tags(), wants.reads_bases());
    let _ = crate::state::with_state(|state| {
      state.count(STORE_ASKS, 1);
      state.count(STORE_WAIT_NS, waited);
      if tags {
        state.count(LOOKUP_STORE_ASKS, 1);
      } else if bases {
        state.count(INTAKE_BASE_ASKS, 1);
      } else {
        state.count(PULL_WHOLE_TEXTS, whole);
      }
    });
  }
  true
}

/// Counter: the asks a push's intake (or its replay) made of the node's changeset store for a file delta's base landed
/// before the push (A-767 C1, main's ruling (b2); `slates_hg::push::intake::Stepped::Store`), each a want of rows or of
/// texts. Format: a counter name in the status report.
pub(crate) const INTAKE_BASE_ASKS: &str = "vcs_store.hg_intake_base_asks";

/// [`answer_store_wants`] over `disk` alone, touching no shard state (a recovery's replay holds the state across its
/// waits): whether every want was answered.
pub(crate) async fn answer_wants_over(
  disk: &slates_store::owner::StoreHandle,
  repository: u128,
  wants: &mut dyn slates_hg::render::StoreWants,
) -> bool {
  while let Some(want) = wants.wants().cloned() {
    if answer_one(disk, repository, want, wants).await.is_none() {
      return false;
    }
  }
  true
}

/// Answers one `want` of `repository` from `disk` into `wants`: the whole texts it gave (zero for any other want), or
/// none where the store refused or was gone.
async fn answer_one(
  disk: &slates_store::owner::StoreHandle,
  repository: u128,
  want: slates_hg::render::Want,
  wants: &mut dyn slates_hg::render::StoreWants,
) -> Option<u64> {
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
      Some(0)
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
      Some(0)
    }
    Want::Texts { texts, most } => match texts_made(disk, repository, &texts, most).await? {
      MadeTexts::Past => {
        wants.texts_past();
        Some(0)
      }
      MadeTexts::Made(made) => {
        let whole = u64::try_from(made.len()).unwrap_or(u64::MAX);
        wants.give_texts(made);
        Some(whole)
      }
    },
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

/// The full texts `texts` (each by its identity) of `repository`, made by the store's owner from their chains and read
/// a piece at a time to their end (`StoreHandle::text`, empty at a text's end; A-767 C1: a pull's revision sent whole,
/// a lookup's `.hgtags`), together at most `most` bytes: past them the reading stops, so a hostile or huge text is
/// never held whole ([`MadeTexts::Past`]). None where the store keeps no such text.
pub(crate) async fn texts_made(
  disk: &slates_store::owner::StoreHandle,
  repository: u128,
  texts: &[[u8; 32]],
  most: u64,
) -> Option<MadeTexts> {
  let mut made = Vec::new();
  made.try_reserve_exact(texts.len()).ok()?;
  let mut total = 0_u64;
  for identity in texts {
    let mut text = Vec::new();
    loop {
      let at = u64::try_from(text.len()).ok()?;
      let piece = disk.text(repository, *identity, at).await.ok()??;
      if piece.is_empty() {
        break;
      }
      total = total.saturating_add(u64::try_from(piece.len()).ok()?);
      if total > most {
        return Some(MadeTexts::Past);
      }
      text.extend_from_slice(&piece);
    }
    made.push(text);
  }
  Some(MadeTexts::Made(made))
}

/// [`held`], for the tests.
#[cfg(test)]
pub(crate) fn held_here(state: &ShardState, id: RepositoryId) -> bool {
  held(state, id)
}

/// A fetch's hold on the generation of a repository's index it began on (A-745, §4.17.1 "A fetch's answer" rule 4): a
/// fork of the index (its rows as they stood, their tables shared) and the register's phase roots and bookmarks as they
/// stood, so every turn of the fetch's answer reads the view its first turn read, whatever lands, is re-based or is
/// rebuilt meanwhile. Given back by its drop, on the shard that holds its store; a refused release counted.
pub(crate) struct Generation {
  /// The repository.
  id: RepositoryId,
  /// The fork of its index, until it is given back.
  index: Option<HgIndex>,
  /// The register's phase roots as they stood.
  roots: Vec<PhaseHead>,
  /// The register's bookmarks as they stood.
  bookmarks: BookmarkHeads,
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
  let (roots, bookmarks) = register_of(state, id, room).ok()?;
  let index = super::within_budget(state, 0, |stores| {
    let (content, store) = stores.mercurial(id)?;
    store.index.fork(content).ok()
  })?;
  Some(Generation {
    id,
    index: Some(index),
    roots,
    bookmarks,
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
    let read = State {
      roots: &generation.roots,
      bookmarks: &generation.bookmarks,
      publishing: PUBLISHING,
    };
    let view = Indexed::new(index, content, read, room).ok()?;
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
  let (roots, bookmarks) = register_of(shard, id, room).ok()?;
  let read = State {
    roots: &roots,
    bookmarks: &bookmarks,
    publishing: PUBLISHING,
  };
  let view = Indexed::new(&store.index, content, read, room).ok()?;
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
  let (roots, bookmarks) = register_of(shard, id, room).ok()?;
  let read = State {
    roots: &roots,
    bookmarks: &bookmarks,
    publishing: PUBLISHING,
  };
  let view = Indexed::new(&store.index, content, read, room).ok()?;
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

/// The phase roots and bookmarks repository `id`'s register holds now, copied and charged to `room` (module docs): a
/// root whose phase or node Mercurial does not have, a name under the bookmarks' tag that no bookmark's name is, and a
/// bookmark whose green names no head are not read, since the register holds none of them for a push it took.
fn register_of(
  state: &ShardState,
  id: RepositoryId,
  room: &mut Admission,
) -> Result<(Vec<PhaseHead>, BookmarkHeads), PastAdmission> {
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
  Ok((roots, bookmarks))
}

#[cfg(test)]
#[path = "hg_view/tests.rs"]
mod tests;
