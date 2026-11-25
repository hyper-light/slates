//! What reads brought back from the node's store, by the repository whose store holds each window's object, let go
//! past each repository's grant (A-724 piece 4's reader; §4.17.5 "The cold trigger"; main 01a20182's rulings (b) and
//! (i') of 2026-11-25).
//!
//! **Why.** A let-go window a read finds away is brought back from the node's store and served (a named read through a
//! mount, `named_objects::bring_back_range`; a git read, `read_through`; a home's read, `homes`). Before, the reader
//! dropped its pin and nothing let the window go again: sixteen texts read back stayed resident at 1,048,576 B against a
//! grant of 196,608 B (`grants::tests::what_a_read_brings_back_is_let_go_past_its_repositorys_grant`), the division's
//! let-go (`grants::let_go_past_grant`) having no caller outside tests. So what a read brings back is a cache, and the
//! division bounds it: each repository's read-backs past its grant go, the coldest first.
//!
//! **Whose a window is** ([`brought_back`], at the one step each bring-back takes into the content store). The
//! repository whose store holds the window's object, the truth the read itself was served by, never a map of its own:
//! a Subversion text's, the repository its caller read it for (`VcsStores::subversion_storing`'s); a git pack's, the
//! repository whose git store holds that pack ([`git_holding_pack`]: a scan of this shard's git stores' packs, so
//! `O(repositories × packs)` on the shard, run only on a bring-back, which has already waited on the node's store); a
//! home's window, its landed note's (a Mercurial revision's repository, or a pack's as above). A window no store here
//! claims is no repository's cache: counted, and let go at the next division it is old enough for, no reader pinning it
//! ([`let_go_unattributed`]).
//!
//! **What it holds.** Each attributed window, by repository, with its bytes and the division it was noted under; each
//! unattributed window, with its division, until it goes. A window let go, or gone, leaves it at the next let-go here.
//! One table a shard, on the shard's thread, as the read pins are (`named_objects::read_pinned`).
//!
//! **Never under a read.** Only a named read pins what it brought back; a git or a home's read brings a window back and
//! reads it after, a step later. So a let-go here takes only a window noted before the division before this one: every
//! read-back stays at least a whole cadence, and no let-go lands between a bring-back and its read.
//!
//! **When it lets go.** At the division (`grants::current_at` when it divides: the learning cadence, no timer of its
//! own; a read asks it too, [`brought_back`], so a shard that only reads divides at its cadence): every repository's
//! read-backs past its grant ([`let_go_past_grants`]), and the unattributed ones no reader pins. And at once where a
//! read takes a repository past its bound ([`bound`], derived: its grant plus the spare, the room no curve would use,
//! which is all a cadence's reads can borrow): what one cadence's reads may add before the next division is the spare,
//! never more. A repository the division gave nothing (no usage row among those it divided) has a grant of nothing: its
//! read-backs go at the next division.
//!
//! **What it never lets go.** A window a reader still pins (`named_objects::read_pinned`), however cold: it counts
//! against its repository's grant, and goes once the pin drops and a let-go here finds it past the grant. A window a
//! home holds here stays, as every landed let-go leaves it (`vcs_store::let_go_read`).

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
/// Counter: a window brought back for a Subversion text, attributed to the repository it was read for. Format: a
/// counter name.
pub(crate) const ATTRIBUTED_TEXT: &str = "vcs_share.read_backs_attributed_text";
/// Counter: a window brought back for a git pack, attributed to the repository whose git store holds the pack. Format:
/// a counter name.
pub(crate) const ATTRIBUTED_PACK: &str = "vcs_share.read_backs_attributed_pack";
/// Counter: a window brought back for a Mercurial revision, attributed to its landed note's repository. Format: a
/// counter name.
pub(crate) const ATTRIBUTED_HG: &str = "vcs_share.read_backs_attributed_hg";
/// Counter: a window brought back whose object no store on this shard claims: no repository's cache, let go once no
/// reader pins it. Format: a counter name.
pub(crate) const UNATTRIBUTED: &str = "vcs_share.read_backs_unattributed";
/// Counter: an unattributed window let go, its pins dropped. Format: a counter name.
pub(crate) const UNATTRIBUTED_LET_GO: &str = "vcs_share.read_backs_unattributed_let_go";

thread_local! {
  /// This shard's attributed read-backs (module docs): by repository, each window's bytes and the division it was noted
  /// under.
  static READ_BACKS: RefCell<BTreeMap<RepositoryId, BTreeMap<ContentId, (u64, u64)>>> = const { RefCell::new(BTreeMap::new()) };
  /// This shard's unattributed read-backs, each with the division it was noted under.
  static UNATTRIBUTED_BACKS: RefCell<BTreeMap<ContentId, u64>> = const { RefCell::new(BTreeMap::new()) };
}

/// What holds a brought-back window's object, as its bring-back names it ([`brought_back`]).
#[derive(Clone, Copy, Debug)]
pub(crate) enum Holder {
  /// A Subversion text read for this repository (`VcsStores::subversion_storing`'s).
  Text(RepositoryId),
  /// The git pack it lies in.
  Pack(ContentId),
  /// A home's window: its landed note names its revision or its pack (`homes::landed`).
  Landed,
}

/// Window `window` (`len` bytes) brought back into the content store for an object `holder` names (module docs): the
/// division asked first at its cadence (`grants::current_at`), then the window attributed to the repository whose store
/// holds its object and noted against its grant (a let-go at once where that takes it past its bound, [`bound`]), or,
/// claimed by none, kept for the division ([`let_go_unattributed`]).
pub(crate) fn brought_back(state: &mut ShardState, window: ContentId, len: u64, holder: Holder) {
  // The division at its cadence, as admission asks it: a read alone drives it too, so a shard that only reads still
  // lets its read-backs go past their grants (no timer of its own).
  let _ = super::grants::current_at(state, slates_rt::futures::now_ns());
  match attributed(state, &window, holder) {
    Some((reader, counter)) => {
      state.count(counter, 1);
      noted(state, reader, window, len);
    }
    None => {
      state.count(UNATTRIBUTED, 1);
      let generation = state.vcs_grants.generation;
      UNATTRIBUTED_BACKS.with(|backs| backs.borrow_mut().insert(window, generation));
    }
  }
}

/// Whether a read-back noted under division `noted` may go now, the grants at `generation`: noted before the division
/// before this one (module docs, "Never under a read"), so a whole cadence has passed since its read.
fn old_enough(noted: u64, generation: u64) -> bool {
  noted.saturating_add(1) < generation
}

/// The repository whose store holds `window`'s object, as `holder` names it (module docs), and the counter of its kind.
fn attributed(
  state: &ShardState,
  window: &ContentId,
  holder: Holder,
) -> Option<(RepositoryId, &'static str)> {
  match holder {
    Holder::Text(reader) => Some((reader, ATTRIBUTED_TEXT)),
    Holder::Pack(pack) => git_holding_pack(state, &pack).map(|reader| (reader, ATTRIBUTED_PACK)),
    Holder::Landed => match crate::vcs_store::homes::landed(window)? {
      crate::vcs_store::homes::Landed::Hg(hg) => Some((hg.repository, ATTRIBUTED_HG)),
      crate::vcs_store::homes::Landed::Pack(at) => {
        git_holding_pack(state, &at.pack).map(|reader| (reader, ATTRIBUTED_PACK))
      }
    },
  }
}

/// The repository whose git store on this shard holds pack `pack`, where one does: a scan of this shard's git stores'
/// packs, `O(repositories × packs)` (module docs), run only on a bring-back, which has already waited on the node's
/// store. A pack-to-repository index is a later piece, should the scan ever show beside that wait.
pub(crate) fn git_holding_pack(state: &ShardState, pack: &ContentId) -> Option<RepositoryId> {
  state.vcs_stores.git_holding_pack(pack)
}

/// Notes `window` (`len` bytes) against repository `reader`'s grant, letting go at once where that takes it past its
/// bound ([`bound`]).
fn noted(state: &mut ShardState, reader: RepositoryId, window: ContentId, len: u64) {
  let generation = state.vcs_grants.generation;
  let held = READ_BACKS.with(|backs| {
    let mut backs = backs.borrow_mut();
    let windows = backs.entry(reader).or_default();
    windows.insert(window, (len, generation));
    windows
      .values()
      .fold(0u64, |held, (len, _)| held.saturating_add(*len))
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

/// Lets go every repository's read-backs past its published grant, and every unattributed one no reader pins (module
/// docs): what the division runs once it has divided.
pub(crate) fn let_go_past_grants(state: &mut ShardState) {
  let readers: Vec<RepositoryId> =
    READ_BACKS.with(|backs| backs.borrow().keys().copied().collect());
  for reader in readers {
    let_go_past(state, reader);
  }
  let_go_unattributed(state);
}

/// Lets go every unattributed read-back old enough ([`old_enough`]) that no reader pins (`vcs_store::let_go_read`),
/// forgetting each one let go or refused.
fn let_go_unattributed(state: &mut ShardState) {
  let generation = state.vcs_grants.generation;
  let windows: Vec<ContentId> = UNATTRIBUTED_BACKS.with(|backs| {
    backs
      .borrow()
      .iter()
      .filter(|(_, noted)| old_enough(**noted, generation))
      .map(|(window, _)| *window)
      .collect()
  });
  for window in windows {
    if crate::vcs_store::named_objects::read_pinned(&window) {
      continue;
    }
    if crate::vcs_store::let_go_read(state, &window).is_ok() {
      state.count(UNATTRIBUTED_LET_GO, 1);
    }
    UNATTRIBUTED_BACKS.with(|backs| backs.borrow_mut().remove(&window));
  }
}

/// Lets go repository `id`'s coldest read-backs, by last read, until what it holds of them is within its grant (none
/// where it has none), passing over a window a reader pins or one too young ([`old_enough`]; `vcs_store::let_go_read`'s
/// rules for the rest); then forgets every window it holds no more. The bytes freed.
fn let_go_past(state: &mut ShardState, id: RepositoryId) -> u64 {
  let grant = state.vcs_grants.of(id).unwrap_or(0);
  let Some(windows) = READ_BACKS.with(|backs| backs.borrow().get(&id).cloned()) else {
    return 0;
  };
  let Some(content) = state.vcs_stores.content() else {
    return 0;
  };
  let generation = state.vcs_grants.generation;
  // Resident, pinned or young or not: such a window counts against the grant though it cannot go yet.
  let mut resident: Vec<(u64, ContentId, u64, bool)> = windows
    .iter()
    .filter(|(window, _)| content.contains(window) && !content.is_away(window))
    .map(|(window, (len, noted))| {
      (
        content.last_read(window).unwrap_or(0),
        *window,
        *len,
        old_enough(*noted, generation),
      )
    })
    .collect();
  let held = resident
    .iter()
    .fold(0u64, |held, (_, _, len, _)| held.saturating_add(*len));
  let mut over = held.saturating_sub(grant);
  resident.sort_unstable_by_key(|(read, ..)| *read);
  let (mut freed, mut let_go, mut kept) = (0u64, 0u64, 0u64);
  for (_, window, _, old) in &resident {
    if over == 0 {
      break;
    }
    if !old {
      continue;
    }
    if crate::vcs_store::named_objects::read_pinned(window) {
      kept = kept.saturating_add(1);
      continue;
    }
    if let Ok(bytes) = crate::vcs_store::let_go_read(state, window) {
      freed = freed.saturating_add(bytes);
      let_go = let_go.saturating_add(1);
      over = over.saturating_sub(bytes);
    }
  }
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

#[cfg(test)]
mod tests {
  use slates_ipc::protocol::VcsTool;
  use slates_vcs::ContentId;

  use super::{Holder, UNATTRIBUTED, UNATTRIBUTED_LET_GO, brought_back, git_holding_pack};
  use crate::state::ShardState;

  /// Shape: the git repositories the attribution test holds on one shard, each with one push's pack.
  const REPOSITORIES: usize = 24;

  /// Counter `name` on the shard.
  fn count(state: &ShardState, name: &str) -> u64 {
    state.refusals.get(name).copied().unwrap_or(0)
  }

  /// Main 01a20182's ruling (i') of 2026-11-25: a window brought back whose object no store on this shard claims is no
  /// repository's cache, and goes once no reader holds it. Do: put a landed window of 1,000 bytes, let it go, bring it
  /// back, and note it as brought back for a pack no git store here holds; then let go at a division two on. Expect:
  /// counted unattributed and kept while its read may still run, then let go at the division (no reader pins it),
  /// counted. Failing first: kept resident, nothing letting it go.
  #[test]
  fn a_read_back_no_store_claims_is_let_go_once_unpinned() {
    crate::test_store::audit_on_stored_shard("read-backs-unattributed", |state| {
      let _ = crate::vcs_store::tests::repository(state, "unclaimed", VcsTool::Git);
      let bytes = [7u8; 1_000];
      let window = crate::vcs_store::within_budget(state, 1 << 16, |stores| {
        let content = stores.content_mut().expect("a content store");
        let window = content.put(&bytes).expect("put");
        assert!(content.mark_landed(&window));
        content.let_go(&window).expect("let go");
        content.bring_back(&window, &bytes).expect("brought back");
        window
      });
      brought_back(state, window, 1_000, Holder::Pack(ContentId([9; 32])));
      let kept = state
        .vcs_stores
        .content()
        .is_some_and(|content| !content.is_away(&window));
      // Two divisions on: a whole cadence old, the read that brought it back long done.
      state.vcs_grants.generation = state.vcs_grants.generation.saturating_add(2);
      super::let_go_past_grants(state);
      let away = state
        .vcs_stores
        .content()
        .is_some_and(|content| content.is_away(&window));
      assert_eq!(
        (
          count(state, UNATTRIBUTED),
          kept,
          count(state, UNATTRIBUTED_LET_GO),
          away
        ),
        (1, true, 1, true)
      );
    });
  }

  /// Main 01a20182's ruling (i'), its cost stated and measured: a git window is attributed to the repository whose git
  /// store holds its pack, by a scan of the shard's git stores' packs. Do: hold [`REPOSITORIES`] git repositories on one
  /// shard, each pushed once (a pack each); find each one's pack's holder, timing the scans. Expect: every pack found
  /// in its own repository, and a pack none holds in none. Prints the scan's nanoseconds a lookup with how many
  /// repositories and packs it walked (`O(repositories × packs)`, run only on a bring-back).
  #[test]
  fn a_packs_window_is_attributed_to_the_repository_whose_store_holds_it() {
    use crate::vcs_store::tests::{commit_of, push_through, repository, tree_of};
    crate::test_store::audit_on_stored_shard("read-backs-packs", |state| {
      let mut held = Vec::new();
      for number in 0..REPOSITORIES {
        let (id, root) = repository(state, &format!("packed-{number}"), VcsTool::Git);
        let files = [(
          b"README".to_vec(),
          format!("repository {number}\n").into_bytes(),
        )];
        let (objects, commit) = commit_of(&files, &[], 1_700_000_000);
        let tree = tree_of(&objects);
        push_through(state, (id, root), &objects, (None, commit, tree));
        let packs: Vec<ContentId> = state
          .vcs_stores
          .git_ref(id)
          .map(|(_, store)| store.repository.packs().map(|(pack, _)| pack).collect())
          .unwrap_or_default();
        assert!(!packs.is_empty(), "repository {number} holds a pack");
        held.extend(packs.into_iter().map(|pack| (id, pack)));
      }
      let began = std::time::Instant::now();
      for (id, pack) in &held {
        assert_eq!(git_holding_pack(state, pack), Some(*id));
      }
      let took = began.elapsed();
      assert_eq!(git_holding_pack(state, &ContentId([9; 32])), None);
      eprintln!(
        "read_backs: {} lookups over {REPOSITORIES} git repositories and {} packs in {took:?}, {} ns a lookup",
        held.len(),
        held.len(),
        took.as_nanos() / u128::try_from(held.len().max(1)).unwrap_or(1)
      );
    });
  }
}
