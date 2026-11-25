//! What reads brought back from the node's store, by the repository that read them, let go past each repository's
//! grant (A-724 piece 4's reader; §4.17.5 "The cold trigger"; main 01a20182's ruling (b) of 2026-11-25).
//!
//! **Why.** A let-go window a read finds away is brought back from the node's store and served (a named read through a
//! mount, `named_objects::bring_back_range`; a git read, `read_through`; a home's read, `homes`). Before, the reader
//! dropped its pin and nothing let the window go again: sixteen texts read back stayed resident at 1,048,576 B against a
//! grant of 131,072 B (`grants::tests::what_a_read_brings_back_is_let_go_past_its_repositorys_grant`), the division's
//! let-go (`grants::let_go_past_grant`) having no caller outside tests. So what a read brings back is a cache, and the
//! division bounds it: each repository's read-backs past its grant go, the coldest first.
//!
//! **What it holds.** Each window a read brought back, by the repository whose reader brought it ([`noted`], at the one
//! step each bring-back already takes into the content store), with its bytes. A window let go, or gone, leaves it at
//! the next let-go here. One table a shard, on the shard's thread, as the read pins are (`named_objects::read_pinned`).
//!
//! **When it lets go.** At the division (`grants::current_at`, when it divides: the learning cadence, no timer of its
//! own): every repository's read-backs past its grant ([`let_go_past_grants`]). And at once where a read takes a
//! repository past its bound ([`bound`], derived: its grant plus the spare, the room no curve would use, which is all
//! a cadence's reads can borrow): what one cadence's reads may add before the next division is the spare, never more.
//! A repository the division gave nothing (no usage row among those it divided) has a grant of nothing: its read-backs
//! go at the next division.
//!
//! **What it never lets go.** A window a reader still pins (`named_objects::read_pinned`), however cold: it counts
//! against its repository's grant, and goes once the pin drops and a let-go here finds it past the grant. A window
//! another holder keeps, or no landed changeset holds, is passed over by the content store itself
//! (`ContentStore::let_go`).

use std::cell::RefCell;
use std::collections::BTreeMap;

use slates_db::catalog::RepositoryId;
use slates_vcs::ContentId;

use crate::state::ShardState;

/// Counter: windows a read brought back let go past their repository's grant (module docs). Format: a counter name.
pub(crate) const LET_GO: &str = "vcs_share.read_backs_let_go";
/// Counter: the bytes those windows freed. Format: a counter name.
pub(crate) const LET_GO_BYTES: &str = "vcs_share.read_backs_let_go_bytes";
/// Counter: a let-go run at once, a read having taken its repository past its bound ([`bound`]), before the next
/// division. Format: a counter name.
pub(crate) const PAST_BOUND: &str = "vcs_share.read_backs_past_bound";
/// Counter: a window let go would have been but a reader pins it: passed over. Format: a counter name.
pub(crate) const PINNED_KEPT: &str = "vcs_share.read_backs_pinned_kept";

thread_local! {
  /// This shard's read-backs (module docs): by repository, each window's bytes.
  static READ_BACKS: RefCell<BTreeMap<RepositoryId, BTreeMap<ContentId, u64>>> = const { RefCell::new(BTreeMap::new()) };
}

/// Notes that a reader of repository `reader` brought window `window` (`len` bytes) back into the content store: the
/// division asked first at its cadence (`grants::current_at`, which lets every repository's read-backs past its grant go
/// when it divides), then a let-go at once where this read takes the repository past its bound ([`bound`]).
pub(crate) fn noted(state: &mut ShardState, reader: RepositoryId, window: ContentId, len: u64) {
  // The division at its cadence, as admission asks it: a read alone drives it too, so a shard that only reads still
  // lets its read-backs go past their grants (no timer of its own).
  let _ = super::grants::current_at(state, slates_rt::futures::now_ns());
  let held = READ_BACKS.with(|backs| {
    let mut backs = backs.borrow_mut();
    let windows = backs.entry(reader).or_default();
    windows.insert(window, len);
    windows
      .values()
      .fold(0u64, |held, len| held.saturating_add(*len))
  });
  if held > bound(state, reader) {
    state.count(PAST_BOUND, 1);
    let_go_past(state, reader);
  }
}

/// Derived: the most bytes of read-backs repository `id` holds before a let-go runs at once (module docs): its published
/// grant, plus the spare the division lent nobody (§4.17.5 "Shares": the room no curve would use, lendable), which is
/// all one cadence's reads can borrow before the next division lets them go.
pub(crate) fn bound(state: &ShardState, id: RepositoryId) -> u64 {
  let grants = &state.vcs_grants;
  grants.of(id).unwrap_or(0).saturating_add(grants.spare)
}

/// Lets go every repository's read-backs past its published grant (module docs): what the division runs once it has
/// divided.
pub(crate) fn let_go_past_grants(state: &mut ShardState) {
  let readers: Vec<RepositoryId> =
    READ_BACKS.with(|backs| backs.borrow().keys().copied().collect());
  for reader in readers {
    let_go_past(state, reader);
  }
}

/// Lets go repository `id`'s coldest read-backs, by last read, until what it holds of them is within its grant (none
/// where it has none), passing over a window a reader pins; then forgets every window it holds no more. The bytes
/// freed.
fn let_go_past(state: &mut ShardState, id: RepositoryId) -> u64 {
  let grant = state.vcs_grants.of(id).unwrap_or(0);
  let Some(windows) = READ_BACKS.with(|backs| backs.borrow().get(&id).cloned()) else {
    return 0;
  };
  let Some(content) = state.vcs_stores.content() else {
    return 0;
  };
  // Resident, pinned or not: a pinned window counts against the grant though it cannot go.
  let mut resident: Vec<(u64, ContentId, u64, bool)> = windows
    .iter()
    .filter(|(window, _)| content.contains(window) && !content.is_away(window))
    .map(|(window, len)| {
      (
        content.last_read(window).unwrap_or(0),
        *window,
        *len,
        crate::vcs_store::named_objects::read_pinned(window),
      )
    })
    .collect();
  let held = resident
    .iter()
    .fold(0u64, |held, (_, _, len, _)| held.saturating_add(*len));
  let mut over = held.saturating_sub(grant);
  resident.sort_unstable_by_key(|(read, ..)| *read);
  let (freed, let_go, kept) = crate::vcs_store::within_budget(state, 0, |stores| {
    let (mut freed, mut let_go, mut kept) = (0u64, 0u64, 0u64);
    let Some(content) = stores.content_mut() else {
      return (freed, let_go, kept);
    };
    for (_, window, _, pinned) in &resident {
      if over == 0 {
        break;
      }
      if *pinned {
        kept = kept.saturating_add(1);
        continue;
      }
      if let Ok(bytes) = content.let_go(window) {
        freed = freed.saturating_add(bytes);
        let_go = let_go.saturating_add(1);
        over = over.saturating_sub(bytes);
      }
    }
    (freed, let_go, kept)
  });
  state.count(LET_GO, let_go);
  state.count(LET_GO_BYTES, freed);
  state.count(PINNED_KEPT, kept);
  forget_away(state, id);
  freed
}

/// Forgets repository `id`'s read-backs the content store no longer holds resident: let go, or gone.
fn forget_away(state: &ShardState, id: RepositoryId) {
  let content = state.vcs_stores.content();
  READ_BACKS.with(|backs| {
    let mut backs = backs.borrow_mut();
    if let Some(windows) = backs.get_mut(&id) {
      windows.retain(|window, _| {
        content.is_some_and(|content| content.contains(window) && !content.is_away(window))
      });
      if windows.is_empty() {
        backs.remove(&id);
      }
    }
  });
}
