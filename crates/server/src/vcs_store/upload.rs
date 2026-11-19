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
//! terms: "each ref (`refs/heads/*`, `refs/tags/*` and every other ref)"): a name under `refs/` as it is, any other a
//! branch under `refs/heads/`, the rule `crate::lfs` names the default branch by. A name that makes no ref git can
//! carry (`slates_git::refname::is_valid`, git's `check_refname_format`: a space, a `..`, a control byte) is not lent,
//! since protocol v2's lines refuse one and a single such name would refuse every `ls-refs`. A ref is lent only to a
//! reader the repository's policy lets read it (`crate::vcs_policy::permits`: git's `transfer.hideRefs` precedence, the
//! last rule that covers the ref deciding, and a ref no rule speaks to the owner's alone), so a hidden ref is neither
//! advertised nor wanted by name (`want-ref`, `deepen-not`). `HEAD` names the repository's default branch by the same
//! rule, and reads as unborn to a reader that may not read that branch, since its target is looked up among the refs
//! lent. The refs are sorted by their full names; two names that make one ref (`main` and `refs/heads/main`) meet
//! upload-pack's own order check, which refuses the request rather than serve either.
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
//! branch makes no ref git can carry, is not lent, since slates-git's answers always name `HEAD`'s branch (no verb
//! records none today; `Op::RepositoryDefaultSet` can). The exchange is §4.11's at neutral until the live
//! memory-pressure and load signals move it, as the codec policy's are.

use slates_db::catalog::{Principal, RepositoryId, RepositoryRecord};
use slates_git::Admission;
use slates_git::refname;
use slates_git::store::NamedRef;
use slates_git::store::learn::{Clock, Exchange};
use slates_git::upload::{Fragments, PackPlan, PackRefused, PackSource, Served, Streamed};

use crate::config::DaemonConfig;
use crate::state::ShardState;
use crate::vcs_http::git::StoreLender;
use crate::vcs_policy::{Compiled, Name, Right};

#[cfg(test)]
mod tests;

/// Format: what every git ref's full name begins with (`git check-ref-format`).
const REFS: &str = "refs/";

/// Format: the namespace a branch's short name lives under (`git check-ref-format`), as `crate::lfs` names the default
/// branch.
const HEADS: &str = "refs/heads/";

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
  let (head, named) = match reader {
    Some(reader) => {
      let (record, head) = lendable(state, id)?;
      (head, readable_refs(state, record, reader)?)
    }
    None => (String::new(), Vec::new()),
  };
  let refs: Vec<NamedRef<'_>> = named
    .iter()
    .map(|(name, green)| NamedRef {
      name: name.as_bytes(),
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
      head: head.as_bytes(),
    };
    Some(step(&served, &mut packs))
  })
}

/// Repository `id`'s record and the full name of the branch its `HEAD` names, when its git store is held on `state`'s
/// shard and can be lent (the module doc): none for a store not held here, one of another tool, or a repository with no
/// default branch or one that makes no ref git can carry.
fn lendable(state: &ShardState, id: RepositoryId) -> Option<(&RepositoryRecord, String)> {
  state.vcs_stores.git_ref(id)?;
  let record = state.db.partition().repositories().repository(id)?;
  let head = git_ref_name(record.default_branch.as_deref()?);
  refname::is_valid(head.as_bytes()).then_some((record, head))
}

/// The refs of `record`'s repository that `reader` may read (the module doc): each ref's full name and its green's key
/// in the store, in name order, every name a ref git can carry; none when the repository's policy cannot be read.
fn readable_refs(
  state: &ShardState,
  record: &RepositoryRecord,
  reader: &Principal,
) -> Option<Vec<(String, u64)>> {
  let compiled = Compiled::of(record).ok()?;
  let mut refs: Vec<(String, u64)> = state
    .db
    .partition()
    .repositories()
    .branches(record.id, "")
    .map(|(name, green)| (git_ref_name(name), super::green_key(green)))
    .filter(|(name, _)| refname::is_valid(name.as_bytes()))
    .filter(|(name, _)| {
      crate::vcs_policy::permits(
        &compiled,
        (record, reader),
        (Name::Ref(name.as_bytes()), Right::Read),
      )
    })
    .collect();
  refs.sort_by(|left, right| left.0.cmp(&right.0));
  Some(refs)
}

/// The git ref a name of the repository's register binds (the module doc): a name under `refs/` as it is, any other a
/// branch's, under `refs/heads/`.
fn git_ref_name(name: &str) -> String {
  if name.starts_with(REFS) {
    name.to_owned()
  } else {
    format!("{HEADS}{name}")
  }
}

/// Derived: the owner's exchange, what a byte the fragments' encoder saves is worth in its time (§4.11's value of a byte
/// over its value of CPU, at neutral): the time this machine takes to copy the byte, as the archive's codec policy prices
/// a byte at neutral (`DaemonConfig::codec`), so a second buys the measured memcpy throughput's bytes
/// (`DaemonConfig::memcpy_bytes_per_second`, at least one byte).
fn exchange(config: &DaemonConfig) -> Exchange {
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
    out: &mut Vec<u8>,
    room: usize,
  ) -> Result<bool, PackRefused> {
    self
      .fragments(&mut ShardClock)
      .write(served, pack, out, room)
  }
}

/// The shard's clock (§4.3), which a turn's quantum is spent on.
struct ShardClock;

impl Clock for ShardClock {
  fn now_ns(&mut self) -> u64 {
    slates_rt::futures::now_ns()
  }
}
