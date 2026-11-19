//! A green's replicas on the shards that may hold its works (A-457 and A-465, A-273 steps 5a and 5b; §4.16 "Rebase off
//! the owner"). A green's owner shard makes a replica of it on every other shard of the node, up to the derived limit
//! ([`crate::DaemonConfig::replica_limit`]), before the first change it makes to the green after the green is
//! installed; it feeds every replica each change it makes, in order, a turn's changes in one message after the turn's
//! replies (A-502); and it makes every replica leave before it gives the green's engine up. A work's shard answers from
//! its replica what the work would ask the owner for its base (its files, a page, every file's length, at its base
//! version) and its rebase ([`serving`], `crate::work_home`'s `answered_by_replica`), with no hop to the owner.
//!
//! **Serving (step 5b, A-465).** A replica serves a version it holds, no older than its fold floor and no newer than its
//! head, by the very function the owner answers by (`verbs::base_files`, `base_page`, `base_sizes`, `rebase_on`) over
//! its engine, which is the owner's at the version it holds; so its answer is the owner's for that state, byte for byte
//! (the oracle test below). A rebase is answered at the replica's head, never below the work's base: one whose head lags
//! the owner's moves the work to that head, and its next submit is judged by the owner against its own, as every stale
//! base is. Anything the replica cannot serve is asked of the owner as before, counted by why
//! (`merge.replica.route_absent`, `route_behind`, `route_refused`, `route_evicted`, `route_over_budget`); what it serves
//! is counted (`merge.replica.served`, the non-vacuity counter). A replica that left after a refusal or an eviction is
//! made again before the owner's next change, counted (`merge.replica.remade`); one that left for its budget is not,
//! which bounds the remaking: each remade replica answers for a refusal or an eviction of the one before.
//!
//! **Idle replicas (A-473).** A replica no work on its shard has used for its idle window is given back, its charge
//! credited, counted (`merge.replica.idle_given_back`). The window is the green's own use there: the longest gap the
//! replica has known between two uses, its first gap measured from its holding, so a pause longer than any its works
//! have taken ends it. Only a recorded use arms the check ([`serving`] → `note_use`): one task a shard, while it holds a
//! used replica, sleeps to the first replica's end and gives back what has gone unused; it is never a periodic sweep.
//! A replica never used on its shard has no use to derive a window from, so it is not judged idle: it costs its feed
//! and its charge until its owner gives the engine up. When work returns, the first request a given-back replica would
//! have served is asked of the owner, counted (`merge.replica.route_idle`), and asks the owner, once, to make it again;
//! the owner's next change does, through the remake path above, counted.
//!
//! **The owner.** Before the first change to a green it owns ([`ensure`], called by the submit before its settle),
//! the owner makes one replica for each other shard (`Green::replica`, refused past the limit:
//! `ReplicaRefusal::Full`) and moves it there whole: every index cloned, the content named by the owner's sealed blocks
//! and read in place, never copied (D-7; `slates_mem::sealed`). After each version it commits ([`feed_version`]) and
//! each fold of its history ([`feed_fold`]) it takes the message that carries the change, in the same step, as the
//! sealed blocks' one-message rule asks, and queues it; the turn's flush sends every shard it fed one message with the
//! turn's messages, in the order it made them ("The turn's feed", below). At each settle of its retention it sets its
//! held limit ([`hold_within`], `slates_merge::replica::held_limit`): past it the slowest replica is evicted. It records
//! which shards hold a replica of each green it owns, and a shard leaves that record only when its replica's
//! registration has come home. Making the replicas before the first change rather than at the install costs a recovered
//! green's clones only if it changes, and needs no other shard to be running while the daemon recovers.
//!
//! **The turn's feed (A-502, measured).** Until A-502 the owner sent each message to every replica's shard as it took
//! it, inside the verb that made the change, so a submit's reply waited for one message to each other shard for each of
//! its changes (its settle's fold and its version), and each replica acknowledged each message, a task on the owner
//! apiece. The supervisor's A/B at 1,000 clients (r406 on the Mac Studio, 2026-11-18: three rounds, load 11 to 22) found
//! accepted submits' p99 at a 20 ms think 0.81, 3.88 and 13.6 ms without the replicas and 60.8, 88.9 and 78.0 ms with
//! them, conflicting submits and head reads worse alike. So the owner queues each message ([`queue_feed`]), and one task
//! a shard ([`flush`]), queued behind every task the turn ran or admitted, sends each fed shard the turn's messages in
//! one message once the turn's replies have gone (`merge.replica.feed_sent`); its replica applies them in order and
//! acknowledges once (`merge.replica.acknowledged`), the acknowledgement of the last covering every one before. The
//! owner's sends and the acknowledgements it takes are one a replica a turn, however many changes the turn made, and
//! none goes before a reply. A shard whose arena refuses the flush's task flushes at once, counted
//! (`merge.replica.flush_unarmed`).
//!
//! **A replica's shard.** It holds the replica within `slates_merge::replica::replica_budget` (the per-green budget,
//! `PartitionCaps::green_chain_bytes`, and the room its shard's memory class has now) and charges the replica's
//! metadata to its shard's budget as memory held for another owner (`ShardBudget::charge_replicated`), recharged after
//! every message of the feed it takes. A replica that refuses a message (a gap, an eviction, a replay not its owner's),
//! or whose charge its shard cannot take, leaves: its charge is credited, its registration goes home with why, and it
//! is counted; its work is asked of the owner, and its owner makes it again before its next change unless it left for
//! its budget (A-465).
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
use slates_merge::replica::{
  Fed, FeedMessage, FeedRefusal, Replica, ReplicaRefusal, held_limit, replica_budget,
};

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
/// Counter: a work's base read or rebase answered by this shard's replica of its green, with no hop to the owner
/// (A-465, A-273 step 5b): the non-vacuity counter of the replica's serving.
pub(crate) const SERVED: &str = "merge.replica.served";
/// Counter: a base read or rebase asked of the owner because this shard holds no replica of the green and none left it:
/// never made here, or given back when its owner asked.
pub(crate) const ROUTE_ABSENT: &str = "merge.replica.route_absent";
/// Counter: … because this shard's replica holds no such version: past its head (the feed has not delivered it) or
/// under its fold floor.
pub(crate) const ROUTE_BEHIND: &str = "merge.replica.route_behind";
/// Counter: … because this shard's replica left after refusing a feed message (a gap, a replay not its owner's).
pub(crate) const ROUTE_REFUSED: &str = "merge.replica.route_refused";
/// Counter: … because this shard's replica left once its owner evicted it (it lagged, or a change went unfed).
pub(crate) const ROUTE_EVICTED: &str = "merge.replica.route_evicted";
/// Counter: … because this shard's replica left, or was never taken, for its budget.
pub(crate) const ROUTE_OVER_BUDGET: &str = "merge.replica.route_over_budget";
/// Counter: a replica its owner made again, on a shard whose replica had left after a refusal or an eviction, or for
/// idleness once a work there asked for it again.
pub(crate) const REMADE: &str = "merge.replica.remade";
/// Counter: a replica given back because no work on its shard used it for its idle window, the longest gap it has known
/// between two uses (A-473): its charge credited, its registration sent home.
pub(crate) const IDLE_GIVEN_BACK: &str = "merge.replica.idle_given_back";
/// Counter: … a base read or rebase asked of the owner because this shard's replica was given back for idleness; the
/// first after it asks the owner to make it again.
pub(crate) const ROUTE_IDLE: &str = "merge.replica.route_idle";
/// Counter: a shard's idle check that could not be started (its task arena full): the replicas wait for their next use
/// to arm it again, holding their charge meanwhile.
pub(crate) const IDLE_CHECK_REFUSED: &str = "merge.replica.idle_check_refused";
/// Counter: a turn's feed its owner sent to a replica's shard: one message carrying the feed's messages of every change
/// the owner made to the green in the turn, in order (A-502); one a replica's shard a turn, however many versions the
/// turn committed.
pub(crate) const FEED_SENT: &str = "merge.replica.feed_sent";
/// Counter: a turn's feed sent at once, before the turn's later replies, because the task that sends it after them could
/// not be started (the shard's task arena full) (A-502).
pub(crate) const FLUSH_UNARMED: &str = "merge.replica.flush_unarmed";
/// Counter: a replica's acknowledgement that came home to its owner: one for each turn's feed it applied any message of
/// (A-502).
pub(crate) const ACKNOWLEDGED: &str = "merge.replica.acknowledged";

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
  /// As an owner: by green and install, the feed's messages its engine made this turn, in order, for the turn's flush to
  /// send each shard holding a replica of it in one message once the turn's replies have gone ([`flush`], A-502).
  /// Bounded by the changes one turn of this shard makes: the first message queued arms the flush, which takes them all.
  pending: BTreeMap<(VolumeId, u64), Vec<FeedMessage>>,
  /// Whether this shard's flush is armed (A-502): set by the first message queued after a flush, cleared by the flush.
  flush_armed: bool,
  /// As a replica's shard: by green, the replica held here. Bounded by the greens of the node.
  held: BTreeMap<VolumeId, Held>,
  /// As a replica's shard: by green, why the replica it held left, and whose it was, for the reason a later request is
  /// asked of the owner (A-465) and, for idleness, to ask the owner for it again (A-473). Set when a replica leaves, and
  /// gone when another is held or the owner gives the engine up. Bounded as `held` is.
  gone: BTreeMap<VolumeId, Tomb>,
  /// The next install number this shard hands an engine it makes replicas of: a message names its engine by it, so a
  /// re-installed green's replicas are never taken for the old engine's.
  next_install: u64,
  /// Set when the daemon stops: no replica is made, and every one held or arriving is given back.
  closed: bool,
  /// Whether this shard's idle check runs (A-473): armed when a held replica's use is recorded, and ended when no used
  /// replica is held.
  idle_armed: bool,
}

/// Why a replica that was held on this shard left it, and whose it was.
#[derive(Clone, Copy, Debug)]
struct Tomb {
  gone: Gone,
  owner: u16,
  install: u64,
}

/// The replicas of one installed engine.
#[derive(Debug)]
struct FedShards {
  /// The engine's install number.
  install: u64,
  /// The shards whose replica's registration has not come home.
  shards: BTreeSet<u16>,
  /// The shards whose replica left, and why: made again before the green's next change when it left for a refusal or
  /// an eviction (A-465), or for idleness once a work there has asked for it again (A-473); never for its budget.
  left: BTreeMap<u16, Gone>,
}

/// Why a replica left its shard (A-465).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Gone {
  /// It refused a feed message: a gap, or a replay not its owner's.
  Refused,
  /// Its owner evicted it: it lagged past the held limit, or a change went unfed.
  Evicted,
  /// Its metadata was past its budget, or its shard's budget refused its charge.
  OverBudget,
  /// Its owner asked it to leave, or the daemon stopped.
  Asked,
  /// No work on its shard used it for its idle window (A-473); `wanted` once a work there has asked for it again.
  Idle {
    /// Whether a work on its shard has asked for it since.
    wanted: bool,
  },
}

impl Gone {
  /// Whether its owner makes it again before the green's next change: after a refusal or an eviction, which a fresh
  /// replica does not repeat, or after idleness once a work has asked for it again; never after its budget, which it
  /// would meet again, nor after idleness no work has ended.
  fn remade(self) -> bool {
    matches!(
      self,
      Gone::Refused | Gone::Evicted | Gone::Idle { wanted: true }
    )
  }
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
  /// When a work on this shard last used it (the host's monotonic clock), or it was held, before its first use.
  last_use_ns: u64,
  /// The longest gap it has known between two uses, the first from its holding: its idle window (A-473). None before
  /// its first use, which is what arms its idle check.
  gap_tail_ns: Option<u64>,
}

impl Held {
  /// When it has gone unused for its idle window: none before its first use.
  fn idle_at(&self) -> Option<u64> {
    self
      .gap_tail_ns
      .map(|tail| self.last_use_ns.saturating_add(tail))
  }
}

/// What one shard sends another about a green's replicas. The engine is boxed: each message is moved whole between
/// shards, and the small ones need not carry its size.
#[derive(Debug)]
enum Message {
  /// Owner to a replica's shard: a replica of install `install` of `green`, owned on shard `owner`.
  Deliver {
    green: VolumeId,
    install: u64,
    owner: u16,
    replica: Box<Green>,
  },
  /// Owner to a replica's shard: the next messages of the green's feed, one turn's, in order (A-502).
  Feed {
    green: VolumeId,
    install: u64,
    messages: Vec<FeedMessage>,
  },
  /// Owner to a replica's shard: the owner gives the engine up, and the replica leaves.
  Leave { green: VolumeId, install: u64 },
  /// Replica's shard to the owner: what the replica passed.
  Passed {
    green: VolumeId,
    install: u64,
    passed: Passed,
  },
  /// Replica's shard to the owner: the replica's registration, from shard `from`, and why it left.
  Left {
    green: VolumeId,
    install: u64,
    from: u16,
    left: Left,
    gone: Gone,
  },
  /// Replica's shard to the owner: a work on shard `from` asked for the replica given back there for idleness (A-473).
  Wanted {
    green: VolumeId,
    install: u64,
    from: u16,
  },
}

impl Message {
  /// Whether a message the carrier dropped is owed again (module docs, "Messages"): a registration on its way or a
  /// leave asked; a feed message and an acknowledgement are counted lost.
  fn owed_again(&self) -> bool {
    matches!(
      self,
      Message::Deliver { .. }
        | Message::Leave { .. }
        | Message::Left { .. }
        | Message::Wanted { .. }
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
    .try_with(|owed| {
      owed
        .try_borrow_mut()
        .map(|mut owed| std::mem::take(&mut *owed))
    })
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
        if let Some(left) = (*replica).into_left() {
          left_home(state, (green, install), target, left, Gone::Refused);
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
    } => receive(state, (green, install), owner, *replica),
    Message::Feed {
      green,
      install,
      messages,
    } => apply(state, (green, install), &messages),
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
      gone,
    } => left_home(state, (green, install), from, left, gone),
    Message::Wanted {
      green,
      install,
      from,
    } => wanted(state, (green, install), from),
  }
}

/// The owner makes `green`'s replicas on every other shard, up to the derived limit, before the green's first change;
/// before a later change, again on each shard whose replica left after a refusal or an eviction (A-465), counted; never
/// while the daemon is stopping (module docs, "The owner"). Called before each change.
pub(crate) fn ensure(state: &mut ShardState, green: VolumeId) {
  settle_owed(state);
  if state.green_replicas.closed || !state.greens.contains_key(&green) {
    return;
  }
  let (install, targets, again) = match state.green_replicas.fed.get_mut(&green) {
    Some(fed) => (fed.install, take_remade(fed), true),
    None => (first_install(state, green), other_shards(state), false),
  };
  for target in targets {
    if !make_one(state, (green, install), target) {
      break;
    }
    if again {
      state.count(REMADE, 1);
    }
  }
  // A delivery the carrier refused is taken back now.
  settle_owed(state);
}

/// The shards whose replica of `fed`'s engine is made again now: those that left after a refusal or an eviction, taken
/// out of its left record; one that left for its budget stays out (A-465).
fn take_remade(fed: &mut FedShards) -> Vec<u16> {
  let again: Vec<u16> = fed
    .left
    .iter()
    .filter(|(_, gone)| gone.remade())
    .map(|(shard, _)| *shard)
    .collect();
  fed.left.retain(|_, gone| !gone.remade());
  again
}

/// A record of `green`'s installed engine, under a new install number, before its first change: the number.
fn first_install(state: &mut ShardState, green: VolumeId) -> u64 {
  let install = state.green_replicas.next_install;
  state.green_replicas.next_install = install.saturating_add(1);
  state.green_replicas.fed.insert(
    green,
    FedShards {
      install,
      shards: BTreeSet::new(),
      left: BTreeMap::new(),
    },
  );
  install
}

/// Every shard of the node but this one.
fn other_shards(state: &ShardState) -> Vec<u16> {
  state
    .shards
    .iter()
    .copied()
    .filter(|shard| *shard != state.shard)
    .collect()
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
      replica: Box::new(replica),
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

/// The install number of `green`'s installed engine when it has a replica, so its changes are taken and fed; none
/// otherwise.
fn fed_install(state: &ShardState, green: VolumeId) -> Option<u64> {
  state
    .green_replicas
    .fed
    .get(&green)
    .filter(|fed| !fed.shards.is_empty())
    .map(|fed| fed.install)
}

/// Whether `green`'s installed engine has a replica, so its changes are taken and fed.
pub(crate) fn has_replicas(state: &ShardState, green: VolumeId) -> bool {
  fed_install(state, green).is_some()
}

/// The owner takes the message that feeds version `version`, just committed from `increment` as `applied` resolved it
/// and recorded with `signature`, to `green`'s replicas, and queues it for the turn's flush (A-502). `applied` is `None`
/// beside an idempotent accept, which changed nothing.
pub(crate) fn feed_version(
  state: &mut ShardState,
  green: VolumeId,
  (version, increment): (u64, &Increment),
  applied: Option<Vec<Applied>>,
  signature: Option<Signature>,
) {
  settle_owed(state);
  let (Some(applied), Some(install)) = (applied, fed_install(state, green)) else {
    return;
  };
  let message = state
    .greens
    .get_mut(&green)
    .and_then(|engine| engine.feed_version(version, increment.clone(), applied, signature));
  if let Some(message) = message {
    queue_feed(state, (green, install), message);
  }
}

/// The owner queues the fold that `message` carries ([`Green::fold_to_budget_and_feed`]) for the turn's flush to
/// `green`'s replicas (A-502).
pub(crate) fn feed_fold(state: &mut ShardState, green: VolumeId, message: Option<FeedMessage>) {
  settle_owed(state);
  let (Some(message), Some(install)) = (message, fed_install(state, green)) else {
    return;
  };
  queue_feed(state, (green, install), message);
}

/// Queues `message`, taken from install `install` of `green` in the step that made its change (so the sealed blocks'
/// one-message rule holds whatever waits here), behind the turn's others, and arms the turn's flush (A-502).
fn queue_feed(state: &mut ShardState, key: (VolumeId, u64), message: FeedMessage) {
  state
    .green_replicas
    .pending
    .entry(key)
    .or_default()
    .push(message);
  arm_flush(state);
}

/// Arms this shard's flush, unless it is armed (A-502): one task, queued behind the tasks this turn ran and admitted, so
/// every reply the turn sends goes before the feed. A shard whose arena refuses the task flushes now, counted: the feed
/// then goes before the turn's later replies, as every message did before A-502, rather than wait for a change that may
/// never come.
fn arm_flush(state: &mut ShardState) {
  if state.green_replicas.flush_armed {
    return;
  }
  let flushing = async {
    let _ = state::with_state_counted(flush);
  };
  match slates_rt::futures::spawn(flushing) {
    Ok(task) => {
      let _ = slates_rt::futures::detach(task);
      state.green_replicas.flush_armed = true;
    }
    Err(_) => {
      state.count(FLUSH_UNARMED, 1);
      flush(state);
    }
  }
}

/// The turn's flush (A-502): each engine's queued messages, in order, to every shard holding a replica of it, in one
/// message a shard, counted (`merge.replica.feed_sent`). An engine given up since, or whose replicas have all left, is
/// sent nothing.
fn flush(state: &mut ShardState) {
  state.green_replicas.flush_armed = false;
  let pending = std::mem::take(&mut state.green_replicas.pending);
  for ((green, install), messages) in pending {
    let Some((_, targets)) = fed_shards(state, green).filter(|(fed, _)| *fed == install) else {
      continue;
    };
    send_feed(state, (green, install), &targets, messages);
  }
}

/// Sends `messages`, one turn's of install `install` of `green`, to every one of `targets` in one message each: a copy to
/// each but the last, which takes them.
fn send_feed(
  state: &mut ShardState,
  (green, install): (VolumeId, u64),
  targets: &[u16],
  messages: Vec<FeedMessage>,
) {
  let Some((last, others)) = targets.split_last() else {
    return;
  };
  for target in others {
    send_turn(state, *target, (green, install), messages.clone());
  }
  send_turn(state, *last, (green, install), messages);
}

/// Sends `target` one turn's `messages` of install `install` of `green`, counted (`merge.replica.feed_sent`).
fn send_turn(
  state: &mut ShardState,
  target: u16,
  (green, install): (VolumeId, u64),
  messages: Vec<FeedMessage>,
) {
  state.count(FEED_SENT, 1);
  send(
    state,
    target,
    Message::Feed {
      green,
      install,
      messages,
    },
  );
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
  // Every shard that held a replica of it is asked to leave: one that holds it gives it back, and one whose replica
  // left forgets why (A-465).
  if let Some(fed) = &fed {
    for target in fed.shards.iter().chain(fed.left.keys()) {
      send(
        state,
        *target,
        Message::Leave {
          green,
          install: fed.install,
        },
      );
    }
  }
  let Some(fed) = fed.filter(|_| engine.replicas_registered() > 0) else {
    return true;
  };
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
    give_home(state, key, owner, (replica.into_left(), Gone::Asked));
    return;
  }
  // The shard's free room: what its arena holds and could still claim from its pool, less its commitments (A-98).
  let room = usize::try_from(state.store.admittable()).unwrap_or(usize::MAX);
  let budget = replica_budget(crate::merge_service::rejected_cache_budget(state), room);
  let replica = match Replica::new(replica, budget) {
    Ok(replica) => replica,
    Err(not_made) => {
      state.count(OVER_BUDGET, 1);
      bury(state, key, owner, Gone::OverBudget);
      give_home(state, key, owner, (not_made.left, Gone::OverBudget));
      return;
    }
  };
  let bytes = metadata_charge(&replica);
  if !charge(state, bytes) {
    state.count(OVER_BUDGET, 1);
    bury(state, key, owner, Gone::OverBudget);
    give_home(state, key, owner, (replica.leave(), Gone::OverBudget));
    return;
  }
  state.green_replicas.gone.remove(&key.0);
  state.green_replicas.held.insert(
    key.0,
    Held {
      owner,
      install: key.1,
      replica,
      charged: bytes,
      last_use_ns: slates_machine::clock::monotonic_ns(),
      gap_tail_ns: None,
    },
  );
  state.count(HELD, 1);
}

/// Keeps why the replica of `key`'s green, `owner`'s, left this shard, for the requests asked of its owner from now on.
fn bury(state: &mut ShardState, (green, install): (VolumeId, u64), owner: u16, gone: Gone) {
  state.green_replicas.gone.insert(
    green,
    Tomb {
      gone,
      owner,
      install,
    },
  );
}

/// The bytes a replica's metadata charges its shard.
fn metadata_charge(replica: &Replica) -> u64 {
  u64::try_from(replica.green().metadata_bytes()).unwrap_or(u64::MAX)
}

/// Charges `bytes` of a replica's metadata to this shard's budget as memory held for another owner, the arena grown
/// from its pool first by what the charge lacks, as every admission is (A-98): whether the budget took it.
fn charge(state: &mut ShardState, bytes: u64) -> bool {
  state.store.make_room(bytes);
  state.store.budget.charge_replicated(bytes).is_ok()
}

/// Sends `left`, a replica's registration, home to its owner, with why it left.
fn give_home(
  state: &mut ShardState,
  (green, install): (VolumeId, u64),
  owner: u16,
  (left, gone): (Option<Left>, Gone),
) {
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
        gone,
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

/// A replica's shard applies the feed's next `messages`, one turn of its owner's, in order, to the replica of `green` it
/// holds (A-502): once they are applied, the replica's charge is trued up and the owner told once what it passed, which
/// covers every message before; a refused one, or a charge the shard cannot take, gives the replica back, and the
/// messages after a refused one are not applied.
fn apply(state: &mut ShardState, key: (VolumeId, u64), messages: &[FeedMessage]) {
  let Some(held) = held_mut(state, key) else {
    state.count(UNHELD, 1);
    return;
  };
  let owner = held.owner;
  let replayed = replay(&mut held.replica, messages);
  state.count(APPLIED, replayed.applied);
  if let Some(refusal) = replayed.refused {
    state.count(REFUSED, 1);
    give_back(state, key.0, gone_for(refusal));
    return;
  }
  if let Some(passed) = replayed.passed
    && recharge(state, key.0)
  {
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

/// What a replica made of one turn's messages of its feed (A-502).
struct Replayed {
  /// The messages it applied.
  applied: u64,
  /// What it passed after the last it applied: its owner frees by it.
  passed: Option<Passed>,
  /// Why it refused the message it stopped at, if it did.
  refused: Option<FeedRefusal>,
}

/// Applies `messages` to `replica` in order, stopping at the first it refuses (A-502).
fn replay(replica: &mut Replica, messages: &[FeedMessage]) -> Replayed {
  let mut replayed = Replayed {
    applied: 0,
    passed: None,
    refused: None,
  };
  for message in messages {
    match replica.apply(message) {
      Ok(Fed::Applied { passed }) => {
        replayed.applied = replayed.applied.saturating_add(1);
        replayed.passed = Some(passed);
      }
      Ok(Fed::PassedOver) => {}
      Err(refusal) => {
        replayed.refused = Some(refusal);
        break;
      }
    }
  }
  replayed
}

/// Why a replica that refused a feed message with `refusal` leaves (A-465).
fn gone_for(refusal: FeedRefusal) -> Gone {
  match refusal {
    FeedRefusal::Evicted => Gone::Evicted,
    FeedRefusal::OverBudget { .. } => Gone::OverBudget,
    _ => Gone::Refused,
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
    state
      .store
      .budget
      .credit_replicated(charged.saturating_sub(bytes));
    return true;
  }
  if charge(state, bytes.saturating_sub(charged)) {
    return true;
  }
  if let Some(held) = state.green_replicas.held.get_mut(&green) {
    held.charged = charged;
  }
  state.count(OVER_BUDGET, 1);
  give_back(state, green, Gone::OverBudget);
  false
}

/// A replica's shard gives the replica of `green` it holds back, for `gone`: its charge credited, its registration sent
/// home with why, and why kept here for the requests asked of the owner from now on (none when it was asked to).
fn give_back(state: &mut ShardState, green: VolumeId, gone: Gone) {
  let Some(held) = state.green_replicas.held.remove(&green) else {
    return;
  };
  state.store.budget.credit_replicated(held.charged);
  if gone != Gone::Asked {
    bury(state, (green, held.install), held.owner, gone);
  }
  give_home(
    state,
    (green, held.install),
    held.owner,
    (held.replica.leave(), gone),
  );
}

/// A replica's shard gives back the replica of `green` its owner asks to leave, and forgets why one left before: the
/// owner gave the engine up.
fn leave_held(state: &mut ShardState, key: (VolumeId, u64)) {
  if held_mut(state, key).is_some() {
    give_back(state, key.0, Gone::Asked);
  }
  state.green_replicas.gone.remove(&key.0);
}

/// The owner takes what a replica of its installed engine passed; one of a retiring engine is not needed, since that
/// engine waits only for its replicas to leave.
fn acknowledged(state: &mut ShardState, (green, install): (VolumeId, u64), passed: Passed) {
  state.count(ACKNOWLEDGED, 1);
  let installed = state
    .green_replicas
    .fed
    .get(&green)
    .is_some_and(|fed| fed.install == install);
  if let (true, Some(engine)) = (installed, state.greens.get_mut(&green)) {
    engine.acknowledge_replica(passed);
  }
}

/// The owner takes a replica's registration home from shard `from`, which gave it back for `gone`: to its installed
/// engine, which records why for its next change (A-465), or to a retiring one, dropped once its last replica has left.
fn left_home(
  state: &mut ShardState,
  (green, install): (VolumeId, u64),
  from: u16,
  left: Left,
  gone: Gone,
) {
  state.count(LEFT, 1);
  let replicas = &mut state.green_replicas;
  if let Some(fed) = replicas
    .fed
    .get_mut(&green)
    .filter(|fed| fed.install == install)
  {
    fed.shards.remove(&from);
    if gone != Gone::Asked {
      fed.left.insert(from, gone);
    }
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

/// The daemon's stop on this shard (module docs, "Giving the owner up"): no replica is made from now on, the feed this
/// turn queued goes nowhere, since every replica leaves, and every one held here is given back.
pub(crate) fn close(state: &mut ShardState) {
  state.green_replicas.closed = true;
  state.green_replicas.pending.clear();
  let held: Vec<VolumeId> = state.green_replicas.held.keys().copied().collect();
  for green in held {
    give_back(state, green, Gone::Asked);
  }
  settle_owed(state);
}

/// The replica of `green` this shard holds, to answer a work's base read at version `asked` or the rebase of a work
/// based there (A-465, A-273 step 5b): when it holds that version, no older than its fold floor and no newer than its
/// head. Otherwise none, counted by why the owner is asked instead: no replica here, never made or given back when its
/// owner asked; one that left after a refusal, an eviction or for its budget; or one that holds no such version.
pub(crate) fn serving(state: &mut ShardState, green: VolumeId, asked: u64) -> Option<&mut Green> {
  if let Some(route) = route_reason(&state.green_replicas, green, asked) {
    state.count(route, 1);
    if route == ROUTE_IDLE {
      want_again(state, green);
    }
    return None;
  }
  note_use(state, green);
  state
    .green_replicas
    .held
    .get_mut(&green)
    .map(|held| held.replica.green_mut())
}

/// Records a use of the replica of `green` held here now (A-473): its gap since the last joins its gap tail, the longest
/// it has known between two uses (the first gap from its holding), which is its idle window; and the shard's idle check
/// is armed, if it does not run already. Only a recorded use arms it: there is no sweep.
fn note_use(state: &mut ShardState, green: VolumeId) {
  let now = slates_machine::clock::monotonic_ns();
  let Some(held) = state.green_replicas.held.get_mut(&green) else {
    return;
  };
  let gap = now.saturating_sub(held.last_use_ns);
  held.gap_tail_ns = Some(held.gap_tail_ns.map_or(gap, |tail| tail.max(gap)));
  held.last_use_ns = now;
  if state.green_replicas.idle_armed {
    return;
  }
  match slates_rt::futures::spawn(idle_check()) {
    Ok(task) => {
      let _ = slates_rt::futures::detach(task);
      state.green_replicas.idle_armed = true;
    }
    Err(_) => state.count(IDLE_CHECK_REFUSED, 1),
  }
}

/// The shard's idle check while it holds a used replica (A-473): sleeps to the moment the first of them has gone
/// unused for its idle window, gives back every one that has, and goes on while any used replica is held; it ends
/// when none is, and the next recorded use arms it again.
async fn idle_check() {
  loop {
    let Some(wait) = state::with_state(next_idle_wait).flatten() else {
      return;
    };
    // A refused sleep (the timer wheel full) still checks now; the next turn sleeps again.
    let _ = slates_rt::futures::sleep(wait.max(1)).await;
    let _ = state::with_state_counted(give_back_idle);
  }
}

/// How long until the first held replica has gone unused for its idle window; none, with the check disarmed, when no
/// used replica is held.
fn next_idle_wait(state: &mut ShardState) -> Option<u64> {
  let now = slates_machine::clock::monotonic_ns();
  let first = state
    .green_replicas
    .held
    .values()
    .filter_map(Held::idle_at)
    .min();
  if first.is_none() {
    state.green_replicas.idle_armed = false;
  }
  first.map(|at| at.saturating_sub(now))
}

/// Gives back every held replica that has gone unused for its idle window (A-473), counted: its charge credited, its
/// registration sent home, and its owner makes it again once a work here asks for it.
fn give_back_idle(state: &mut ShardState) {
  let now = slates_machine::clock::monotonic_ns();
  let idle: Vec<VolumeId> = state
    .green_replicas
    .held
    .iter()
    .filter(|(_, held)| held.idle_at().is_some_and(|at| at <= now))
    .map(|(green, _)| *green)
    .collect();
  for green in idle {
    state.count(IDLE_GIVEN_BACK, 1);
    give_back(state, green, Gone::Idle { wanted: false });
  }
}

/// A work here asked for the replica of `green` given back for idleness: its owner is told, once, so its next change
/// makes it again (A-473).
fn want_again(state: &mut ShardState, green: VolumeId) {
  let Some(tomb) = state.green_replicas.gone.get_mut(&green) else {
    return;
  };
  if tomb.gone != (Gone::Idle { wanted: false }) {
    return;
  }
  tomb.gone = Gone::Idle { wanted: true };
  let (owner, install, from) = (tomb.owner, tomb.install, state.shard);
  send(
    state,
    owner,
    Message::Wanted {
      green,
      install,
      from,
    },
  );
}

/// The owner learns a work on shard `from` asked for its replica of `green`, given back there for idleness: it is made
/// again before the green's next change (A-473).
fn wanted(state: &mut ShardState, (green, install): (VolumeId, u64), from: u16) {
  let Some(fed) = state
    .green_replicas
    .fed
    .get_mut(&green)
    .filter(|fed| fed.install == install)
  else {
    return;
  };
  if let Some(gone) = fed.left.get_mut(&from)
    && matches!(gone, Gone::Idle { .. })
  {
    *gone = Gone::Idle { wanted: true };
  }
}

/// Why a request for `green` at version `asked` is asked of the owner rather than served by this shard's replica (the
/// counter it moves); none when the replica holds the version.
fn route_reason(replicas: &GreenReplicas, green: VolumeId, asked: u64) -> Option<&'static str> {
  let Some(held) = replicas.held.get(&green) else {
    return Some(match replicas.gone.get(&green).map(|tomb| tomb.gone) {
      Some(Gone::Idle { .. }) => ROUTE_IDLE,
      Some(Gone::Refused) => ROUTE_REFUSED,
      Some(Gone::Evicted) => ROUTE_EVICTED,
      Some(Gone::OverBudget) => ROUTE_OVER_BUDGET,
      Some(Gone::Asked) | None => ROUTE_ABSENT,
    });
  };
  let replica = held.replica.green();
  let holds = asked >= replica.folded_below() && asked <= replica.head();
  (!holds).then_some(ROUTE_BEHIND)
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
    let (green, work) = green_and_work(state, prefix);
    for version in 0..versions {
      submit_one(state, work, version);
    }
    green
  }

  /// On `state`, its owner's shard: a green and one work over it; their ids.
  fn green_and_work(
    state: &mut ShardState,
    prefix: &str,
  ) -> (VolumeId, slates_ipc::protocol::VolumeId) {
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
    let ReplyBody::WorkCreated { id: work, .. } = crate::verbs::dispatch(state, 1, &owner, work)
    else {
      panic!("the work was not created");
    };
    (crate::verbs::to_db_volume(id), work)
  }

  /// Writes `/f` through `work` with bytes of `byte` and submits it: accepted as a new version.
  fn submit_one(state: &mut ShardState, work: slates_ipc::protocol::VolumeId, byte: u8) {
    submit_edit(state, work, (0, byte));
  }

  /// Writes eight bytes of `byte` at the start of `/f` through `work`, in place of the `replaced` bytes there, and submits
  /// it: accepted as a new version.
  fn submit_edit(
    state: &mut ShardState,
    work: slates_ipc::protocol::VolumeId,
    (replaced, byte): (u64, u8),
  ) {
    let owner = principal();
    let edit = RequestBody::Edit {
      work,
      path: "/f".to_owned(),
      at: 0,
      delete_len: replaced,
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
  fn held_at(
    state: &mut ShardState,
    green: VolumeId,
    head: (u64, [u8; 32]),
  ) -> Option<(u64, [u8; 32])> {
    let replica = state
      .green_replicas
      .held
      .get_mut(&green)?
      .replica
      .green_mut();
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
    assert_eq!(
      (registered, fed),
      (1, vec![other]),
      "one replica, on the other shard"
    );
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
    assert_eq!(
      slates_mem::sealed::dropped_with_readers(),
      0,
      "no owner dropped with a replica registered"
    );
  }

  /// A-457, R8. Do: on a one-shard daemon, create a green and submit a version. Expect: no replica: the owner fed no
  /// shard, holds no registration, and sent none.
  #[test]
  fn a_one_shard_daemon_makes_no_replica() {
    let seen = crate::daemon::audit_on_shard(|state| {
      let green = green_with_versions(state, "alone", 1);
      (
        state
          .green_replicas
          .fed
          .get(&green)
          .map(|fed| fed.shards.len()),
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
    assert_eq!(
      slates_mem::sealed::dropped_with_readers(),
      0,
      "no owner dropped with a replica registered"
    );
  }

  /// An increment, `tag`'s, based at `base`, that writes `bytes` over the start of `path`, creating it when `create`.
  fn increment(
    tag: u8,
    base: u64,
    (path, create): (&str, bool),
    bytes: &[u8],
  ) -> slates_merge::engine::Increment {
    use slates_merge::ops_doc::{Op, OpKind, OpsDoc};
    let mut doc = OpsDoc::new();
    let index = doc.paths.intern(path);
    let len = u64::try_from(bytes.len()).unwrap();
    let op = |kind, len, src| Op {
      kind,
      flags: 0,
      path: index,
      at: 0,
      len,
      src,
    };
    if create {
      doc.ops.push(op(OpKind::Create, 0, u64::MAX));
      doc.ops.push(op(OpKind::Insert, len, 0));
    } else {
      doc.ops.push(op(OpKind::Overwrite, len, 0));
    }
    slates_merge::engine::Increment {
      id: [tag; 32],
      base,
      doc,
      post_state: bytes.to_vec(),
      evidence: Vec::new(),
    }
  }

  /// A work of `green` based at version 1 whose increment, composed on its shard, is `increment`: what its shard moves
  /// to be rebased.
  fn sealed(
    green: VolumeId,
    increment: slates_merge::engine::Increment,
  ) -> crate::work_home::SealedWork {
    crate::work_home::SealedWork {
      work: VolumeId { bytes: [0x5e; 16] },
      green,
      base_version: 1,
      journal: Vec::new(),
      files: crate::work_files::WorkFiles::default(),
      composed: Some(increment),
    }
  }

  /// What the green's engine at `state` answers of a work's base reads at version 1 (its files and their entries, a
  /// page, every file's length) and of two rebases from version 1 (one clean, one that conflicts), each whole: by the
  /// owner's functions on its own shard, or by the replica's on another.
  fn answers(state: &mut ShardState, green: VolumeId, on_owner: bool) -> Vec<String> {
    let path = crate::merge_service::canonical_path("/f").to_owned();
    let paths = vec![path.clone(), "absent".to_owned()];
    let clean = sealed(green, increment(7, 1, ("g", true), b"new"));
    let conflict = sealed(green, increment(8, 1, (&path, false), b"theirs"));
    if on_owner {
      return vec![
        format!("{:?}", crate::verbs::owner_files(state, green, 1, &paths)),
        format!(
          "{:?}",
          crate::verbs::owner_page(state, green, 1, path, (2, 4))
        ),
        format!("{:?}", crate::verbs::owner_sizes(state, green, 1)),
        format!("{:?}", crate::verbs::owner_rebase(state, clean)),
        format!("{:?}", crate::verbs::owner_rebase(state, conflict)),
      ];
    }
    let files = crate::verbs::base_files(serving(state, green, 1).map(|e| &*e), 1, &paths);
    let page = crate::verbs::base_page(serving(state, green, 1).map(|e| &*e), 1, path, (2, 4));
    let sizes = crate::verbs::base_sizes(serving(state, green, 1).map(|e| &*e), 1);
    vec![
      format!("{files:?}"),
      format!("{page:?}"),
      format!("{sizes:?}"),
      format!(
        "{:?}",
        crate::verbs::rebase_on(serving(state, green, 1), &clean)
      ),
      format!(
        "{:?}",
        crate::verbs::rebase_on(serving(state, green, 1), &conflict)
      ),
    ]
  }

  /// A-465 (A-273 step 5b), the oracle: what a work's shard asks of its green, answered by the replica there and by the
  /// owner, compared whole. Do: on a two-shard daemon, a green on the first shard with two versions, the second
  /// replacing the first's bytes; once the second shard's replica holds its head, ask both the same: a work's base files
  /// at version 1 (one there, one not, with their entries), a page of one, every file's length, a clean rebase from
  /// version 1, and one that conflicts with version 2. Expect: every answer the same, byte for byte, the rebases at the
  /// same head.
  #[test]
  fn what_a_replica_answers_is_the_owners_answer_whole() {
    let daemon = crate::daemon::audit_daemon_of(2);
    let (green, head) = crate::daemon::observe_first(&daemon, |state| {
      let (green, work) = green_and_work(state, "oracle");
      submit_edit(state, work, (0, 0));
      submit_edit(state, work, (8, 1));
      let engine = state.greens.get_mut(&green).unwrap();
      (green, (engine.head(), engine.head_identity()))
    });
    let held = eventually(&daemon, 1, move |state| held_at(state, green, head));
    assert_eq!(held, Some(head), "the replica holds the owner's head");
    let owner = crate::daemon::observe_first(&daemon, move |state| answers(state, green, true));
    let replica = crate::daemon::observe_at(&daemon, 1, move |state| answers(state, green, false));
    assert_eq!(replica, owner, "the replica's answers are the owner's");
    assert!(
      owner.iter().any(|answer| answer.starts_with("Rebased {")),
      "a clean rebase among them: {owner:?}"
    );
    assert!(
      owner
        .iter()
        .any(|answer| answer.starts_with("Reply(Rebased {")),
      "and a conflict: {owner:?}"
    );
    daemon.stop();
  }

  /// Runs `body` on `state` as a client's request resumed there runs (`lease_wait::run_resumed`): its gate parks it when
  /// it needs its green, and the answer resumes it later.
  fn run_as_request(state: &mut ShardState, word: u64, body: RequestBody) {
    let verb = crate::lease_wait::ResumedVerb {
      route: None,
      origin: 0,
      id: slates_wire::request::RequestId::from_word(word),
      client_id: 1,
      principal: principal(),
      body,
    };
    crate::lease_wait::run_resumed(state, verb, None, None);
  }

  /// The second shard's counters of what its replicas served and what it asked of owners.
  fn served_and_asked(state: &mut ShardState) -> (u64, u64) {
    (
      state.refusals.get(SERVED).copied().unwrap_or(0),
      state
        .refusals
        .get(crate::work_home::ASKED_THE_OWNER)
        .copied()
        .unwrap_or(0),
    )
  }

  /// A-465 (A-273 step 5b). Do: on a two-shard daemon, a green on the first shard at version 1; a work over it made on
  /// the second shard through its gate, as a client's request is (its admission asks the owner); a second version on
  /// the first shard; once the second shard's replica holds it, the work makes a file and is rebased, both through the
  /// gate. Expect: the edit's base read and the rebase each answered by the replica (`merge.replica.served` moved by
  /// two) with nothing asked of the owner (`merge.work.asked_owner` unchanged), and the work moved to the head.
  #[test]
  fn a_works_base_read_and_rebase_on_its_replicas_shard_are_served_there_with_no_hop() {
    let daemon = crate::daemon::audit_daemon_of(2);
    let (green, work0) = crate::daemon::observe_first(&daemon, |state| {
      let (green, work) = green_and_work(state, "served");
      submit_one(state, work, 1);
      (green, work)
    });
    let name = crate::daemon::observe_at(&daemon, 1, |state| local_name(state, "served-off"));
    let create = RequestBody::CreateWork {
      green: crate::verbs::to_wire_volume(green),
      name: name.clone(),
    };
    crate::daemon::observe_at(&daemon, 1, move |state| run_as_request(state, 1, create));
    let work = eventually(&daemon, 1, move |state| {
      let record = state.db.partition().volume_by_name(&name)?;
      state.works.contains_key(&record.id).then_some(record.id)
    })
    .expect("the work was made on the second shard");
    let head = crate::daemon::observe_first(&daemon, move |state| {
      submit_one(state, work0, 2);
      let engine = state.greens.get_mut(&green).unwrap();
      (engine.head(), engine.head_identity())
    });
    assert!(eventually(&daemon, 1, move |state| held_at(state, green, head)).is_some());
    let before = crate::daemon::observe_at(&daemon, 1, served_and_asked);
    let edit = RequestBody::Edit {
      work: crate::verbs::to_wire_volume(work),
      path: "/g".to_owned(),
      at: 0,
      delete_len: 0,
      bytes: b"made here".to_vec(),
    };
    let rebase = RequestBody::Rebase {
      work: crate::verbs::to_wire_volume(work),
    };
    crate::daemon::observe_at(&daemon, 1, move |state| run_as_request(state, 2, edit));
    let edited = eventually(&daemon, 1, move |state| {
      (!state.works.get(&work)?.journal.is_empty()).then_some(())
    });
    assert!(edited.is_some(), "the edit ran");
    crate::daemon::observe_at(&daemon, 1, move |state| run_as_request(state, 3, rebase));
    let moved = eventually(&daemon, 1, move |state| {
      (state.works.get(&work)?.base_version == head.0).then_some(())
    });
    assert!(moved.is_some(), "the work moved to the owner's head");
    let after = crate::daemon::observe_at(&daemon, 1, served_and_asked);
    assert_eq!(
      (after.0.saturating_sub(before.0), after.1),
      (2, before.1),
      "both served by the replica, nothing asked of the owner"
    );
    daemon.stop();
  }

  /// A-465 (A-273 step 5b). Do: on a two-shard daemon, a green with a replica at its head on the second shard; ask that
  /// shard's replica for a version past the head and for a green it holds no replica of; give the replica back as
  /// though its owner had evicted it, and ask again; make a version on the owner; then give the remade replica back for
  /// its budget, make another version, and ask again. Expect: each request asked of the owner counted by why (behind,
  /// absent, evicted, over its budget); the evicted replica made again before the next change, counted, and held again;
  /// the one that left for its budget not made again.
  #[test]
  fn a_replica_that_left_is_remade_unless_for_its_budget_and_each_route_is_counted_by_why() {
    let daemon = crate::daemon::audit_daemon_of(2);
    let (green, work) = crate::daemon::observe_first(&daemon, |state| {
      let (green, work) = green_and_work(state, "routed");
      submit_one(state, work, 1);
      (green, work)
    });
    let head = crate::daemon::observe_first(&daemon, move |state| {
      let engine = state.greens.get_mut(&green).unwrap();
      (engine.head(), engine.head_identity())
    });
    assert!(eventually(&daemon, 1, move |state| held_at(state, green, head)).is_some());
    let other = VolumeId { bytes: [0x0f; 16] };
    let routes = crate::daemon::observe_at(&daemon, 1, move |state| {
      let behind = serving(state, green, head.0 + 1).is_none();
      let absent = serving(state, other, 1).is_none();
      give_back(state, green, Gone::Evicted);
      let evicted = serving(state, green, 1).is_none();
      [behind, absent, evicted]
    });
    assert_eq!(routes, [true; 3]);
    let remade = crate::daemon::observe_first(&daemon, move |state| {
      submit_one(state, work, 2);
      state.refusals.get(REMADE).copied()
    });
    let held_again = eventually(&daemon, 1, move |state| {
      (state.refusals.get(HELD).copied() == Some(2)).then_some(())
    });
    let over = crate::daemon::observe_at(&daemon, 1, move |state| {
      give_back(state, green, Gone::OverBudget);
      serving(state, green, 1).is_none()
    });
    let not_remade = crate::daemon::observe_first(&daemon, move |state| {
      submit_one(state, work, 3);
      state.refusals.get(REMADE).copied()
    });
    let counted = crate::daemon::observe_at(&daemon, 1, |state| {
      [ROUTE_BEHIND, ROUTE_ABSENT, ROUTE_EVICTED, ROUTE_OVER_BUDGET]
        .map(|route| state.refusals.get(route).copied().unwrap_or(0))
    });
    assert_eq!(
      (remade, held_again.is_some()),
      (Some(1), true),
      "remade once, held again"
    );
    assert!(over, "its budget's leave routes to the owner");
    assert_eq!(not_remade, Some(1), "never remade for its budget");
    assert_eq!(counted, [1, 1, 1, 1], "each route counted by why");
    daemon.stop();
  }

  /// A-473. Do: on a two-shard daemon, a green with a replica on the second shard; use the replica twice there (a work's
  /// base read at version 1, as `start_ask` asks it), then use it no more; once it is given back, ask it again, and make
  /// a version on the owner. Expect: the replica given back once unused for its idle window, counted, its charge on its
  /// shard credited to nothing; the next request asked of the owner as idle, counted; and the owner's next change making
  /// it again, counted, so the shard holds it again.
  #[test]
  fn an_unused_replica_is_given_back_after_its_idle_window_and_made_again_when_work_returns() {
    let daemon = crate::daemon::audit_daemon_of(2);
    let (green, work) = crate::daemon::observe_first(&daemon, |state| {
      let (green, work) = green_and_work(state, "idle");
      submit_one(state, work, 1);
      (green, work)
    });
    let head = crate::daemon::observe_first(&daemon, move |state| {
      let engine = state.greens.get_mut(&green).unwrap();
      (engine.head(), engine.head_identity())
    });
    assert!(eventually(&daemon, 1, move |state| held_at(state, green, head)).is_some());
    let charged = crate::daemon::observe_at(&daemon, 1, |state| state.store.budget.replicated());
    let used = crate::daemon::observe_at(&daemon, 1, move |state| {
      [
        serving(state, green, 1).is_some(),
        serving(state, green, 1).is_some(),
      ]
    });
    assert_eq!(used, [true; 2], "the replica serves version 1, twice");
    let given_back = eventually(&daemon, 1, move |state| {
      let idle = state.refusals.get(IDLE_GIVEN_BACK).copied();
      idle
        .filter(|_| !state.green_replicas.held.contains_key(&green))
        .map(|idle| (idle, state.store.budget.replicated()))
    });
    assert_eq!(
      given_back,
      Some((1, 0)),
      "given back once idle, its charge {charged} credited"
    );
    let routed = crate::daemon::observe_at(&daemon, 1, move |state| {
      (
        serving(state, green, 1).is_none(),
        state.refusals.get(ROUTE_IDLE).copied(),
      )
    });
    assert_eq!(
      routed,
      (true, Some(1)),
      "work returned: asked of the owner as idle"
    );
    let remade = crate::daemon::observe_first(&daemon, move |state| {
      submit_one(state, work, 2);
      state.refusals.get(REMADE).copied()
    });
    assert_eq!(remade, Some(1), "made again at the owner's next change");
    let held_again = eventually(&daemon, 1, move |state| {
      (state.refusals.get(HELD).copied() == Some(2)).then_some(())
    });
    assert!(held_again.is_some(), "held again");
    daemon.stop();
  }

  /// The owner's counts of the feed's messages it sent and of its replicas' acknowledgements that came home.
  fn sent_and_acknowledged(state: &mut ShardState) -> [u64; 2] {
    [FEED_SENT, ACKNOWLEDGED].map(|counter| state.refusals.get(counter).copied().unwrap_or(0))
  }

  /// The owner's head of `green` and its identity.
  fn head_of(state: &mut ShardState, green: VolumeId) -> (u64, [u8; 32]) {
    let engine = state.greens.get_mut(&green).unwrap();
    (engine.head(), engine.head_identity())
  }

  /// Whether the replicas on the second and third shards both hold `head` of `green`, within an observation's budget
  /// each. A replica sends its acknowledgement in the step that applies the feed, so once both hold it, an observation
  /// of the owner made after this sees every acknowledgement of that feed home.
  fn both_hold(daemon: &crate::daemon::Daemon, green: VolumeId, head: (u64, [u8; 32])) -> bool {
    [1, 2]
      .into_iter()
      .all(|at| eventually(daemon, at, move |state| held_at(state, green, head)).is_some())
  }

  /// A-502. Do: on a three-shard daemon, a green on the first shard with one version, held at its head by the replicas
  /// on the other two shards; then, in one turn of the owner, three more versions submitted, each reply read before the
  /// next submit. Expect: no message of the feed sent before any of the turn's replies (before A-502 each reply waited
  /// for one message a replica for each of the version's changes); once the turn is over, both replicas at the owner's
  /// head, the turn's changes sent to each replica's shard in one message, two in all, and one acknowledgement come home
  /// from each.
  #[test]
  fn a_turns_changes_go_to_each_replica_in_one_message_after_the_turns_replies() {
    let daemon = crate::daemon::audit_daemon_of(3);
    let (green, work, first) = crate::daemon::observe_first(&daemon, |state| {
      let (green, work) = green_and_work(state, "turned");
      submit_one(state, work, 1);
      (green, work, head_of(state, green))
    });
    assert!(
      both_hold(&daemon, green, first),
      "both replicas hold version 1"
    );
    let before = crate::daemon::observe_first(&daemon, sent_and_acknowledged);
    let (sent_before_replies, head) = crate::daemon::observe_first(&daemon, move |state| {
      let mut sent_before_replies = Vec::new();
      for byte in [2, 3, 4] {
        let [sent, _] = sent_and_acknowledged(state);
        submit_one(state, work, byte);
        let [sent_by_the_reply, _] = sent_and_acknowledged(state);
        sent_before_replies.push(sent_by_the_reply.saturating_sub(sent));
      }
      (sent_before_replies, head_of(state, green))
    });
    assert_eq!(
      sent_before_replies,
      [0, 0, 0],
      "no message of the feed sent before any of the turn's replies"
    );
    assert!(
      both_hold(&daemon, green, head),
      "both replicas at the owner's head"
    );
    let after = crate::daemon::observe_first(&daemon, sent_and_acknowledged);
    assert_eq!(
      [
        after[0].saturating_sub(before[0]),
        after[1].saturating_sub(before[1])
      ],
      [2, 2],
      "the turn's changes in one message to each replica's shard, one acknowledgement back from each"
    );
    daemon.stop();
    assert_eq!(
      slates_mem::sealed::dropped_with_readers(),
      0,
      "no owner dropped with a replica registered"
    );
  }
}
