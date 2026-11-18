//! A green's takeover successor adopts its holder ledger (§4.16 owner-loss recovery, "Masked after takeover on a
//! candidate that holds the ledger"; A-333).
//!
//! Each holder of a green keeps the green's ledger: every merge record it accepted, in its acceptor of the green's
//! register (durable across a warm restart, §4.8 "Recovery"), and the inputs those records name, in its content hold
//! (AUD-29-59), retained while a record names them. It also keeps a replica of the green, the engine each version is
//! recomputed into before the holder accepts the version's record. When the owner dies, the successor the takeover round
//! names is one of those holders.
//!
//! **What was there.** The successor rebuilt the green beside its ledger, in one poll of each shard: on the control
//! shard version 0's inputs were restored whole from the hold into one dense copy (1.67 GB for the Linux kernel), every
//! increment's too, and every accepted position was cloned and decoded; on the owner shard the origin's bytes were
//! recorded (`GreenOriginated`), every increment was recorded again (`GreenAdvanced`), and the green was replayed twice.
//! The partition's merge budget is a quarter of a shard's metadata reserve (about 333 MB a shard on a 64 GB machine of
//! 16 shards), so a kernel-sized origin was refused at its first record, and the takeover was given up at the failover
//! bound: losing the primary lost the green (GAPS, "The many-client load test from 10,000 to 100,000 clients").
//!
//! **Now.**
//! - The partition records the green by its adoption (`Op::GreenAdopted`: the catalog record, the adopted version and
//!   identity, version 0's inputs manifest), a few dozen bytes whatever the tree; its chain holds only the versions after
//!   the adoption.
//! - The green's engine is the successor's own replica, already at or near the adopted version: taken as it is, advanced
//!   from its head by the increments this node holds (`merge_origin::advance_replica`), or rebuilt from the ledger when
//!   there is none (`merge_origin::rebuild_replica_through`), a slice at a time ([`engine_for_adoption`]), then moved by
//!   value to the green's owner shard (`verbs::adopt_taken_over_green`). Nothing is copied, recorded again or replayed.
//! - The submit completions the ledger's records carried (A-255) are gathered a slice of records at a time by a task of
//!   the control shard ([`gathered_completions`]), so the successor answers a submit's resend from its own partition.
//! - A restarted successor rebuilds the engine from the same ledger at boot, after every partition has recovered and
//!   before any client is admitted ([`readopt_at_boot`]): through the adopted version, or version 0 alone when a
//!   checkpoint past the adoption stands, and the owner shard then applies its own chain after it a slice a call.
//!
//! The ledger stays: a green's holder records are never compacted, and the inputs they name are retained while named, so
//! the adopted versions can be read, rebased onto and rebuilt from for as long as the green lives here.
//!
//! At `f = 0` there is no takeover and nothing here runs (R8).

use std::collections::BTreeMap;

use slates_db::catalog::VolumeId as DbVolumeId;
use slates_db::register::ObjectId;
use slates_ipc::protocol::Refusal;
use slates_merge::engine::Green;
use slates_rt::shard::ShardId;

use crate::merge_origin::Rebuild;
use crate::merge_service::{MergeRecordValue, SubmitCompletion};
use crate::state::ShardState;

/// Counted on a takeover successor's control shard for each slice of reading its ledger's records for the submit
/// completions they carried (A-255, A-333): more than one shows a long ledger was read in slices.
/// Format: a counter name in the daemon's status report.
const COMPLETIONS_SLICES: &str = "merge.takeover.completion_slices";
/// Counted on a takeover successor whose task to gather a green's completions could not be spawned (its task arena
/// full): the next attempt starts it again.
/// Format: a counter name in the daemon's status report.
const COMPLETIONS_UNSPAWNED: &str = "merge.takeover.completions_unspawned";
/// Counted on a takeover successor whose replica of a green was past the version its takeover adopted: the replica is
/// dropped and rebuilt through the adopted version. The round adopts this node's newest record or a newer one, so zero
/// is the only reading the design allows.
/// Format: a counter name in the daemon's status report.
const REPLICA_PAST_ADOPTION: &str = "merge.takeover.replica_past_adoption";
/// Counted on a takeover successor whose replica, at the version its takeover adopted, does not have the adopted
/// record's identity: the green is refused on this node, loudly (§4.16 "mismatch fatal-and-loud").
/// Format: a counter name in the daemon's status report.
const ADOPTION_MISMATCH: &str = "merge.takeover.adoption_mismatch";
/// Counted at a restarted successor's boot for each green it re-adopted from its ledger (A-333): the non-vacuity counter
/// of the re-adoption.
/// Format: a counter name in the daemon's status report.
pub(crate) const READOPTED: &str = "merge.takeover.readopted";
/// Counted at a restarted successor's boot for each green it could not re-adopt from its ledger (records short, inputs
/// not held, a rebuild off the adopted identity, or the owner shard not answering within its attempts): the green is
/// fenced, every verb on it refused typed. Zero is the healthy reading.
/// Format: a counter name in the daemon's status report.
pub(crate) const READOPTION_FENCED: &str = "merge.takeover.readoption_fenced";

/// The submit completions a green's ledger carried, gathered for its adoption (A-255, A-333;
/// `MergeShardState::adoption_completions`).
pub(crate) enum Completions {
  /// A task of this shard is reading the ledger's records.
  Gathering,
  /// Read through `through`: each client's newest completions, at most its completion bound, with the version each was
  /// answered.
  Gathered {
    /// The last version read.
    through: u64,
    /// The completions kept, each with its version.
    completions: Vec<(u64, SubmitCompletion)>,
  },
}

/// What a takeover successor's control shard hands the green's owner shard for its adoption (A-333): the catalog facts
/// its adopted records name, the adopted version, its identity and version 0's inputs manifest, the engine at that
/// version and the completions the ledger's records carried.
pub(crate) struct AdoptedGreen {
  /// The green's mount name.
  pub(crate) name: String,
  /// Whether every submit must carry evidence.
  pub(crate) require_evidence: bool,
  /// The owning principal.
  pub(crate) owner: slates_db::catalog::Principal,
  /// The access list of the green's adopted catalog register.
  pub(crate) access: Vec<slates_db::catalog::AccessEntry>,
  /// The adopted catalog register's sequence, which the green's record continues from.
  pub(crate) catalog_sequence: u64,
  /// The green's write policy, from its adopted catalog register (A-285, A-321).
  pub(crate) write: Option<slates_db::catalog::WritePolicy>,
  /// The policy's version, from the same register.
  pub(crate) write_version: u64,
  /// The green's trusted signers, from the same register (A-321).
  pub(crate) signers: Vec<Vec<u8>>,
  /// The adopted version.
  pub(crate) head: u64,
  /// The adopted record's identity: the green's head identity at `head`.
  pub(crate) identity: [u8; 32],
  /// Version 0's inputs manifest, as the ledger's version 0 record names it; `None` for a green with no origin.
  pub(crate) origin: Option<[u8; 32]>,
  /// The promotion epoch the takeover re-committed the green's registers at, which the holders fenced the green at:
  /// its records are written at it (`merge_service::record_epoch`), and the adoption keeps it in the green's record
  /// (`VolumeRecord::taken_over`), so a restart of this node, which forgets `placed_heads`, writes them there still.
  pub(crate) epoch: u64,
  /// The engine at `head`: this node's replica.
  pub(crate) engine: Green,
  /// The submit completions the ledger's records carried, each with its version.
  pub(crate) completions: Vec<(u64, SubmitCompletion)>,
}

/// A green recovered on its owner shard that its takeover adopted from this node's holder ledger (A-333;
/// `MergeShardState::awaiting_adoption`): its partition records versions up to the adoption by reference, so no replay
/// of the partition rebuilds it. The boot rebuilds its engine from the ledger ([`readopt_at_boot`]).
pub(crate) struct AwaitingAdoption {
  /// The version the engine is rebuilt through on the control shard: the adoption's, or 0 when a checkpoint past it
  /// stands (the checkpoint applies onto version 0).
  pub(crate) through: u64,
  /// The identity the engine must have at `through`: the adoption's; `None` for version 0, whose identity the control
  /// shard checks against its own record, the checkpoint's then checked as the chain is applied.
  pub(crate) identity: Option<[u8; 32]>,
  /// The owner shard's replay of its own chain onto the handed engine, once it was handed.
  pub(crate) replay: Option<Box<crate::verbs::GreenReplay>>,
}

/// What a takeover successor has of a green's engine at the version it adopted (A-333, [`engine_for_adoption`]).
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Engine {
  /// This node's replica is at the adopted version with its identity: the green's engine, still in the replicas.
  Ready,
  /// A task of this shard is building it.
  Building,
  /// A build began now (an advance or a rebuild), its first slice run.
  Started,
  /// The replica is behind, and these versions' inputs, named by records this node accepted, are not held here: each
  /// version and manifest.
  Unheld(Vec<(u64, [u8; 32])>),
  /// The ledger here lacks the record of this version: no engine can be built from it until the records from it up to
  /// the adopted version are adopted position by position (`fleet::adopt_missing_records`, A-364, GAP-A9-7's record
  /// half).
  RecordsShort(u64),
  /// The green is refused on this node: its replica's identity at the adopted version is another, or a recomputation
  /// mismatched before (fatal-and-loud, counted).
  Refused,
}

/// Brings this node's replica of `object` to the version its takeover adopted, `through`, with the adopted record's
/// `identity` (A-333): ready when it is there, else the build begun or still running. A replica behind is advanced from
/// its head, once the few versions it lacks are known to be held here; one missing is rebuilt from the ledger through
/// the adopted version, version 0 seeded from its inputs and each version after it recomputed from its increment. A
/// replica whose rebuild ended without one (counted why) is begun again at the next ask.
pub(crate) fn engine_for_adoption(
  state: &mut ShardState,
  object: ObjectId,
  (through, identity): (u64, [u8; 32]),
) -> Engine {
  if state.merge.refused.contains(&object) {
    return Engine::Refused;
  }
  if state.merge.seeding.contains(&object) {
    return Engine::Building;
  }
  match state.merge.replicas.get(&object).map(Green::head) {
    Some(head) if head == through => at_adopted(state, object, identity),
    Some(head) if head < through => advance(state, object, (head, through), identity),
    Some(_) => {
      state.merge.replicas.remove(&object);
      state.count(REPLICA_PAST_ADOPTION, 1);
      rebuild(state, object, through, identity)
    }
    None => rebuild(state, object, through, identity),
  }
}

/// Advances the replica at `head` to `through`, once the versions it lacks are each in this node's ledger with their
/// inputs held.
fn advance(
  state: &mut ShardState,
  object: ObjectId,
  (head, through): (u64, u64),
  identity: [u8; 32],
) -> Engine {
  let from = head.saturating_add(1);
  match crate::merge_origin::ledger_unheld(state, object, (from, through)) {
    Err(version) => Engine::RecordsShort(version),
    Ok(unheld) if !unheld.is_empty() => Engine::Unheld(unheld),
    Ok(_) => match crate::merge_origin::advance_replica(state, object, through) {
      Rebuild::Installed(at) if at == through => at_adopted(state, object, identity),
      Rebuild::Installed(_) | Rebuild::Later | Rebuild::Nothing => Engine::Started,
    },
  }
}

/// Rebuilds the replica from this node's ledger through `through`. The adopted version's inputs are checked first: a
/// successor one version behind the quorum holds the adopted record (its adoption committed it here) but not always its
/// inputs, which a fetch can bring (`fleet::fetch_green_inputs`); every earlier version this node accepted holding its
/// inputs (A-244), retained while its record names them.
fn rebuild(state: &mut ShardState, object: ObjectId, through: u64, identity: [u8; 32]) -> Engine {
  match crate::merge_origin::ledger_unheld(state, object, (through, through)) {
    Err(version) => return Engine::RecordsShort(version),
    Ok(unheld) if !unheld.is_empty() => return Engine::Unheld(unheld),
    Ok(_) => {}
  }
  match crate::merge_origin::rebuild_replica_through(state, object, through) {
    Rebuild::Installed(at) if at == through => at_adopted(state, object, identity),
    Rebuild::Nothing => Engine::RecordsShort(0),
    Rebuild::Installed(_) | Rebuild::Later => Engine::Started,
  }
}

/// The replica at the adopted version: ready when its identity is the adopted record's; otherwise the green is refused on
/// this node, loudly, and the replica dropped.
fn at_adopted(state: &mut ShardState, object: ObjectId, identity: [u8; 32]) -> Engine {
  let reached = state
    .merge
    .replicas
    .get_mut(&object)
    .map(|replica| (replica.head(), replica.head_identity()));
  match reached {
    Some((_, at)) if at == identity => Engine::Ready,
    Some((head, _)) => {
      state.merge.refused.insert(object);
      state.merge.replicas.remove(&object);
      state.count(ADOPTION_MISMATCH, 1);
      slates_machine::log_line!(
        "slates-server: taken-over green {}: this node's replica at version {head} is not the adopted record's \
         identity; the green is refused on this node",
        crate::verbs::hex_of(&object.0)
      );
      Engine::Refused
    }
    None => Engine::Building,
  }
}

/// Takes the green's engine, `object`'s replica, out of this node's replicas, for its owner shard.
pub(crate) fn take_engine(state: &mut ShardState, object: ObjectId) -> Option<Green> {
  state.merge.replicas.remove(&object)
}

/// Gives a refused or unanswered adoption's engine back to this node's replicas, where it waits for the next attempt.
pub(crate) fn give_back(state: &mut ShardState, object: ObjectId, adopted: AdoptedGreen) {
  state.merge.replicas.entry(object).or_insert(adopted.engine);
  state.merge.adoption_completions.insert(
    object,
    Completions::Gathered {
      through: adopted.head,
      completions: adopted.completions,
    },
  );
}

/// Where the gathering of a ledger's completions is ([`gathered_completions`]).
pub(crate) enum Gathered {
  /// Gathered: the completions, taken out of this shard's state.
  Done(Vec<(u64, SubmitCompletion)>),
  /// A gathering began with this ask (or its task could not be spawned, counted): what an attempt that starts work is.
  Began,
  /// The gathering a task of this shard runs is still running.
  Running,
}

/// The submit completions `object`'s ledger carried in versions 1 to `through` (A-255): `Done` with them once gathered;
/// else the gathering begun by this ask, its first slice run at once so a short ledger is gathered in the asking call,
/// or still running in a task of this shard.
pub(crate) fn gathered_completions(
  state: &mut ShardState,
  object: ObjectId,
  through: u64,
) -> Gathered {
  match state.merge.adoption_completions.remove(&object) {
    Some(Completions::Gathered {
      through: gathered,
      completions,
    }) if gathered == through => return Gathered::Done(completions),
    Some(Completions::Gathering) => {
      state
        .merge
        .adoption_completions
        .insert(object, Completions::Gathering);
      return Gathered::Running;
    }
    // None yet, or gathered through another version for an earlier adoption: gathered again.
    Some(Completions::Gathered { .. }) | None => {}
  }
  let mut walk = CompletionsWalk {
    object,
    next: 1,
    through,
    bound: usize::try_from(crate::verbs::completion_bound(state)).unwrap_or(usize::MAX),
    kept: BTreeMap::new(),
  };
  state.count(COMPLETIONS_SLICES, 1);
  let _ = crate::image::sliced(state, |s| walk.step(s));
  if walk.ended() {
    return Gathered::Done(walk.gathered());
  }
  state
    .merge
    .adoption_completions
    .insert(object, Completions::Gathering);
  if slates_rt::futures::spawn(run_gathering(walk))
    .and_then(slates_rt::futures::detach)
    .is_err()
  {
    state.merge.adoption_completions.remove(&object);
    state.count(COMPLETIONS_UNSPAWNED, 1);
  }
  Gathered::Began
}

/// Forgets what this shard gathered or is gathering for `object`'s adoption: the green was adopted, given up on or
/// destroyed. A task still gathering finds its entry gone and keeps nothing.
pub(crate) fn forget_completions(state: &mut ShardState, object: ObjectId) {
  state.merge.adoption_completions.remove(&object);
}

/// The task that gathers a ledger's completions after the first slice, owned by the green's entry in
/// `adoption_completions`: it stores what it gathered there, and nothing once the entry is gone.
async fn run_gathering(mut walk: CompletionsWalk) {
  let object = walk.object;
  loop {
    // A borrow refused after the slice ran discards only the result: the walk's own position says whether it ended.
    let _ = crate::state::with_state_counted(|s| {
      s.count(COMPLETIONS_SLICES, 1);
      crate::image::sliced(s, |s| walk.step(s))
    });
    if walk.ended() {
      break;
    }
    if crate::state::try_with_state(|_| ()).is_err() {
      return;
    }
    slates_rt::futures::yield_now().await;
  }
  let through = walk.through;
  let completions = walk.gathered();
  let _ = crate::state::with_state_counted(|s| {
    if matches!(
      s.merge.adoption_completions.get(&object),
      Some(Completions::Gathering)
    ) {
      s.merge.adoption_completions.insert(
        object,
        Completions::Gathered {
          through,
          completions,
        },
      );
    }
  });
}

/// A ledger's records read a version a unit for the submit completions they carried ([`gathered_completions`]): each
/// client's newest, at most its completion bound, as a green's successor records them (`verbs::completion_bound`). A
/// client holds at most that many unacknowledged at once, so its newest that many are the only ones it may still resend.
struct CompletionsWalk {
  /// The green.
  object: ObjectId,
  /// The next version to read.
  next: u64,
  /// The last version to read.
  through: u64,
  /// The completions kept for each client at most.
  bound: usize,
  /// Each client's newest completions so far, by its host and client id.
  kept: BTreeMap<SubmitClient, Vec<KeptCompletion>>,
}

/// A client whose submits a green's ledger answered, as its completions name it: its host and its client id.
type SubmitClient = (u64, u32);

/// One completion kept for a client: the request's sequence, the version it was answered and the work it came from.
type KeptCompletion = (u32, u64, Option<DbVolumeId>);

impl CompletionsWalk {
  /// Whether every version up to `through` has been read.
  fn ended(&self) -> bool {
    self.next > self.through
  }

  /// One version's record read for its completion; `Some` once every version is read. A version whose record this node
  /// does not hold carries none it can know.
  fn step(&mut self, state: &mut ShardState) -> Result<Option<()>, Refusal> {
    if self.ended() {
      return Ok(Some(()));
    }
    let version = self.next;
    let completion = crate::merge_origin::ledger_value(state, self.object, version)
      .and_then(|value: MergeRecordValue| value.completion);
    if let Some(key) = completion {
      self.keep(key, version);
    }
    match self.next.checked_add(1) {
      Some(next) => self.next = next,
      // The last version there can be: nothing past it to read.
      None => self.through = self.next.saturating_sub(1),
    }
    Ok(self.ended().then_some(()))
  }

  /// Keeps `key`, answered `version`, among its client's newest: past the bound, its oldest request goes.
  fn keep(&mut self, key: SubmitCompletion, version: u64) {
    let kept = self.kept.entry((key.origin, key.client)).or_default();
    kept.push((key.sequence, version, key.work));
    if kept.len() > self.bound
      && let Some(oldest) = kept
        .iter()
        .enumerate()
        .min_by_key(|(_, (sequence, ..))| *sequence)
        .map(|(at, _)| at)
    {
      kept.swap_remove(oldest);
    }
  }

  /// Every completion kept, with its version.
  fn gathered(self) -> Vec<(u64, SubmitCompletion)> {
    self
      .kept
      .into_iter()
      .flat_map(|((origin, client), kept)| {
        kept.into_iter().map(move |(sequence, version, work)| {
          (
            version,
            SubmitCompletion {
              origin,
              client,
              sequence,
              work,
            },
          )
        })
      })
      .collect()
  }
}

/// Re-adopts every green a restarted successor's partitions hold by adoption (A-333), at boot: after every partition
/// has recovered and before any client is admitted, so no verb meets one half built and every work over one is rebuilt
/// after it (`daemon::recover_works_across_partitions`). Each green's engine is rebuilt on this, the control shard, from
/// this node's ledger, a slice at a time, handed to its owner shard, and stepped there through the owner's own chain a
/// slice a call ([`crate::verbs::step_readoption`]). A green that cannot be re-adopted is fenced on its owner shard:
/// every verb on it refused typed, its records left as the evidence (AUD-29-18).
pub(crate) async fn readopt_at_boot(control: u16, shards: &[ShardId]) {
  for shard in shards.iter().copied() {
    let awaiting = crate::xshard::call_within(
      control,
      shard.0,
      |s| {
        s.merge
          .awaiting_adoption
          .iter()
          .map(|(id, awaiting)| (*id, awaiting.through))
          .collect::<Vec<_>>()
      },
      crate::daemon::LIVENESS_BUDGET_NS,
    )
    .await
    .unwrap_or_default();
    for (id, through) in awaiting {
      let readopted = readopt_one(control, shard.0, id, through).await;
      if !readopted {
        let _ = crate::xshard::call_within(
          control,
          shard.0,
          move |s| crate::verbs::fence_awaiting_adoption(s, id),
          crate::daemon::LIVENESS_BUDGET_NS,
        )
        .await;
        let _ = crate::state::with_state_counted(|s| s.count(READOPTION_FENCED, 1));
      }
    }
  }
}

/// Re-adopts green `id` on `owner` through `through` from this node's ledger: whether it now serves there. A hand-off
/// the owner shard does not answer is attempted again from the ledger, as many times as a takeover attempts a green
/// (`fleet::takeover_attempts`), since the engine went with it.
async fn readopt_one(control: u16, owner: u16, id: DbVolumeId, through: u64) -> bool {
  let object = ObjectId(id.bytes);
  let attempts =
    crate::state::with_state(|s| crate::fleet::takeover_attempts(s.failover_ns)).unwrap_or(1);
  for _ in 0..attempts {
    let Some(engine) = rebuilt_engine(object, through).await else {
      return false;
    };
    let began = crate::xshard::call_within(
      control,
      owner,
      move |s| crate::verbs::begin_readoption(s, id, engine),
      crate::daemon::LIVENESS_BUDGET_NS,
    )
    .await;
    match began {
      Some(true) => return stepped_through(control, owner, id).await,
      Some(false) => return false,
      None => {}
    }
  }
  false
}

/// The engine of `object` rebuilt from this node's ledger through `through`, a slice at a time on this shard: version 0
/// seeded from its held inputs and each later version recomputed from its held increment, each checked against its
/// record's identity (`merge_origin::rebuild_replica_through`). `None` when the ledger cannot build it (counted why).
async fn rebuilt_engine(object: ObjectId, through: u64) -> Option<Green> {
  let begun = crate::state::with_state(|s| {
    s.merge.replicas.remove(&object);
    crate::merge_origin::rebuild_replica_through(s, object, through)
  })?;
  if matches!(begun, Rebuild::Nothing) {
    return None;
  }
  // The build's task runs between this task's turns: it ends with the replica installed, or without it, counted why.
  while crate::state::with_state(|s| s.merge.seeding.contains(&object))? {
    slates_rt::futures::yield_now().await;
  }
  crate::state::with_state(|s| {
    let at = s.merge.replicas.get(&object).map(Green::head);
    (at == Some(through))
      .then(|| take_engine(s, object))
      .flatten()
  })
  .flatten()
}

/// Steps green `id`'s replay of its owner's own chain onto its handed engine, a slice a call, until it is installed or
/// fenced: whether it is installed. A call the owner shard does not answer is asked again, the replay waiting there, as
/// many times in a row as a takeover attempts a green (`fleet::takeover_attempts`); past that the green is fenced.
async fn stepped_through(control: u16, owner: u16, id: DbVolumeId) -> bool {
  let bound =
    crate::state::with_state(|s| crate::fleet::takeover_attempts(s.failover_ns)).unwrap_or(1);
  let mut unanswered: u32 = 0;
  while unanswered < bound {
    let step = crate::xshard::call_within(
      control,
      owner,
      move |s| crate::verbs::step_readoption(s, id),
      crate::daemon::LIVENESS_BUDGET_NS,
    )
    .await;
    match step {
      Some(crate::verbs::ReadoptionStep::Installed) => {
        let _ = crate::state::with_state_counted(|s| s.count(READOPTED, 1));
        return true;
      }
      Some(crate::verbs::ReadoptionStep::Fenced) => return false,
      Some(crate::verbs::ReadoptionStep::More) => unanswered = 0,
      None => unanswered = unanswered.saturating_add(1),
    }
    if crate::state::try_with_state(|_| ()).is_err() {
      return false;
    }
    slates_rt::futures::yield_now().await;
  }
  false
}

#[cfg(test)]
mod tests {
  use super::*;

  /// A completion of `client` (host 1) at request `sequence`.
  fn key(client: u32, sequence: u32) -> SubmitCompletion {
    SubmitCompletion {
      origin: 1,
      client,
      sequence,
      work: None,
    }
  }

  /// A-255, A-333: a successor records each client's newest completions, at most the bound a client, whatever order the
  /// ledger's versions carried them in. Do: keep, at a bound of 2, client 7's requests 5, 3, 9 and 4 (versions 1 to 4)
  /// and client 8's request 1 (version 5). Expect client 7's requests 9 and 5 with their versions, and client 8's.
  #[test]
  fn a_ledgers_completions_keep_each_clients_newest_within_its_bound() {
    let mut walk = CompletionsWalk {
      object: ObjectId([1; 16]),
      next: 1,
      through: 5,
      bound: 2,
      kept: BTreeMap::new(),
    };
    for (version, (client, sequence)) in [(7, 5), (7, 3), (7, 9), (7, 4), (8, 1)]
      .into_iter()
      .enumerate()
    {
      walk.keep(key(client, sequence), u64::try_from(version).unwrap() + 1);
    }
    let mut gathered: Vec<(u64, u32, u32)> = walk
      .gathered()
      .into_iter()
      .map(|(version, key)| (version, key.client, key.sequence))
      .collect();
    gathered.sort_unstable();
    assert_eq!(gathered, [(1, 7, 5), (3, 7, 9), (5, 8, 1)]);
  }

  /// §4.16 "mismatch fatal-and-loud" (A-333): a successor's replica becomes the green's engine only at the adopted
  /// record's identity. Do: give two greens a replica at version 0, and ask for each at version 0, one with the replica's
  /// identity and one with another. Expect the first ready with its replica kept for the hand-off; the second refused on
  /// this node, its replica dropped, counted once.
  #[test]
  fn a_replica_off_the_adopted_identity_is_never_adopted() {
    let (ready, refused, kept, dropped, counted) = crate::daemon::audit_on_shard(|state| {
      let (good, bad) = (ObjectId([41; 16]), ObjectId([42; 16]));
      let mut replica = Green::new();
      let identity = replica.head_identity();
      state.merge.replicas.insert(good, replica);
      state.merge.replicas.insert(bad, Green::new());
      let ready = engine_for_adoption(state, good, (0, identity));
      let refused = engine_for_adoption(state, bad, (0, [9; 32]));
      (
        ready,
        refused,
        state.merge.replicas.contains_key(&good),
        !state.merge.replicas.contains_key(&bad) && state.merge.refused.contains(&bad),
        state.refusals.get(ADOPTION_MISMATCH).copied(),
      )
    });
    assert_eq!(ready, Engine::Ready, "the replica at the adopted identity");
    assert_eq!(refused, Engine::Refused, "a replica off it");
    assert!(kept, "the ready replica waits for the hand-off");
    assert!(
      dropped,
      "the replica off the identity is dropped and the green refused"
    );
    assert_eq!(counted, Some(1), "counted once");
  }
}
