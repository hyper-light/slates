//! A repository's store lent to git's upload-pack from its owner shard's map (A-682; A-625 part 3's [`StoreLender`] over
//! A-662's map; §4.17.2 "A fetch" and "Transports" items 1 and 2; §4.17.1 "Policy").
//!
//! **What is lent.** upload-pack (`slates_git::upload`, A-640) answers from a [`Served`]: A-614's store, the content it
//! is kept in, the refs a client may see and the branch `HEAD` names. [`ShardStores`] lends them from the shard's map
//! ([`super::VcsStores`]) afresh for each step of an answer, so a shard's state is never held across a wait (A-640 part
//! 2). The content store is lent the room the shard's budget admits for the step's reads and set back after it, as every
//! read of the map is (`super::lent_to_read`): upload-pack lists a tree, a tag or a commit within that room. A store the
//! map does not hold here (another shard's, one a recovery could not make again, another tool's) is not lent, and each
//! edge answers as it answers a service not served.
//!
//! **The refs, read by the asker's rights.** The step that answers a request ([`StoreLender::lend`]) reads the
//! repository's names from the partition's register as it runs. Each name binds a green, which is a git ref (§4.17.1's
//! terms: "each ref (`refs/heads/*`, `refs/tags/*` and every other ref)"), by git's push's rule (`crate::git_push`'s
//! `ref_of`, A-654), so both services see one ref a name: a short name `N` is `refs/heads/N`, a name under `refs/` is
//! itself, and a name beginning `refs/heads/` is no ref, since its ref is another name's, so names and refs map one to
//! one. A name that makes no ref git can carry (git's `check_refname_format`: a space, a `..`, a control byte) is no
//! ref either, since protocol v2's lines refuse one and a single such name would refuse every `ls-refs`. A ref is lent
//! only to a reader the repository's policy lets read it, hidden as git's push hides one from its pusher
//! (`crate::git_push`'s `PolicyHidden`: A-622's `Read`, git's `transfer.hideRefs` precedence, a ref no rule speaks to
//! the owner's alone), so a hidden ref is neither advertised nor wanted by name (`want-ref`, `deepen-not`). `HEAD` names
//! the repository's default branch by the same rule, and reads as unborn to a reader that may not read that branch,
//! since its target is looked up among the refs lent. The refs are lent in name order, as upload-pack reads them.
//!
//! **A pack's steps read no refs.** After the request's step, an answer's steps hand the store to the pack's source
//! alone, which names what it sends by position (`slates_git::upload::Answer::write`), so they are lent the objects
//! alone ([`StoreLender::lend_objects`]): a step costs its room and one packet whatever the repository's refs (A-640's
//! bound), never a walk of the register.
//!
//! **The pack** ([`ShardPacks`]) is A-639's stream from the store's fragments (`slates_git::upload::Fragments`, A-653),
//! each turn on the shard's clock (§4.3), within the step quantum the shard keeps now (`DaemonConfig::step_quantum_ns`),
//! at the owner's exchange ([`exchange`]).
//!
//! **Owed** (GAPS' §4.17 entry). The objects a hidden ref alone reaches stay fetchable by their ids: slates-git's
//! negotiation answers a want of any object the store holds (`negotiation::Refusal::NotOurs` names one it lacks), so
//! hiding them is slates-git's hidden refs, owed there. A repository with no default branch, or one whose default
//! branch is no ref, is not lent, since slates-git's answers always name `HEAD`'s branch (no verb records none today;
//! `Op::RepositoryDefaultSet` can). The exchange is §4.11's at neutral until the live memory-pressure and load signals
//! move it, as the codec policy's are.

use slates_db::catalog::{Principal, RepositoryId, RepositoryRecord};
use slates_git::Admission;
use slates_git::receive::HiddenRefs;
use slates_git::store::NamedRef;
use slates_git::store::learn::{Clock, Exchange};
use slates_git::upload::{Fragments, PackPlan, PackRefused, PackSource, Served, Streamed};
use slates_vcs::Work;

use crate::config::DaemonConfig;
use crate::git_push::{PolicyHidden, ref_of};
use crate::state::ShardState;
use crate::vcs_http::git::StoreLender;

// Its clone over protocol v2 serves the made commits' tests too (`super::commits`, A-664 part 3).
#[cfg(test)]
pub(crate) mod tests;

/// Format: nanoseconds in a second, the measured throughput's unit of time against the exchange's.
const NANOS_PER_SECOND: u64 = 1_000_000_000;

/// The lender of this shard's stores to upload-pack (the module doc): the HTTP edge's, and the SSH site's once the
/// daemon starts its edge. It holds nothing; each step borrows the shard's state for its own length.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct ShardStores;

impl StoreLender for ShardStores {
  type Packs = ShardPacks;

  fn holds(&self, id: RepositoryId) -> bool {
    crate::state::with_state_counted(|state| lendable(state, id).is_some()).unwrap_or(false)
  }

  /// Derived: the room repository `id`'s works are admitted on this shard (`crate::vcs_share::room`, A-724), as a
  /// push's.
  fn room(&mut self, id: RepositoryId) -> Option<u64> {
    crate::state::with_state_counted(|state| crate::vcs_share::room(state, id).bytes)
  }

  fn lend<R>(
    &mut self,
    (id, reader): (RepositoryId, &Principal),
    step: impl FnOnce(&Served<'_>, &mut ShardPacks) -> R,
  ) -> Option<R> {
    crate::state::with_state_counted(|state| lend_on(state, (id, Some(reader)), step)).flatten()
  }

  fn lend_objects<R>(
    &mut self,
    id: RepositoryId,
    step: impl FnOnce(&Served<'_>, &mut ShardPacks) -> R,
  ) -> Option<R> {
    crate::state::with_state_counted(|state| lend_on(state, (id, None), step)).flatten()
  }

  type Hold = GitGeneration;

  fn hold(&mut self, id: RepositoryId, reader: &Principal) -> Option<GitGeneration> {
    crate::state::with_state_counted(|state| hold_in(state, id, reader)).flatten()
  }

  fn lend_held<R>(
    &mut self,
    hold: &GitGeneration,
    step: impl FnOnce(&Served<'_>, &mut ShardPacks) -> R,
  ) -> Option<R> {
    crate::state::with_state_counted(|state| lend_held_in(state, hold, step)).flatten()
  }

  /// A-718: every version of repository `id` encoded as its fragments and kept, a turn at a time, before a fetch's answer
  /// is made, so the answer copies them rather than searching its objects for deltas ([`super::git_encode`]); waited for
  /// at most one liveness window, past which the answer begins and a task finishes the encoding behind it.
  async fn encode(&mut self, id: RepositoryId) {
    super::git_encode::encode_for_fetch(id).await;
  }
}

/// Runs `step` over repository `id`'s git store on `state`'s shard (the module doc): as upload-pack answers a request of
/// `reader`, with the refs `reader` may read and `HEAD`'s branch; or, with no reader, for an answer's next bytes, no ref
/// read. None when the store is not lendable here ([`lendable`]), or the repository's policy cannot be read for a
/// reader (A-622's guard keeps such rules out: a damaged record is never read without its policy).
pub(crate) fn lend_on<R>(
  state: &mut ShardState,
  (id, reader): (RepositoryId, Option<&Principal>),
  step: impl FnOnce(&Served<'_>, &mut ShardPacks) -> R,
) -> Option<R> {
  let began = slates_rt::futures::now_ns();
  let lent = lend_uncounted(state, (id, reader), step);
  // A-724 piece 1: a request's step (one with a reader) begins a fetch on the repository's usage row; every step that
  // was lent is counted with its time. Owed: a fetch's answered count, at the pack's last step.
  if lent.is_some() {
    let now = slates_rt::futures::now_ns();
    if let Some(reader) = reader {
      state.vcs_usage.begun(&id, reader, Work::Fetch, began);
    }
    state
      .vcs_usage
      .stepped(&id, Work::Fetch, (now.saturating_sub(began), 0), now);
  }
  lent
}

/// [`lend_on`]'s step, uncounted.
fn lend_uncounted<R>(
  state: &mut ShardState,
  (id, reader): (RepositoryId, Option<&Principal>),
  step: impl FnOnce(&Served<'_>, &mut ShardPacks) -> R,
) -> Option<R> {
  let (head, named) = match reader {
    Some(reader) => {
      let (record, head) = lendable(state, id)?;
      (head, readable_refs(state, record, reader)?)
    }
    None => (Vec::new(), Vec::new()),
  };
  let refs: Vec<NamedRef<'_>> = named
    .iter()
    .map(|(name, green)| NamedRef {
      name,
      green: *green,
    })
    .collect();
  let mut packs = ShardPacks::of(&state.config);
  super::lent_to_read((&mut state.store, &mut state.vcs_stores), |stores| {
    let (content, store) = stores.git_ref(id)?;
    let served = Served {
      repository: &store.repository,
      store: content,
      refs: &refs,
      head: &head,
    };
    Some(step(&served, &mut packs))
  })
}

/// A fork of a git repository's store as it stood at `stamp` (A-745, §4.17.1 "A fetch's answer" rule 4;
/// `slates_git::store::Repository::fork`), and how many fetches hold it.
#[derive(Debug)]
pub(crate) struct GitFork {
  /// Where the repository stood when it was forked ([`slates_git::store::Repository::stamp`]).
  stamp: (u64, u64),
  /// The fork.
  repository: slates_git::store::Repository,
  /// The fetches holding it.
  holders: u64,
}

/// A fetch's hold on the generation of a git repository it began on (A-745, §4.17.1 "A fetch's answer" rule 4): the fork
/// its shard keeps at the stamp the repository stood at ([`GitFork`], shared by every fetch that began there), and the
/// refs its reader could read and `HEAD`'s branch as they stood, so every step of its answer reads what its first step
/// read whatever lands meanwhile. Given back by its drop, on the shard that keeps the fork; the fork with its last
/// holder.
pub(crate) struct GitGeneration {
  /// The repository.
  id: RepositoryId,
  /// The stamp of the fork it holds.
  stamp: (u64, u64),
  /// The refs its reader could read, by full name and green, in name order.
  refs: Vec<(Vec<u8>, u64)>,
  /// The full name of the branch `HEAD` named.
  head: Vec<u8>,
}

impl Drop for GitGeneration {
  fn drop(&mut self) {
    let (id, stamp) = (self.id, self.stamp);
    crate::state::with_state(|state| release_fork(state, id, stamp)).unwrap_or_default();
  }
}

/// Holds repository `id`'s generation as `state` holds it now for `reader` ([`GitGeneration`]): the fork at the stamp it
/// stands at, made where none is kept; none when it is not lendable here or cannot be forked.
fn hold_in(state: &mut ShardState, id: RepositoryId, reader: &Principal) -> Option<GitGeneration> {
  let (record, head) = lendable(state, id)?;
  let refs = readable_refs(state, record, reader)?;
  let stamp = state.vcs_stores.git_ref(id)?.1.repository.stamp();
  let kept = state
    .vcs_stores
    .git_forks
    .get_mut(&id)
    .and_then(|forks| forks.iter_mut().find(|fork| fork.stamp == stamp));
  match kept {
    Some(fork) => fork.holders = fork.holders.saturating_add(1),
    None => {
      // The fork's records charge the shard's budget, as the repository's do, lent for the fork alone.
      let repository = super::within_budget(state, 0, |stores| {
        let (content, store) = stores.git(id)?;
        store.repository.fork(content).ok()
      })?;
      state
        .vcs_stores
        .git_forks
        .entry(id)
        .or_default()
        .push(GitFork {
          stamp,
          repository,
          holders: 1,
        });
    }
  }
  Some(GitGeneration {
    id,
    stamp,
    refs,
    head,
  })
}

/// Runs `step` over the generation `hold` holds, counted as [`lend_on`] counts a step; none when its fork is gone.
fn lend_held_in<R>(
  state: &mut ShardState,
  hold: &GitGeneration,
  step: impl FnOnce(&Served<'_>, &mut ShardPacks) -> R,
) -> Option<R> {
  let began = slates_rt::futures::now_ns();
  let refs: Vec<NamedRef<'_>> = hold
    .refs
    .iter()
    .map(|(name, green)| NamedRef {
      name,
      green: *green,
    })
    .collect();
  let mut packs = ShardPacks::of(&state.config);
  let lent = super::lent_to_read((&mut state.store, &mut state.vcs_stores), |stores| {
    let fork = stores
      .git_forks
      .get(&hold.id)?
      .iter()
      .find(|fork| fork.stamp == hold.stamp)?;
    let served = Served {
      repository: &fork.repository,
      store: stores.content.as_ref()?,
      refs: &refs,
      head: &hold.head,
    };
    Some(step(&served, &mut packs))
  });
  if lent.is_some() {
    let now = slates_rt::futures::now_ns();
    state
      .vcs_usage
      .stepped(&hold.id, Work::Fetch, (now.saturating_sub(began), 0), now);
  }
  lent
}

/// Lets go of one hold on repository `id`'s fork at `stamp`, giving the fork back with its last holder; a refused
/// release counted.
fn release_fork(state: &mut ShardState, id: RepositoryId, stamp: (u64, u64)) {
  let Some(forks) = state.vcs_stores.git_forks.get_mut(&id) else {
    return;
  };
  let Some(at) = forks.iter().position(|fork| fork.stamp == stamp) else {
    return;
  };
  let last = forks.get_mut(at).is_some_and(|fork| {
    fork.holders = fork.holders.saturating_sub(1);
    fork.holders == 0
  });
  if !last {
    return;
  }
  let fork = forks.remove(at);
  if forks.is_empty() {
    state.vcs_stores.git_forks.remove(&id);
  }
  let released = super::within_budget(state, 0, |stores| {
    stores
      .content
      .as_mut()
      .map_or(Ok(()), |content| fork.repository.release(content))
  });
  if released.is_err() {
    state.count(FORK_RELEASE_REFUSED, 1);
  }
}

/// Format: the count of a git fetch's fork its content store refused to take back (A-745 rule 4).
const FORK_RELEASE_REFUSED: &str = "vcs_store.git_fork_release";

/// Repository `id`'s record and the full name of the branch its `HEAD` names, when its git store is held on `state`'s
/// shard and can be lent (the module doc): none for a store not held here, one of another tool, or a repository with no
/// default branch or one whose default branch is no ref.
fn lendable(state: &ShardState, id: RepositoryId) -> Option<(&RepositoryRecord, Vec<u8>)> {
  state.vcs_stores.git_ref(id)?;
  let record = state.db.partition().repositories().repository(id)?;
  let head = ref_of(record.default_branch.as_deref()?)?;
  Some((record, head))
}

/// The refs of `record`'s repository that `reader` may read (the module doc): each ref's full name and its green's key
/// in the store, in name order, one ref a name; none when the repository's policy cannot be read.
fn readable_refs(
  state: &ShardState,
  record: &RepositoryRecord,
  reader: &Principal,
) -> Option<Vec<(Vec<u8>, u64)>> {
  let hidden = PolicyHidden::of(record, reader).ok()?;
  let mut refs: Vec<(Vec<u8>, u64)> = state
    .db
    .partition()
    .repositories()
    .branches(record.id, "")
    .filter_map(|(name, green)| Some((ref_of(name)?, super::green_key(green))))
    .filter(|(name, _)| !hidden.is_hidden(name))
    .collect();
  refs.sort_unstable_by(|left, right| left.0.cmp(&right.0));
  Some(refs)
}

/// Derived: the owner's exchange, what a byte the fragments' encoder saves is worth in its time (§4.11's value of a byte
/// over its value of CPU, at neutral): the time this machine takes to copy the byte, as the archive's codec policy prices
/// a byte at neutral (`DaemonConfig::codec`), so a second buys the measured memcpy throughput's bytes
/// (`DaemonConfig::memcpy_bytes_per_second`, at least one byte).
pub(super) fn exchange(config: &DaemonConfig) -> Exchange {
  Exchange {
    ns: NANOS_PER_SECOND,
    bytes: config.memcpy_bytes_per_second.max(1),
  }
}

/// A-639's stream as upload-pack's pack source on the owner shard (the module doc): each turn on the shard's clock, at
/// the owner's exchange, within the step quantum the shard keeps when the step was lent.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ShardPacks {
  /// What a saved byte is worth in time.
  exchange: Exchange,
  /// The nanoseconds a turn may spend.
  quantum_ns: u64,
}

impl ShardPacks {
  /// The source on a shard `config` configures, at its step quantum now.
  fn of(config: &DaemonConfig) -> ShardPacks {
    ShardPacks {
      exchange: exchange(config),
      quantum_ns: config.step_quantum_ns(),
    }
  }

  /// A-639's stream's source for one call, its turns on `clock`.
  fn fragments<'c>(&self, clock: &'c mut ShardClock) -> Fragments<'c> {
    Fragments {
      clock,
      exchange: self.exchange,
      quantum_ns: self.quantum_ns,
    }
  }
}

impl PackSource for ShardPacks {
  type Pack = Streamed;

  /// The shard's step quantum, begun now, on the shard's clock (§4.3; A-745): a fetch's choice of its pack's objects
  /// stops between units once it is spent.
  fn turn(&self) -> impl FnMut() -> bool + 'static {
    let began = slates_rt::futures::now_ns();
    let quantum = self.quantum_ns;
    move || slates_rt::futures::now_ns().saturating_sub(began) >= quantum
  }

  fn begin(
    &mut self,
    served: &Served<'_>,
    plan: &PackPlan<'_>,
    admission: &mut Admission,
  ) -> Result<Streamed, PackRefused> {
    self
      .fragments(&mut ShardClock)
      .begin(served, plan, admission)
  }

  fn write(
    &mut self,
    served: &Served<'_>,
    pack: &mut Streamed,
    admission: &mut Admission,
    out: &mut Vec<u8>,
    room: usize,
  ) -> Result<bool, PackRefused> {
    self
      .fragments(&mut ShardClock)
      .write(served, pack, admission, out, room)
  }
}

/// The shard's clock (§4.3), which a turn's quantum is spent on.
pub(super) struct ShardClock;

impl Clock for ShardClock {
  fn now_ns(&mut self) -> u64 {
    slates_rt::futures::now_ns()
  }
}
