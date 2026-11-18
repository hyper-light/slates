//! A green's replicas on the shards that may hold its works (A-457, A-273 step 5a; §4.16 "Rebase off the owner").
//! A green's owner shard makes a replica of it on every other shard of the node, up to the derived limit
//! ([`crate::DaemonConfig::replica_limit`]), before the first change it makes to the green after the green is
//! installed; it feeds every replica each change it makes, in order; and it makes every replica leave before it gives
//! the green's engine up. Nothing reads a replica yet: serving a work's rebases and named-version reads from its
//! shard's replica, and routing a refused one's work to the owner, is step 5b.
//!
//! **The owner.** Before the first change to a green it owns ([`ensure`], called by the submit before its settle),
//! the owner makes one replica for each other shard (`Green::replica`, refused past the limit:
//! `ReplicaRefusal::Full`) and moves it there whole: every index cloned, the content named by the owner's sealed blocks
//! and read in place, never copied (D-7; `slates_mem::sealed`). After each version it commits ([`feed_version`]) and
//! each fold of its history ([`feed_fold`]) it takes the message that carries the change and sends it to every shard
//! it fed, in the order it made them. At each settle of its retention it sets its held limit
//! ([`hold_within`], `slates_merge::replica::held_limit`): past it the slowest replica is evicted. It records which
//! shards hold a replica of each green it owns, and a shard leaves that record only when its replica's registration
//! has come home. Making the replicas before the first change rather than at the install costs a recovered green's
//! clones only if it changes, and needs no other shard to be running while the daemon recovers.
//!
//! **A replica's shard.** It holds the replica within `slates_merge::replica::replica_budget` (the per-green budget,
//! `PartitionCaps::green_chain_bytes`, and the room its shard's memory class has now) and charges the replica's
//! metadata to its shard's budget as memory held for another owner (`ShardBudget::charge_replicated`), recharged after
//! every message. A replica that refuses a message (a gap, an eviction, a replay not its owner's), or whose charge its
//! shard cannot take, leaves: its charge is credited, its registration goes home, and it is counted. It is not made
//! again here: step 5b routes its work to the owner.
//!
//! **Giving the owner up.** A destroyed, fenced or re-installed green's engine is kept, retiring, until every replica
//! of it has left ([`give_up`]); only then is it dropped, so its blocks are freed with no reader left to read them
//! (`slates_mem::sealed`, "Lifetimes"). The daemon's stop closes every shard's replicas ([`close`]) and waits for
//! their registrations ([`outstanding`]) before its shards stop (`Daemon::stop_parts`), so an owner is never dropped
//! with a replica registered (`slates_mem::sealed::dropped_with_readers` stays at zero).
//!
//! **Messages.** Every message rides the cross-shard carrier ([`crate::xshard::hand_reply_home`]): kept until its
//! target's control channel has room, and run on the target even when the target's arena refuses its task, so none is
//! lost while both shards live, in the order it was sent. One the carrier refuses (its target gone, its keep bound
//! reached) is dropped where it was refused. A registration on its way (a replica delivered, a registration going
//! home) or a leave asked of a replica is then owed again from that shard's next replica step ([`OWED`]): a delivery
//! its owner could not send is taken back at once, and the rest are sent again. A feed message or an acknowledgement
//! is counted lost: the replica meets a lost message as a gap and leaves, and a later acknowledgement covers a lost one.
//!
//! Laptop degenerate (R8): with one shard there is no other shard and no replica, and every step here is the same code
//! with nothing to send.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};

use slates_db::catalog::VolumeId;
use slates_mem::sealed::{Left, Passed};
use slates_merge::chain_entry::{Applied, Signature};
use slates_merge::engine::{Green, Increment};
use slates_merge::replica::{Fed, FeedMessage, Replica, ReplicaRefusal, held_limit, replica_budget};

use crate::state::{self, ShardState};

/// Counter: a replica its owner made and sent to another shard.
pub(crate) const SENT: &str = "merge.replica.sent";
/// Counter: a replica its owner did not make: it holds the derived limit already (`ReplicaRefusal::Full`).
pub(crate) const FULL: &str = "merge.replica.full";
/// Counter: a replica its shard took and holds.
pub(crate) const HELD: &str = "merge.replica.held";
/// Counter: a replica its shard did not take, or gave back, for its budget: its metadata past
/// `replica_budget`, or a charge its shard's budget refused.
pub(crate) const OVER_BUDGET: &str = "merge.replica.over_budget";
/// Counter: a feed message a replica applied: the non-vacuity counter of the feed.
pub(crate) const APPLIED: &str = "merge.replica.applied";
/// Counter: a feed message a replica refused (a gap, an eviction, a replay not its owner's), after which it left.
pub(crate) const REFUSED: &str = "merge.replica.refused";
/// Counter: a replica's registration back on its owner.
pub(crate) const LEFT: &str = "merge.replica.left";
/// Counter: a retiring engine dropped once its last replica left.
pub(crate) const RETIRED: &str = "merge.replica.retired";
/// Counter: a message for a replica this shard does not hold (it left, or never came): nothing to do.
pub(crate) const UNHELD: &str = "merge.replica.unheld";
/// Counter: a message the carrier refused (its target gone, its keep bound reached): owed again or lost (module docs).
pub(crate) const NOT_CARRIED: &str = "merge.replica.not_carried";
/// Counter: a registration that came home to no engine: its owner had dropped it.
pub(crate) const STRAY: &str = "merge.replica.stray";

/// This shard's part in the replicas of greens (module docs): as an owner, the shards it fed and the engines it is
/// retiring; as a replica's shard, the replicas it holds.
#[derive(Debug, Default)]
pub(crate) struct GreenReplicas {
  /// As an owner: by green, the replicas of its installed engine. An entry is made before the green's first change
  /// and goes with the engine. Bounded by the greens this shard owns, each by the shards.
  fed: BTreeMap<VolumeId, FedShards>,
  /// As an owner: engines given up while a replica still held them, by green and install, each kept until its last
  /// replica left. Bounded by the greens this shard gave up while replicas held them.
  retiring: BTreeMap<(VolumeId, u64), Green>,
  /// As a replica's shard: by green, the replica held here. Bounded by the greens of the node.
  held: BTreeMap<VolumeId, Held>,
  /// The next install number this shard hands an engine it makes replicas of: a message names its engine by it, so a
  /// re-installed green's replicas are never taken for the old engine's.
  next_install: u64,
  /// Set when the daemon stops: no replica is made, and every one held or arriving is given back.
  closed: bool,
}

/// The replicas of one installed engine.
#[derive(Debug)]
struct FedShards {
  /// The engine's install number.
  install: u64,
  /// The shards whose replica's registration has not come home.
  shards: BTreeSet<u16>,
}

/// A replica held on this shard.
#[derive(Debug)]
struct Held {
  /// Its owner's shard.
  owner: u16,
  /// Its engine's install number on the owner.
  install: u64,
  replica: Replica,
  /// The bytes its metadata charged this shard's budget.
  charged: u64,
}

/// What one shard sends another about a green's replicas.
#[derive(Debug)]
enum Message {
  /// Owner to a replica's shard: a replica of install `install` of `green`, owned on shard `owner`.
  Deliver {
    green: VolumeId,
    install: u64,
    owner: u16,
    replica: Green,
  },
  /// Owner to a replica's shard: the next message of the green's feed.
  Feed {
    green: VolumeId,
    install: u64,
    message: FeedMessage,
  },
  /// Owner to a replica's shard: the owner gives the engine up, and the replica leaves.
  Leave { green: VolumeId, install: u64 },
  /// Replica's shard to the owner: what the replica passed.
  Passed {
    green: VolumeId,
    install: u64,
    passed: Passed,
  },
  /// Replica's shard to the owner: the replica's registration, from shard `from`.
  Left {
    green: VolumeId,
    install: u64,
    from: u16,
    left: Left,
  },
}

impl Message {
  /// Whether a message the carrier dropped is owed again (module docs, "Messages"): a registration on its way or a
  /// leave asked; a feed message and an acknowledgement are counted lost.
  fn owed_again(&self) -> bool {
    matches!(
      self,
      Message::Deliver { .. } | Message::Leave { .. } | Message::Left { .. }
    )
  }
}

thread_local! {
  /// Messages this shard's thread owes again (module docs, "Messages"), with their targets: kept by a dropped message
  /// that was never handled ([`Carried`]) and settled at the shard's next replica step ([`settle_owed`]). Only this
  /// thread touches it, so it needs no lock (D-7). Bounded by the registrations of the node's greens and the leaves
  /// asked of them, one message each.
  static OWED: RefCell<Vec<(u16, Message)>> = const { RefCell::new(Vec::new()) };
}

/// A message on its way to its target. Handled there; or, dropped unhandled (the carrier refused it, or its target's
/// state was out of reach), kept in [`OWED`] where it was dropped when it is owed again.
struct Carried(Option<(u16, Message)>);

impl Drop for Carried {
  fn drop(&mut self) {
    let Some((target, message)) = self.0.take() else {
      return;
    };
    if !message.owed_again() {
      return;
    }
    // Never sent from a drop, which may run inside the shard's borrow of its state or in its thread's teardown: kept
    // for this thread's next replica step. A teardown's is dropped with its shard, which the daemon's stop waits for.
    let _ = OWED.try_with(|owed| {
      if let Ok(mut owed) = owed.try_borrow_mut() {
        owed.push((target, message));
      }
    });
  }
}

/// Sends `message` to `target` on the carrier (module docs, "Messages"); counted when the carrier refuses it.
fn send(state: &mut ShardState, target: u16, message: Message) {
  let carried = Carried(Some((target, message)));
  let sent = crate::xshard::hand_reply_home(target, move || {
    let mut carried = carried;
    state::with_state_counted(move |s| {
      if let Some((_, message)) = carried.0.take() {
        handle(s, message);
      }
    });
  });
  if sent.is_err() {
    state.count(NOT_CARRIED, 1);
  }
}

/// Settles what this shard owes again ([`OWED`]): a delivery this shard could not send as the owner is taken back,
/// a message for this shard is handled here, and the rest are sent again.
fn settle_owed(state: &mut ShardState) {
  let owed = OWED
    .try_with(|owed| owed.try_borrow_mut().map(|mut owed| std::mem::take(&mut *owed)))
    .ok()
    .and_then(Result::ok)
    .unwrap_or_default();
  for (target, message) in owed {
    match message {
      Message::Deliver {
        green,
        install,
        owner,
        replica,
      } if owner == state.shard => {
        if let Some(left) = replica.into_left() {
          left_home(state, (green, install), target, left);
        }
      }
      message if target == state.shard => handle(state, message),
      message => send(state, target, message),
    }
  }
}

/// Handles a message that reached this shard.
fn handle(state: &mut ShardState, message: Message) {
  settle_owed(state);
  match message {
    Message::Deliver {
      green,
      install,
      owner,
      replica,
    } => receive(state, (green, install), owner, replica),
    Message::Feed {
      green,
      install,
      message,
    } => apply(state, (green, install), &message),
    Message::Leave { green, install } => leave_held(state, (green, install)),
    Message::Passed {
      green,
      install,
      passed,
    } => acknowledged(state, (green, install), passed),
    Message::Left {
      green,
      install,
      from,
      left,
    } => left_home(state, (green, install), from, left),
  }
}

/// The owner makes `green`'s replicas on every other shard, up to the derived limit, unless they were made for this
/// install already or the daemon is stopping (module docs, "The owner"). Called before the green's first change.
pub(crate) fn ensure(state: &mut ShardState, green: VolumeId) {
  settle_owed(state);
  let replicas = &state.green_replicas;
  if replicas.closed || replicas.fed.contains_key(&green) || !state.greens.contains_key(&green) {
    return;
  }
  let install = state.green_replicas.next_install;
  state.green_replicas.next_install = install.saturating_add(1);
  state.green_replicas.fed.insert(
    green,
    FedShards {
      install,
      shards: BTreeSet::new(),
    },
  );
  let others: Vec<u16> = state
    .shards
    .iter()
    .copied()
    .filter(|shard| *shard != state.shard)
    .collect();
  for target in others {
    if !make_one(state, (green, install), target) {
      break;
    }
  }
  // A delivery the carrier refused is taken back now.
  settle_owed(state);
}

/// Makes one replica of `green` for `target` and sends it: whether the owner may make another.
fn make_one(state: &mut ShardState, (green, install): (VolumeId, u64), target: u16) -> bool {
  let limit = state.config.replica_limit;
  let Some(engine) = state.greens.get_mut(&green) else {
    return false;
  };
  let replica = match engine.replica(limit) {
    Ok(replica) => replica,
    Err(ReplicaRefusal::Full { .. }) => {
      state.count(FULL, 1);
      return false;
    }
    Err(ReplicaRefusal::NotAnOwner) => return false,
  };
  if let Some(fed) = state.green_replicas.fed.get_mut(&green) {
    fed.shards.insert(target);
  }
  state.count(SENT, 1);
  let owner = state.shard;
  send(
    state,
    target,
    Message::Deliver {
      green,
      install,
      owner,
      replica,
    },
  );
  true
}

/// The shards fed for `green`'s installed engine, and its install number; none when it has no replica.
fn fed_shards(state: &ShardState, green: VolumeId) -> Option<(u64, Vec<u16>)> {
  state
    .green_replicas
    .fed
    .get(&green)
    .filter(|fed| !fed.shards.is_empty())
    .map(|fed| (fed.install, fed.shards.iter().copied().collect()))
}

/// Whether `green`'s installed engine has a replica, so its changes are taken and fed.
pub(crate) fn has_replicas(state: &ShardState, green: VolumeId) -> bool {
  fed_shards(state, green).is_some()
}

/// The owner feeds version `version`, just committed from `increment` as `applied` resolved it and recorded with
/// `signature`, to `green`'s replicas. `applied` is `None` beside an idempotent accept, which changed nothing.
pub(crate) fn feed_version(
  state: &mut ShardState,
  green: VolumeId,
  (version, increment): (u64, &Increment),
  applied: Option<Vec<Applied>>,
  signature: Option<Signature>,
) {
  settle_owed(state);
  let (Some(applied), Some((install, targets))) = (applied, fed_shards(state, green)) else {
    return;
  };
  let message = state
    .greens
    .get_mut(&green)
    .and_then(|engine| engine.feed_version(version, increment.clone(), applied, signature));
  if let Some(message) = message {
    broadcast(state, (green, install), &targets, message);
  }
}

/// The owner feeds the fold that `message` carries ([`Green::fold_to_budget_and_feed`]) to `green`'s replicas.
pub(crate) fn feed_fold(state: &mut ShardState, green: VolumeId, message: Option<FeedMessage>) {
  settle_owed(state);
  let (Some(message), Some((install, targets))) = (message, fed_shards(state, green)) else {
    return;
  };
  broadcast(state, (green, install), &targets, message);
}

/// Sends `message` to every one of `targets`.
fn broadcast(
  state: &mut ShardState,
  (green, install): (VolumeId, u64),
  targets: &[u16],
  message: FeedMessage,
) {
  for target in targets {
    send(
      state,
      *target,
      Message::Feed {
        green,
        install,
        message: message.clone(),
      },
    );
  }
}

/// Sets `engine`'s held limit (`slates_merge::replica::held_limit`): the room `budget`, its per-green budget, leaves
/// beyond its own retention, past which its slowest replica is evicted. Called at each settle of its retention.
pub(crate) fn hold_within(engine: &mut Green, budget: usize) {
  let own = engine
    .retention_charge()
    .saturating_sub(engine.content_held_bytes());
  engine.set_replica_held_limit(held_limit(budget, own));
}

/// The owner gives `green`'s engine up (destroyed, fenced, re-installed): dropped now when no replica holds it, else
/// kept, retiring, until every replica has left, each asked to (module docs, "Giving the owner up"). Whether there was
/// an engine.
pub(crate) fn give_up(state: &mut ShardState, green: VolumeId) -> bool {
  settle_owed(state);
  let fed = state.green_replicas.fed.remove(&green);
  let Some(engine) = state.greens.remove(&green) else {
    return false;
  };
  let Some(fed) = fed.filter(|_| engine.replicas_registered() > 0) else {
    return true;
  };
  for target in &fed.shards {
    send(
      state,
      *target,
      Message::Leave {
        green,
        install: fed.install,
      },
    );
  }
  state
    .green_replicas
    .retiring
    .insert((green, fed.install), engine);
  true
}

/// A replica's shard takes `replica` of `green` from shard `owner`, within its budget and its shard's room, or gives
/// it back (module docs, "A replica's shard").
fn receive(state: &mut ShardState, key: (VolumeId, u64), owner: u16, replica: Green) {
  let replicas = &state.green_replicas;
  if replicas.closed || replicas.held.contains_key(&key.0) {
    give_home(state, key, owner, replica.into_left());
    return;
  }
  let room = usize::try_from(state.store.budget.admittable()).unwrap_or(usize::MAX);
  let budget = replica_budget(crate::merge_service::rejected_cache_budget(state), room);
  let replica = match Replica::new(replica, budget) {
    Ok(replica) => replica,
    Err(not_made) => {
      state.count(OVER_BUDGET, 1);
      give_home(state, key, owner, not_made.left);
      return;
    }
  };
  let bytes = metadata_charge(&replica);
  if state.store.budget.charge_replicated(bytes).is_err() {
    state.count(OVER_BUDGET, 1);
    give_home(state, key, owner, replica.leave());
    return;
  }
  state.green_replicas.held.insert(
    key.0,
    Held {
      owner,
      install: key.1,
      replica,
      charged: bytes,
    },
  );
  state.count(HELD, 1);
}

/// The bytes a replica's metadata charges its shard.
fn metadata_charge(replica: &Replica) -> u64 {
  u64::try_from(replica.green().metadata_bytes()).unwrap_or(u64::MAX)
}

/// Sends `left`, a replica's registration, home to its owner.
fn give_home(state: &mut ShardState, (green, install): (VolumeId, u64), owner: u16, left: Option<Left>) {
  if let Some(left) = left {
    let from = state.shard;
    send(
      state,
      owner,
      Message::Left {
        green,
        install,
        from,
        left,
      },
    );
  }
}

/// The replica of `green` held here, when it is of install `install`.
fn held_mut(state: &mut ShardState, (green, install): (VolumeId, u64)) -> Option<&mut Held> {
  state
    .green_replicas
    .held
    .get_mut(&green)
    .filter(|held| held.install == install)
}

/// A replica's shard applies the feed's next `message` to the replica of `green` it holds: an applied message is
/// acknowledged to the owner and the replica's charge trued up; a refused one, or a charge the shard cannot take, gives
/// the replica back.
fn apply(state: &mut ShardState, key: (VolumeId, u64), message: &FeedMessage) {
  let Some(held) = held_mut(state, key) else {
    state.count(UNHELD, 1);
    return;
  };
  let owner = held.owner;
  match held.replica.apply(message) {
    Ok(Fed::Applied { passed }) => {
      state.count(APPLIED, 1);
      if recharge(state, key.0) {
        send(
          state,
          owner,
          Message::Passed {
            green: key.0,
            install: key.1,
            passed,
          },
        );
      }
    }
    Ok(Fed::PassedOver) => {}
    Err(_) => {
      state.count(REFUSED, 1);
      give_back(state, key.0);
    }
  }
}

/// Trues up the charge of the replica of `green` held here to its metadata now: whether it still holds it (a growth
/// its shard's budget refused gives it back).
fn recharge(state: &mut ShardState, green: VolumeId) -> bool {
  let Some(held) = state.green_replicas.held.get_mut(&green) else {
    return false;
  };
  let (bytes, charged) = (metadata_charge(&held.replica), held.charged);
  held.charged = bytes;
  if bytes <= charged {
    state.store.budget.credit_replicated(charged.saturating_sub(bytes));
    return true;
  }
  if state
    .store
    .budget
    .charge_replicated(bytes.saturating_sub(charged))
    .is_ok()
  {
    return true;
  }
  if let Some(held) = state.green_replicas.held.get_mut(&green) {
    held.charged = charged;
  }
  state.count(OVER_BUDGET, 1);
  give_back(state, green);
  false
}

/// A replica's shard gives the replica of `green` it holds back: its charge credited, its registration sent home.
fn give_back(state: &mut ShardState, green: VolumeId) {
  let Some(held) = state.green_replicas.held.remove(&green) else {
    return;
  };
  state.store.budget.credit_replicated(held.charged);
  give_home(state, (green, held.install), held.owner, held.replica.leave());
}

/// A replica's shard gives back the replica of `green` its owner asks to leave.
fn leave_held(state: &mut ShardState, key: (VolumeId, u64)) {
  if held_mut(state, key).is_some() {
    give_back(state, key.0);
  }
}

/// The owner takes what a replica of its installed engine passed; one of a retiring engine is not needed, since that
/// engine waits only for its replicas to leave.
fn acknowledged(state: &mut ShardState, (green, install): (VolumeId, u64), passed: Passed) {
  let installed = state
    .green_replicas
    .fed
    .get(&green)
    .is_some_and(|fed| fed.install == install);
  if let (true, Some(engine)) = (installed, state.greens.get_mut(&green)) {
    engine.acknowledge_replica(passed);
  }
}

/// The owner takes a replica's registration home from shard `from`: to its installed engine, or to a retiring one,
/// dropped once its last replica has left.
fn left_home(state: &mut ShardState, (green, install): (VolumeId, u64), from: u16, left: Left) {
  state.count(LEFT, 1);
  let replicas = &mut state.green_replicas;
  if let Some(fed) = replicas.fed.get_mut(&green).filter(|fed| fed.install == install) {
    fed.shards.remove(&from);
    if let Some(engine) = state.greens.get_mut(&green) {
      engine.leave_replica(left);
    }
    return;
  }
  let Some(retiring) = replicas.retiring.get_mut(&(green, install)) else {
    state.count(STRAY, 1);
    return;
  };
  retiring.leave_replica(left);
  if retiring.replicas_registered() == 0 {
    replicas.retiring.remove(&(green, install));
    state.count(RETIRED, 1);
  }
}

/// The daemon's stop on this shard (module docs, "Giving the owner up"): no replica is made from now on, and every one
/// held here is given back.
pub(crate) fn close(state: &mut ShardState) {
  state.green_replicas.closed = true;
  let held: Vec<VolumeId> = state.green_replicas.held.keys().copied().collect();
  for green in held {
    give_back(state, green);
  }
  settle_owed(state);
}

/// What this shard still has of its greens' replicas: the registrations its owned and retiring engines hold, the
/// replicas it holds, and the messages it owes again. The daemon's stop waits for none.
pub(crate) fn outstanding(state: &mut ShardState) -> usize {
  settle_owed(state);
  let registered: usize = state
    .greens
    .values()
    .map(Green::replicas_registered)
    .chain(
      state
        .green_replicas
        .retiring
        .values()
        .map(Green::replicas_registered),
    )
    .fold(0, usize::saturating_add);
  let owed = OWED
    .try_with(|owed| owed.try_borrow().map_or(0, |owed| owed.len()))
    .unwrap_or(0);
  registered
    .saturating_add(state.green_replicas.held.len())
    .saturating_add(owed)
}

#[cfg(test)]
mod tests {
  //! A green's replicas through a real daemon's shards (A-457): made on the other shard and fed to the owner's head;
  //! none on a one-shard daemon (R8); and no owner given up, by a destroy or by the daemon's stop, with a replica of it
  //! registered.

  use slates_db::catalog::Principal;
  use slates_ipc::protocol::{ReplyBody, RequestBody};

  use super::*;

  /// The principal the tests act as.
  fn principal() -> Principal {
    Principal::Uid {
      host: slates_db::HostId(0),
      uid: 1234,
    }
  }

  /// A name starting `prefix` that this shard's partition owns, so a test's green and work need no alias elsewhere.
  fn local_name(state: &ShardState, prefix: &str) -> String {
    (0u32..)
      .map(|n| format!("{prefix}-{n}"))
      .find(|name| crate::verbs::owner_of_name(name, state.shards.len()) == state.partition)
      .unwrap()
  }

  /// On `state`, its owner's shard: a green with one work over it and `versions` versions submitted through the work,
  /// each writing its own bytes; the green's id.
  fn green_with_versions(state: &mut ShardState, prefix: &str, versions: u8) -> VolumeId {
    let owner = principal();
    let name = local_name(state, prefix);
    let created = RequestBody::CreateGreen {
      name: name.clone(),
      require_evidence: false,
      base: None,
    };
    let ReplyBody::GreenCreated { id } = crate::verbs::dispatch(state, 1, &owner, created) else {
      panic!("the green was not created");
    };
    let work = RequestBody::CreateWork {
      green: id,
      name: local_name(state, &format!("{name}-work")),
    };
    let ReplyBody::WorkCreated { id: work, .. } = crate::verbs::dispatch(state, 1, &owner, work) else {
      panic!("the work was not created");
    };
    for version in 0..versions {
      submit_one(state, work, version);
    }
    crate::verbs::to_db_volume(id)
  }

  /// Writes `/f` through `work` with bytes of `byte` and submits it: accepted as a new version.
  fn submit_one(state: &mut ShardState, work: slates_ipc::protocol::VolumeId, byte: u8) {
    let owner = principal();
    let edit = RequestBody::Edit {
      work,
      path: "/f".to_owned(),
      at: 0,
      delete_len: 0,
      bytes: vec![byte; 8],
    };
    let edited = crate::verbs::dispatch(state, 1, &owner, edit);
    assert!(matches!(edited, ReplyBody::Edited), "{edited:?}");
    let submit = RequestBody::Submit {
      work,
      evidence: Vec::new(),
    };
    let submitted = crate::verbs::dispatch(state, 1, &owner, submit);
    assert!(
      matches!(
        submitted,
        ReplyBody::Submitted {
          version: Some(_),
          ..
        }
      ),
      "{submitted:?}"
    );
  }

  /// Asks `look` of the shard of partition `at` until it answers, within an observation's budget.
  fn eventually<T: Send + 'static>(
    daemon: &crate::daemon::Daemon,
    at: usize,
    look: impl FnOnce(&mut ShardState) -> Option<T> + Clone + Send + 'static,
  ) -> Option<T> {
    let started = std::time::Instant::now();
    while started.elapsed() < std::time::Duration::from_nanos(crate::daemon::OBSERVE_BUDGET_NS) {
      if let Some(found) = crate::daemon::observe_at(daemon, at, look.clone()) {
        return Some(found);
      }
      std::thread::yield_now();
    }
    None
  }

  /// The replica of `green` held on `state`, its head and identity, when it is at `head`.
  fn held_at(state: &mut ShardState, green: VolumeId, head: (u64, [u8; 32])) -> Option<(u64, [u8; 32])> {
    let replica = state.green_replicas.held.get_mut(&green)?.replica.green_mut();
    let at = (replica.head(), replica.head_identity());
    (at == head).then_some(at)
  }

  /// A-457 (A-273 step 5a). Do: on a two-shard daemon, create a green on the first shard and submit two versions through
  /// a work there; then stop the daemon. Expect: the owner made one replica, for the other shard, and holds its one
  /// registration; the other shard holds it fed to the owner's head (its head and identity the owner's), counted held
  /// once with the versions applied; and the stop drops no owner with a replica registered.
  #[test]
  fn a_two_shard_daemons_green_gets_one_replica_on_the_other_shard_fed_to_its_owners_head() {
    let daemon = crate::daemon::audit_daemon_of(2);
    let (green, head, registered, fed) = crate::daemon::observe_first(&daemon, |state| {
      let green = green_with_versions(state, "replicated", 2);
      let fed: Vec<u16> = state
        .green_replicas
        .fed
        .get(&green)
        .map(|fed| fed.shards.iter().copied().collect())
        .unwrap_or_default();
      let engine = state.greens.get_mut(&green).unwrap();
      let head = (engine.head(), engine.head_identity());
      (green, head, engine.replicas_registered(), fed)
    });
    let other = crate::daemon::observe_at(&daemon, 1, |state| state.shard);
    assert_eq!((registered, fed), (1, vec![other]), "one replica, on the other shard");
    let held = eventually(&daemon, 1, move |state| held_at(state, green, head));
    assert_eq!(held, Some(head), "the replica is fed to the owner's head");
    let counted = crate::daemon::observe_at(&daemon, 1, |state| {
      (
        state.refusals.get(HELD).copied(),
        state.refusals.get(APPLIED).copied().unwrap_or(0),
      )
    });
    assert_eq!(counted.0, Some(1), "held once");
    assert!(counted.1 >= 2, "both versions applied: {counted:?}");
    daemon.stop();
    assert_eq!(slates_mem::sealed::dropped_with_readers(), 0, "no owner dropped with a replica registered");
  }

  /// A-457, R8. Do: on a one-shard daemon, create a green and submit a version. Expect: no replica: the owner fed no
  /// shard, holds no registration, and sent none.
  #[test]
  fn a_one_shard_daemon_makes_no_replica() {
    let seen = crate::daemon::audit_on_shard(|state| {
      let green = green_with_versions(state, "alone", 1);
      (
        state.green_replicas.fed.get(&green).map(|fed| fed.shards.len()),
        state.greens.get(&green).map(Green::replicas_registered),
        state.refusals.get(SENT).copied(),
      )
    });
    assert_eq!(seen, (Some(0), Some(0), None));
  }

  /// A-457 (`slates_mem::sealed`, "Lifetimes"). Do: on a two-shard daemon, give two greens a replica each on the other
  /// shard; destroy the first; stop the daemon with the second still live. Expect: the destroyed green's engine retired,
  /// counted, once its replica left, and the other shard holds no replica of it; and no owner, the destroyed one or the
  /// live one at the stop, was dropped with a replica registered.
  #[test]
  fn giving_the_owner_up_drops_no_reader() {
    let daemon = crate::daemon::audit_daemon_of(2);
    let (destroyed, live) = crate::daemon::observe_first(&daemon, |state| {
      (
        green_with_versions(state, "destroyed", 1),
        green_with_versions(state, "kept", 1),
      )
    });
    let both = eventually(&daemon, 1, move |state| {
      let held = &state.green_replicas.held;
      (held.contains_key(&destroyed) && held.contains_key(&live)).then_some(())
    });
    assert!(both.is_some(), "both replicas held");
    let reply = crate::daemon::observe_first(&daemon, move |state| {
      let volume = crate::verbs::to_wire_volume(destroyed);
      crate::verbs::dispatch(state, 1, &principal(), RequestBody::Destroy { volume })
    });
    assert!(matches!(reply, ReplyBody::Destroyed), "{reply:?}");
    let retired = eventually(&daemon, 0, |state| {
      let retired = state.refusals.get(RETIRED).copied();
      retired.filter(|_| state.green_replicas.retiring.is_empty())
    });
    assert_eq!(retired, Some(1), "retired once its replica left");
    let still_held = crate::daemon::observe_at(&daemon, 1, move |state| {
      state.green_replicas.held.contains_key(&destroyed)
    });
    assert!(!still_held, "the destroyed green's replica was given back");
    daemon.stop();
    assert_eq!(slates_mem::sealed::dropped_with_readers(), 0, "no owner dropped with a replica registered");
  }
}
