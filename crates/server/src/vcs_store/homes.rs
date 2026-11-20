//! Content homed by its identity across a node's shards (A-724 piece 5; §4.17.5 "Giant repositories"; main's ruling of
//! 2026-11-20 on piece 6; R8).
//!
//! **Why.** A repository's owner shard keeps its register, its ordered landing, its indexes' tails and fences and its
//! hot set (§4.10's family placement). Everything else of a repository need not live there: a monorepo's content past
//! one shard's budget would otherwise bound the repository by that shard, and its CPU by that shard's core. Content
//! outside the owner's hot set is homed by its identity instead: the content id ranks the node's shards by rendezvous
//! (`slates_db::register::rendezvous_ranked_by_key`, the one weight every placement in slates ranks by), and the first is
//! its home. The fleet's half (piece 6) ranks the region's members by the same weight, holding f+1 homes; within one
//! node one home, since every shard of a node shares its RAM and fails with it.
//!
//! **What is homed.** A window: content of at most one window (`ContentStore::put`), named by its plain BLAKE3, so a
//! sender knows its home before it sends. An object longer than a window is homed as its windows, each by its own
//! identity, so one large object spreads over the node's shards. A home keeps a window in its shard's one content store
//! (A-662), deduplicated by identity with what its own repositories hold, charged to its shard's memory budget as
//! retention as everything that store holds is (the store lent the room its shard could admit for the step, then
//! settled), held once a sender and freed with its last holder.
//!
//! **How it is reached.** By a cross-shard call judged in the home's own time (`crate::xshard::call_in_shard_time`,
//! A-432), the caller's budget its bound; a call to the caller's own shard runs at once, the same code at one shard and
//! many (R8). No catalog is consulted: the identity is the route (§2 ban 12), and no coordination call is made.
//!
//! **Refused** ([`HomeRefused`]): bytes past a window; a home whose content store refused (its budget); a home that did
//! not answer within the budget; an answer naming another identity than the bytes' (never in a correct home).

use slates_db::register::rendezvous_ranked_by_key;
use slates_vcs::{ContentId, ContentRefused, ContentStore};

use crate::state::ShardState;

/// Counter: windows a home kept for a sender. Format: a counter name.
pub(crate) const HOMED: &str = "vcs_store.homes.homed";
/// Counter: windows a home read back for a reader. Format: a counter name.
pub(crate) const READ: &str = "vcs_store.homes.read";
/// Counter: windows a home let go for a holder. Format: a counter name.
pub(crate) const LET_GO: &str = "vcs_store.homes.let_go";

/// Why a window was not homed, read or let go (module docs, "Refused").
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum HomeRefused {
  /// The bytes are longer than a window: home each window by its own identity.
  PastWindow,
  /// The home's content store refused (its budget, or a count exhausted).
  Content(ContentRefused),
  /// The home did not answer within the caller's budget.
  Unanswered,
  /// The home named another identity than the bytes': never in a correct home.
  Misnamed,
  /// The home holds no such content.
  NotHeld,
}

/// The shard of `shards` that is `identity`'s home (module docs): the first by rendezvous over the identity; none for
/// no shards.
pub(crate) fn home_of(shards: &[u16], identity: &ContentId) -> Option<u16> {
  let members: Vec<u64> = shards.iter().map(|shard| u64::from(*shard)).collect();
  rendezvous_ranked_by_key(&members, &identity.0)
    .first()
    .and_then(|member| u16::try_from(*member).ok())
}

/// Keeps the window `bytes` on this shard, its home, held once more for its sender: its identity. The shard's content
/// store is made if it has none (as a first repository makes it), lent the room the shard could admit for the step,
/// and settled after.
pub(crate) fn keep_here(state: &mut ShardState, bytes: &[u8]) -> Result<ContentId, HomeRefused> {
  if state.vcs_stores.content.is_none() {
    let window = state.store.content.chunk_bytes();
    let made = ContentStore::new(state.vcs_stores.charged, window).map_err(HomeRefused::Content)?;
    state.vcs_stores.content = Some(made);
  }
  let need = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
  super::lend(&mut state.store, &mut state.vcs_stores, need);
  let kept = state
    .vcs_stores
    .content
    .as_mut()
    .map(|content| content.put(bytes));
  super::settle(state);
  match kept {
    Some(Ok(identity)) => {
      state.count(HOMED, 1);
      Ok(identity)
    }
    Some(Err(ContentRefused::PastWindow { .. })) => Err(HomeRefused::PastWindow),
    Some(Err(refused)) => Err(HomeRefused::Content(refused)),
    None => Err(HomeRefused::NotHeld),
  }
}

/// The bytes of the window `identity` this shard keeps as its home, if it keeps it.
pub(crate) fn read_here(state: &mut ShardState, identity: &ContentId) -> Option<Vec<u8>> {
  let bytes = state
    .vcs_stores
    .content
    .as_ref()
    .and_then(|content| content.get(identity))
    .map(<[u8]>::to_vec)?;
  state.count(READ, 1);
  Some(bytes)
}

/// Lets go one holder of the window `identity` this shard keeps as its home; freed with its last, the charge settled.
pub(crate) fn let_go_here(state: &mut ShardState, identity: &ContentId) -> Result<(), HomeRefused> {
  let held = state
    .vcs_stores
    .content
    .as_ref()
    .is_some_and(|content| content.contains(identity));
  if !held {
    return Err(HomeRefused::NotHeld);
  }
  super::lend(&mut state.store, &mut state.vcs_stores, 0);
  let released = state
    .vcs_stores
    .content
    .as_mut()
    .map(|content| content.release(identity));
  super::settle(state);
  match released {
    Some(Ok(_)) => {
      state.count(LET_GO, 1);
      Ok(())
    }
    Some(Err(refused)) => Err(HomeRefused::Content(refused)),
    None => Err(HomeRefused::NotHeld),
  }
}

/// Homes the window `bytes` from shard `origin` at its home among `shards` (module docs), waiting for the home at most
/// `budget_ns` of its own time: the window's identity, held once more for the caller.
pub(crate) async fn home(
  origin: u16,
  shards: &[u16],
  bytes: Vec<u8>,
  budget_ns: u64,
) -> Result<ContentId, HomeRefused> {
  let identity = ContentId::of(&bytes);
  let target = home_of(shards, &identity).ok_or(HomeRefused::Unanswered)?;
  let kept = crate::xshard::call_in_shard_time(
    origin,
    target,
    move |state| keep_here(state, &bytes),
    budget_ns,
  )
  .await
  .ok_or(HomeRefused::Unanswered)??;
  if kept == identity {
    Ok(identity)
  } else {
    Err(HomeRefused::Misnamed)
  }
}

/// The bytes of the window `identity` from its home among `shards`, read from shard `origin` within `budget_ns` of
/// the home's own time (module docs): none when it is not homed there or the home did not answer.
pub(crate) async fn read(
  origin: u16,
  shards: &[u16],
  identity: ContentId,
  budget_ns: u64,
) -> Option<Vec<u8>> {
  let target = home_of(shards, &identity)?;
  let bytes = crate::xshard::call_in_shard_time(
    origin,
    target,
    move |state| read_here(state, &identity),
    budget_ns,
  )
  .await
  .flatten()?;
  // A home answers the bytes it keeps under the identity, which its store named by their hash; checked again here, so a
  // window read across shards is never other bytes than its name (§4.17.1 item 5's rule on arrival, kept on reading).
  (ContentId::of(&bytes) == identity).then_some(bytes)
}

/// Lets go one holder of the window `identity` at its home among `shards`, from shard `origin` within `budget_ns` of
/// the home's own time.
pub(crate) async fn let_go(
  origin: u16,
  shards: &[u16],
  identity: ContentId,
  budget_ns: u64,
) -> Result<(), HomeRefused> {
  let target = home_of(shards, &identity).ok_or(HomeRefused::Unanswered)?;
  crate::xshard::call_in_shard_time(
    origin,
    target,
    move |state| let_go_here(state, &identity),
    budget_ns,
  )
  .await
  .ok_or(HomeRefused::Unanswered)?
}

#[cfg(test)]
mod tests;
