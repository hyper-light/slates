//! An owner's verb that names a Mercurial changeset by a tag reads the heads' `.hgtags` from the node's changeset store
//! first, between turns (A-767 C1, main's ruling (A) of 2026-11-21: one read path, no state that grows; R6, everything
//! async): A-575's `CreateBranch --from <name>`, whose name reaches the tag namespace
//! (`slates_hg::serve::lookup::needs_tags`).
//!
//! **How.** The verb's door (`verbs::run_recorded`, beside the green's materialization door): where the name reaches
//! the tags, the view's heads name a `.hgtags` revision, and no read of those revisions is held, the request parks
//! ([`park_request`]) and a task of the shard ([`read`]) asks the store the rows of every head's `.hgtags` revision in
//! one want, then their texts in one, within the room the repository's share admits (`slates_hg::serve::lookup::TagsRead`,
//! answered by `hg_view::answer_store_wants`, counted `vcs_store.hg_lookup_store_asks`). Then, in one step, the read is
//! held, every request parked on it runs again (`verbs::resume_all`, each recorded as `run_recorded` records it) and
//! the read is let go: what the shard keeps between steps is the parked requests and one read under way a repository,
//! never a text after its step. A request that runs again where the heads' `.hgtags` changed meanwhile (a push landed)
//! parks again for a new read.
//!
//! **Refused.** A store that cannot answer the read answers every request parked on it `ContentUnavailable`,
//! unrecorded, so its retry runs ([`crate::verbs::answer_refused`]); at the parked bound (the green door's: the
//! shard's client seats over its region's slots) a request is refused `Overloaded`; a `.hgtags` past the read's room
//! is refused by the lookup past its admission, as the verb's other refusals are.

use std::collections::BTreeMap;

use slates_db::catalog::RepositoryId;
use slates_hg::serve::lookup::{TagsRead, needs_tags};
use slates_ipc::protocol::{Refusal, ReplyBody, RequestBody};

use crate::state::ShardState;
use crate::verbs::ParkedSubmit;

/// Counter: a request parked on a read of its repository's `.hgtags`. Format: a counter name in the daemon's status
/// report.
const PARKED: &str = "vcs_store.hg_tags.parked";
/// Counter: a request refused at the parked bound, or a read whose task could not start. Format: a counter name in the
/// daemon's status report.
const PARK_REFUSED: &str = "vcs_store.hg_tags.park_refused";
/// Counter: a read of a repository's `.hgtags` the store could not answer. Format: a counter name in the daemon's
/// status report.
const UNREAD: &str = "vcs_store.hg_tags.unread";

/// The shard's reads of Mercurial repositories' `.hgtags` under way, and the requests parked on them.
#[derive(Default)]
pub(crate) struct HgTags {
  /// Each repository's read under way, before its task takes it.
  begun: BTreeMap<RepositoryId, TagsRead>,
  /// Each repository's requests waiting for its read.
  parked: BTreeMap<RepositoryId, Vec<ParkedSubmit>>,
  /// Each repository's read done, held only within the step its parked requests run again in.
  read: BTreeMap<RepositoryId, TagsRead>,
}

/// The tags `name` needs over repository `id`'s served view, within `room`: none where it resolves before the tags or
/// the heads name no `.hgtags` revision ([`Owed::Ready`] then holds an empty read); the read held where it names the
/// revisions the heads name now; else the read to begin.
pub(crate) enum Owed {
  /// The tags the name resolves with, if it reaches them.
  Ready(Option<TagsRead>),
  /// A read to begin before the name resolves.
  Read(TagsRead),
}

/// What `name` needs of repository `id`'s `.hgtags` now (module docs); none where no view is lent.
pub(crate) fn owed(
  state: &mut ShardState,
  id: RepositoryId,
  name: &[u8],
  room: &mut slates_hg::Admission,
) -> Option<Result<Owed, slates_hg::serve::Refusal>> {
  let held = state.hg_tags.read.get(&id).cloned();
  super::hg_view::lend_in(state, id, room, |view, room| {
    if !needs_tags(view, name) {
      return Ok(Owed::Ready(None));
    }
    let begun = TagsRead::begin(view, room)?;
    if begun.wants().is_none() {
      return Ok(Owed::Ready(Some(begun)));
    }
    Ok(match held.filter(|held| held.same_files(&begun)) {
      Some(held) => Owed::Ready(Some(held)),
      None => Owed::Read(begun),
    })
  })
}

/// The repository a request names by a tag that must be read first, and the read to begin (the door, module docs);
/// none for any other request.
pub(crate) fn owing(
  state: &mut ShardState,
  body: &RequestBody,
) -> Option<(RepositoryId, TagsRead)> {
  let RequestBody::CreateBranch {
    repository, from, ..
  } = body
  else {
    return None;
  };
  let record = state
    .db
    .partition()
    .repositories()
    .repository_named(repository)
    .cloned()?;
  if !crate::repository::named_as_mercurial_resolves(&record, from) {
    return None;
  }
  let mut room = slates_hg::Admission::new(crate::vcs_share::room(state, record.id).bytes);
  match owed(state, record.id, from.as_bytes(), &mut room)? {
    Ok(Owed::Read(begun)) => Some((record.id, begun)),
    Ok(Owed::Ready(_)) | Err(_) => None,
  }
}

/// Parks the running recorded request `body`, by `principal`, on repository `id`'s `.hgtags` read, beginning it with
/// `begun` where none is under way: no reply now, the request run again and answered on its route once the read is
/// done. `Overloaded` with no recorded request to park, at the bound, or where the read's task cannot start.
pub(crate) fn park_request(
  state: &mut ShardState,
  (id, begun): (RepositoryId, TagsRead),
  (client_id, principal, body): (u32, &slates_db::catalog::Principal, RequestBody),
) -> Option<ReplyBody> {
  let shard = state.shard;
  let Some((origin, request)) = state.current_request else {
    state.count(PARK_REFUSED, 1);
    return Some(crate::verbs::refused(Refusal::Overloaded { shard }));
  };
  let bound = state
    .config
    .clients_per_shard
    .saturating_mul(usize::try_from(state.config.region.slots).unwrap_or(1));
  let held: usize = state.hg_tags.parked.values().map(Vec::len).sum();
  if held >= bound {
    state.count(PARK_REFUSED, 1);
    return Some(crate::verbs::refused(Refusal::Overloaded { shard }));
  }
  if !state.hg_tags.parked.contains_key(&id) {
    let started = slates_rt::futures::spawn(read(id))
      .and_then(slates_rt::futures::detach)
      .is_ok();
    if !started {
      state.count(PARK_REFUSED, 1);
      return Some(crate::verbs::refused(Refusal::Overloaded { shard }));
    }
    state.hg_tags.begun.insert(id, begun);
  }
  let route = state.reply_route.take();
  let parked = crate::verbs::parked_request(
    (origin, request),
    route,
    (client_id, principal.clone(), body),
    state,
  );
  state.hg_tags.parked.entry(id).or_default().push(parked);
  state.count(PARKED, 1);
  None
}

/// The read of repository `id`'s `.hgtags` (module docs): its wants answered by the node's changeset store, then, in
/// one step, the requests parked on it run again over it, or refused where the store could not answer.
async fn read(id: RepositoryId) {
  let begun = crate::state::with_state(|state| state.hg_tags.begun.remove(&id)).flatten();
  let Some(mut tags) = begun else {
    return;
  };
  let answered = super::hg_view::answer_store_wants(&mut tags, id).await;
  let _ = crate::state::with_state(|state| {
    let parked = state.hg_tags.parked.remove(&id).unwrap_or_default();
    if answered {
      state.hg_tags.read.insert(id, tags);
      crate::verbs::resume_all(state, parked);
      state.hg_tags.read.remove(&id);
    } else {
      state.count(UNREAD, 1);
      for request in parked {
        let _handed = crate::verbs::answer_refused(state, request, Refusal::ContentUnavailable);
      }
    }
  });
}

/// A request's door run in a test's synchronous history, which holds the shard so no task of it runs: the tags its name
/// needs read from the node's store and waited for on a thread of its own (`test_store::answer_wants_blocking`), held
/// while the request is dispatched over them as [`read`] runs it again, then let go. Its reply, and whether a read was
/// made.
#[cfg(test)]
pub(crate) fn dispatch_blocking(
  state: &mut ShardState,
  principal: &slates_db::catalog::Principal,
  body: RequestBody,
) -> (ReplyBody, bool) {
  let Some((id, mut begun)) = owing(state, &body) else {
    return (crate::verbs::dispatch(state, 1, principal, body), false);
  };
  let answered = state.vcs_disk.clone().is_some_and(|disk| {
    crate::test_store::answer_wants_blocking(&disk, u128::from_be_bytes(id.bytes), &mut begun)
  });
  assert!(
    answered,
    "the store answered the .hgtags read: left {:?}",
    begun.wants()
  );
  state.hg_tags.read.insert(id, begun);
  let reply = crate::verbs::dispatch(state, 1, principal, body);
  state.hg_tags.read.remove(&id);
  (reply, true)
}
