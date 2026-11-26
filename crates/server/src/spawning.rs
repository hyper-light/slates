//! A shard's conversation with the daemon's spawner (A-924 piece 2a; `slates_ipc::spawner`): how a hook or a helper is
//! started on this shard without the shard paying for the daemon's descriptor table.
//!
//! **One task owns the end.** The shard's runtime keeps one readiness registration a descriptor and direction (a later
//! one replaces an earlier, A-149), so no two tasks may wait on the end. The shard's conversation task ([`converse`])
//! alone reads and writes it; a spawn ([`spawn`]) and its exit ([`Spawned::exit`]) are slots in this thread's state the
//! task fills, each waking the task that waits on it. A request kicks the conversation task, which races the end's
//! readability, its writability while it owes bytes, and its kick.
//!
//! **One spawn in flight.** The conversation sends its next spawn request only once the last was answered, so the
//! descriptors the spawner reads belong to one request (`slates_ipc::spawner`'s conversation rule). Releases go at once.
//!
//! **The spawner's death, and its restart** (piece 2b). Its end reads closed: every slot of that generation is
//! answered [`SpawnRefused::SpawnerGone`] (its children's groups are the askers' to kill, `Spawned::kill_group`: a
//! spawner that died rather than ended has killed none of them), and the shard tells the spawner's owner, the control
//! shard ([`own`]). The owner starts a new one once per generation, however many shards tell it, after a back-off
//! ([`back_off_ns`]), and sends each shard its new end ([`reinstall`]), or tells each that the attempt failed and tries
//! again after a longer back-off. A spawn asked while a restart is awaited waits for that attempt's answer, which always
//! comes (the new end or the failure), then goes on or is refused typed; between attempts it is refused at once. Ids
//! keep rising across generations, and a handle of a gone generation answers `SpawnerGone`, so no handle of the dead
//! spawner's ever names a new one's child.
//!
//! **Bounded.** A shard's spawns in flight (asked, running, not yet let go) are at most its task bound: each of its
//! tasks asks for at most one child at a time. One past it is refused [`SpawnRefused::Busy`].

use std::cell::RefCell;
use std::collections::{BTreeMap, VecDeque};
use std::future::Future;
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::pin::Pin;
use std::process::ExitStatus;
use std::task::{Context, Poll, Waker};

use slates_ipc::confined_spawn::{Spawn, SpawnedChild};
use slates_ipc::spawner::{self as wire, Answer, FrameKind, Inbound, Outbound, Read};
use slates_rt::futures::{self, Either};

/// Why a spawn through the spawner was not answered with a child.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SpawnRefused {
  /// This shard holds no end of a spawner: a test binary that names no `spawner_role!`, or a daemon whose spawner never
  /// started.
  NoSpawner,
  /// The spawner's end closed: it ended or died (A-924's death rule).
  SpawnerGone,
  /// The request could not be made: an argument or a path holding a NUL, or past the frame's cap.
  Unrequestable,
  /// The spawner could not start it, or a stream's descriptor could not be duplicated: the operating system's error.
  Os(i32),
  /// This shard's spawns in flight are at its bound (the module doc).
  Busy,
}

/// Where a shard's conversation sits: its spawner's generation, the shard that owns the spawner, and its bound.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Placement {
  /// The spawner's generation: zero at the daemon's start, one more at each restart.
  pub(crate) generation: u64,
  /// The shard that owns the spawner ([`own`]): the control shard.
  pub(crate) owner: u16,
  /// The most spawns in flight on this shard (the module doc).
  pub(crate) bound: usize,
}

/// What this thread's conversation knows of one spawn.
#[derive(Default)]
struct Slot {
  /// Its answer: the pid, or the error.
  spawned: Option<Result<u32, i32>>,
  /// Its exit, once answered.
  exited: Option<ExitStatus>,
  /// The task waiting on it.
  waker: Option<Waker>,
  /// Its asker went away before it was answered: a child started for it is killed and let go.
  abandoned: bool,
}

/// This shard's conversation with the spawner.
struct Conversation {
  end: OwnedFd,
  inbound: Inbound,
  outbound: Outbound,
  /// Spawn requests waiting their turn, each with the descriptors that ride it.
  queue: VecDeque<(u64, Vec<u8>, Vec<OwnedFd>)>,
  /// The spawn request sent and not yet answered.
  in_flight: Option<u64>,
  slots: BTreeMap<u64, Slot>,
  next_id: u64,
  gone: bool,
  kicked: bool,
  task: Option<Waker>,
  placement: Placement,
  /// Its end closed and the owner not yet told ([`report_gone`], made outside this state's borrow).
  report_owed: bool,
  /// Its end closed and the owner's restart not yet answered: a spawn waits for it.
  restart_awaited: bool,
  /// The tasks waiting for the restart's answer.
  restart_waiters: Vec<Waker>,
}

thread_local! {
  /// This shard's conversation, once its end is installed ([`install`]).
  static CONVERSATION: RefCell<Option<Conversation>> = const { RefCell::new(None) };
}

/// Runs `work` on this shard's conversation; none where there is none (or it is lent already).
fn with<T>(work: impl FnOnce(&mut Conversation) -> T) -> Option<T> {
  CONVERSATION.with(|conversation| {
    let mut conversation = conversation.try_borrow_mut().ok()?;
    conversation.as_mut().map(work)
  })
}

/// Installs `end` as this shard's end of the spawner, at `placement`, and starts its conversation task, which lives as
/// long as the shard's runtime or until the spawner's end closes. A conversation it replaces hands on its next id, so
/// ids keep rising across generations, and its waiters for the restart are woken.
pub(crate) fn install(end: OwnedFd, placement: Placement) -> Result<(), slates_rt::RtError> {
  let waiters = CONVERSATION.with(|conversation| {
    let Ok(mut conversation) = conversation.try_borrow_mut() else {
      return Vec::new();
    };
    let (next_id, waiters) = conversation
      .take()
      .map_or((1, Vec::new()), |old| (old.next_id, old.restart_waiters));
    *conversation = Some(Conversation {
      end,
      inbound: Inbound::default(),
      outbound: Outbound::default(),
      queue: VecDeque::new(),
      in_flight: None,
      slots: BTreeMap::new(),
      next_id,
      gone: false,
      kicked: false,
      task: None,
      placement,
      report_owed: false,
      restart_awaited: false,
      restart_waiters: Vec::new(),
    });
    waiters
  });
  waiters.into_iter().for_each(Waker::wake);
  futures::spawn(converse()).and_then(futures::detach)
}

/// Installs a restarted spawner's `end` of `generation` here ([`install`]), at this shard's placement otherwise.
fn reinstall(end: OwnedFd, generation: u64) {
  let Some(placement) = with(|conversation| conversation.placement) else {
    return;
  };
  if install(
    end,
    Placement {
      generation,
      ..placement
    },
  )
  .is_err()
  {
    restart_failed();
  }
}

/// The owner's restart failed: this shard's waiters are woken to find the spawner still gone.
fn restart_failed() {
  let waiters = with(|conversation| {
    conversation.restart_awaited = false;
    std::mem::take(&mut conversation.restart_waiters)
  })
  .unwrap_or_default();
  waiters.into_iter().for_each(Waker::wake);
}

/// Tells the spawner's owner that this shard's end of it closed, if it has not been told: on its own shard at once, else
/// by a message there; one it cannot be sent answers this shard's waiters as a failed restart.
fn report_gone() {
  let owed = with(|conversation| {
    std::mem::take(&mut conversation.report_owed).then_some(conversation.placement)
  })
  .flatten();
  let Some(placement) = owed else {
    return;
  };
  let here = futures::shard_id().map(|shard| shard.0);
  if here == Some(placement.owner) {
    spawner_gone(placement.generation);
    return;
  }
  let generation = placement.generation;
  match here {
    Some(here) => crate::xshard::run_on_waiting(
      here,
      placement.owner,
      move |_| spawner_gone(generation),
      SPAWNER_REPORT_REFUSED,
    ),
    None => restart_failed(),
  }
}

/// Counter: a shard's report of its spawner's end closing that could not reach the owner's shard (A-924).
const SPAWNER_REPORT_REFUSED: &str = "spawner.report_refused";
/// Counter: the spawners the owner started again after a death (A-924): none while the spawner serves. A test binary that
/// names no `spawner_role!()`, or a host whose `main` does not serve first, starts a spawner that ends at once, which
/// this counts once a window.
pub(crate) const SPAWNER_RESTARTED: &str = "spawner.restarted";

/// Counter: a dead spawner whose reaping task the shard could not start: left a zombie until the daemon's process ends
/// (A-924).
const SPAWNER_REAP_REFUSED: &str = "spawner.reap_refused";

/// Counts `counter` on this shard, when there is one.
fn count_on_shard(counter: &'static str) {
  let _ = crate::state::with_state_counted(|state| state.count(counter, 1));
}

/// The conversation task (the module doc): steps the conversation, then waits for the end or a kick.
async fn converse() {
  loop {
    let waits = with(|conversation| {
      conversation.step();
      (!conversation.gone).then(|| {
        (
          conversation.end.as_raw_fd(),
          !conversation.outbound.is_empty(),
        )
      })
    })
    .flatten();
    report_gone();
    let Some((raw, owes)) = waits else {
      return;
    };
    let writable = async move {
      if owes {
        slates_rt::readiness::writable(raw).await
      } else {
        std::future::pending().await
      }
    };
    let woke = futures::race2(
      slates_rt::readiness::readable(raw),
      futures::race2(writable, Kicked),
    )
    .await;
    if matches!(
      woke,
      Either::First(Err(_)) | Either::Second(Either::First(Err(_)))
    ) {
      let _ = with(Conversation::end_gone);
      report_gone();
      return;
    }
  }
}

/// Ready once the conversation was kicked (a request or a release queued), the kick taken.
struct Kicked;

impl Future for Kicked {
  type Output = ();

  fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
    let kicked = with(|conversation| {
      if conversation.kicked {
        conversation.kicked = false;
        return true;
      }
      conversation.task = Some(cx.waker().clone());
      false
    });
    match kicked {
      Some(false) => Poll::Pending,
      Some(true) | None => Poll::Ready(()),
    }
  }
}

impl Conversation {
  /// Wakes the conversation task.
  fn kick(&mut self) {
    self.kicked = true;
    if let Some(task) = self.task.take() {
      task.wake();
    }
  }

  /// Sends what is owed and the next spawn in its turn, reads what arrived and answers its slots.
  fn step(&mut self) {
    while !self.gone {
      if self.in_flight.is_none()
        && let Some((id, bytes, descriptors)) = self.queue.pop_front()
      {
        self.outbound.owe(&bytes, descriptors);
        self.in_flight = Some(id);
      }
      if self.outbound.flush(self.end.as_fd()).is_err() {
        self.end_gone();
        return;
      }
      match self.inbound.read(self.end.as_fd()) {
        Ok(Read::Bytes) => self.take_answers(),
        Ok(Read::Waiting) => return,
        Ok(Read::Closed | Read::Malformed) | Err(_) => self.end_gone(),
      }
    }
  }

  /// Answers the slots of every whole frame read.
  fn take_answers(&mut self) {
    while let Some(frame) = self.inbound.next_frame() {
      let Some(answer) = wire::answer_of(&frame) else {
        continue;
      };
      if matches!(answer, Answer::Spawned(_) | Answer::Refused(_))
        && self.in_flight == Some(frame.id)
      {
        self.in_flight = None;
      }
      self.answer(frame.id, answer);
    }
  }

  /// Gives spawn `id` its `answer`, waking its waiter; a child started for an asker that went away is killed and let go.
  fn answer(&mut self, id: u64, answer: Answer) {
    let Some(slot) = self.slots.get_mut(&id) else {
      return;
    };
    match answer {
      Answer::Spawned(pid) => slot.spawned = Some(Ok(pid)),
      Answer::Refused(number) => slot.spawned = Some(Err(number)),
      Answer::Exited(status) => slot.exited = Some(status),
    }
    if slot.abandoned {
      if let Some(Ok(pid)) = slot.spawned {
        kill_group(pid);
      }
      self.slots.remove(&id);
      self.release(id);
      return;
    }
    if let Some(waker) = slot.waker.take() {
      waker.wake();
    }
  }

  /// Owes a release of spawn `id`.
  fn release(&mut self, id: u64) {
    if let Ok(bytes) = wire::frame(FrameKind::Release, id, &[]) {
      self.outbound.owe(&bytes, Vec::new());
    }
  }

  /// The spawner's end closed: every waiter woken to find it gone, nothing more asked of it, and the owner owed word of
  /// it (a restart awaited).
  fn end_gone(&mut self) {
    if !self.gone {
      self.report_owed = true;
      self.restart_awaited = true;
    }
    self.gone = true;
    self.queue.clear();
    for slot in self.slots.values_mut() {
      if let Some(waker) = slot.waker.take() {
        waker.wake();
      }
    }
  }
}

/// Sends `SIGKILL` to the group `pid` leads.
fn kill_group(pid: u32) {
  if let Some(pid) = i32::try_from(pid)
    .ok()
    .and_then(rustix::process::Pid::from_raw)
  {
    let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
  }
}

/// Starts `spawn` through this shard's spawner: the child, once the spawner answered.
pub(crate) async fn spawn(spawn: &Spawn<'_>) -> Result<Spawned, SpawnRefused> {
  let body = wire::spawn_body(spawn).map_err(|_| SpawnRefused::Unrequestable)?;
  if body.len() > wire::most_body_bytes() {
    return Err(SpawnRefused::Unrequestable);
  }
  let descriptors = wire::spawn_descriptors(spawn)
    .into_iter()
    .map(|fd| {
      fd.try_clone_to_owned()
        .map_err(|error| SpawnRefused::Os(error.raw_os_error().unwrap_or(0)))
    })
    .collect::<Result<Vec<OwnedFd>, SpawnRefused>>()?;
  if with(|conversation| conversation.gone && conversation.restart_awaited) == Some(true) {
    Restarted.await;
  }
  let (id, generation) = with(|conversation| asked(conversation, &body, descriptors))
    .ok_or(SpawnRefused::NoSpawner)??;
  Answered {
    id,
    generation,
    done: false,
  }
  .await
}

/// Queues a spawn of `body` on `conversation`, `descriptors` riding it: its id and generation, or why not.
fn asked(
  conversation: &mut Conversation,
  body: &[u8],
  descriptors: Vec<OwnedFd>,
) -> Result<(u64, u64), SpawnRefused> {
  if conversation.gone {
    return Err(SpawnRefused::SpawnerGone);
  }
  if conversation.slots.len() >= conversation.placement.bound {
    return Err(SpawnRefused::Busy);
  }
  let id = conversation.next_id;
  conversation.next_id = id.saturating_add(1);
  let bytes = wire::frame(FrameKind::Spawn, id, body).map_err(|_| SpawnRefused::Unrequestable)?;
  conversation.slots.insert(id, Slot::default());
  conversation.queue.push_back((id, bytes, descriptors));
  conversation.kick();
  Ok((id, conversation.placement.generation))
}

/// Ready once no restart is awaited here: a new end installed, or the owner's attempt failed.
struct Restarted;

impl Future for Restarted {
  type Output = ();

  fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
    let waiting = with(|conversation| {
      let waiting = conversation.gone && conversation.restart_awaited;
      if waiting {
        conversation.restart_waiters.push(cx.waker().clone());
      }
      waiting
    });
    if waiting == Some(true) {
      Poll::Pending
    } else {
      Poll::Ready(())
    }
  }
}

/// Ready once spawn `id` of `generation` is answered: the child, or why not.
struct Answered {
  id: u64,
  generation: u64,
  done: bool,
}

impl Future for Answered {
  type Output = Result<Spawned, SpawnRefused>;

  fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
    let (id, generation) = (self.id, self.generation);
    let answered = with(|conversation| match conversation.slots.get_mut(&id) {
      _ if conversation.placement.generation != generation => Some(Err(SpawnRefused::SpawnerGone)),
      Some(Slot {
        spawned: Some(Ok(pid)),
        ..
      }) => Some(Ok(Spawned {
        id,
        generation,
        pid: *pid,
      })),
      Some(Slot {
        spawned: Some(Err(number)),
        ..
      }) => {
        let number = *number;
        conversation.slots.remove(&id);
        Some(Err(SpawnRefused::Os(number)))
      }
      _ if conversation.gone => {
        conversation.slots.remove(&id);
        Some(Err(SpawnRefused::SpawnerGone))
      }
      Some(slot) => {
        slot.waker = Some(cx.waker().clone());
        None
      }
      None => Some(Err(SpawnRefused::SpawnerGone)),
    })
    .unwrap_or(Some(Err(SpawnRefused::NoSpawner)));
    match answered {
      Some(answer) => {
        self.done = true;
        Poll::Ready(answer)
      }
      None => Poll::Pending,
    }
  }
}

impl Drop for Answered {
  /// An asker gone before its answer: a child the spawner starts for it is killed and let go when its answer comes.
  fn drop(&mut self) {
    if self.done {
      return;
    }
    let (id, generation) = (self.id, self.generation);
    let _ = with(|conversation| {
      if conversation.placement.generation == generation
        && let Some(slot) = conversation.slots.get_mut(&id)
      {
        slot.abandoned = true;
        slot.waker = None;
      }
    });
  }
}

/// A child the spawner started for this shard: its pid and group, and its exit as the spawner answers it. Dropped, it
/// is let go: the spawner reaps it once it has exited (its owner kills its group first if it must not run on).
#[derive(Debug)]
pub(crate) struct Spawned {
  id: u64,
  generation: u64,
  pid: u32,
}

impl Spawned {
  /// Sends `SIGKILL` to the group it leads: it and what it started that stayed in its group.
  pub(crate) fn kill_group(&self) {
    kill_group(self.pid);
  }

  /// Whether a process other than it is still in its group, it being its group's leader (`slates_ipc::group_holders`).
  pub(crate) fn group_has_others(&self) -> bool {
    slates_ipc::group_holders::group_has_others(self.pid)
  }

  /// Its exit, if the spawner has answered it now; the spawner's death answered as [`SpawnRefused::SpawnerGone`].
  pub(crate) fn exited(&self) -> Option<Result<ExitStatus, SpawnRefused>> {
    let (id, generation) = (self.id, self.generation);
    with(|conversation| match conversation.slots.get(&id) {
      _ if conversation.placement.generation != generation => Some(Err(SpawnRefused::SpawnerGone)),
      Some(Slot {
        exited: Some(status),
        ..
      }) => Some(Ok(*status)),
      _ if conversation.gone => Some(Err(SpawnRefused::SpawnerGone)),
      _ => None,
    })
    .unwrap_or(Some(Err(SpawnRefused::NoSpawner)))
  }

  /// Ready once it has exited (or the spawner has gone): [`Spawned::exited`] waited for.
  pub(crate) fn exit(&self) -> Exit<'_> {
    Exit { spawned: self }
  }
}

/// [`Spawned::exit`]'s future.
pub(crate) struct Exit<'s> {
  spawned: &'s Spawned,
}

impl Future for Exit<'_> {
  type Output = Result<ExitStatus, SpawnRefused>;

  fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
    if let Some(exited) = self.spawned.exited() {
      return Poll::Ready(exited);
    }
    let id = self.spawned.id;
    let _ = with(|conversation| {
      if let Some(slot) = conversation.slots.get_mut(&id) {
        slot.waker = Some(cx.waker().clone());
      }
    });
    Poll::Pending
  }
}

impl Drop for Spawned {
  /// Lets it go: its slot forgotten, the spawner told to reap it once it has exited.
  fn drop(&mut self) {
    let (id, generation) = (self.id, self.generation);
    let _ = with(|conversation| {
      if conversation.placement.generation != generation {
        return;
      }
      conversation.slots.remove(&id);
      if !conversation.gone {
        conversation.release(id);
        conversation.kick();
      }
    });
  }
}

/// The spawner's owner, on the control shard: the process, its generation, the shards it serves, and what its back-off
/// is drawn from.
struct Owner {
  /// The spawner running now; none while a restart replaces it.
  child: Option<SpawnedChild>,
  /// Where the owner hands its spawner at its drop, to be joined on the daemon's process thread once the shards have
  /// ended (`Daemon::stop_parts`), never on a shard's; none for a test's runtime, which joins it itself.
  join_to: Option<std::sync::mpsc::Sender<SpawnedChild>>,
  generation: u64,
  /// The shards an end each goes to, in the order the spawner's ends are made.
  shards: Vec<u16>,
  /// When the spawner now running started, on the shard's clock.
  started_at_ns: u64,
  /// The spawners that died within a liveness window of their start, in a row.
  deaths_in_a_row: u32,
  /// A restart is under way: further word of the same death is not acted on again.
  restarting: bool,
}

thread_local! {
  /// The spawner's owner, on the shard that owns it ([`own`]).
  static OWNER: RefCell<Option<Owner>> = const { RefCell::new(None) };
}

/// Runs `work` on this shard's owner of the spawner; none where it owns none.
fn with_owner<T>(work: impl FnOnce(&mut Owner) -> T) -> Option<T> {
  OWNER.with(|owner| owner.try_borrow_mut().ok()?.as_mut().map(work))
}

/// Makes this shard the spawner's owner: `child`, serving `shards` an end each in order, handed to `join_to` at the
/// owner's drop. The control shard calls it at its start (the daemon's
/// start made the spawner).
pub(crate) fn own(
  child: SpawnedChild,
  shards: Vec<u16>,
  join_to: Option<std::sync::mpsc::Sender<SpawnedChild>>,
) {
  OWNER.with(|owner| {
    if let Ok(mut owner) = owner.try_borrow_mut() {
      *owner = Some(Owner {
        child: Some(child),
        join_to,
        generation: 0,
        shards,
        started_at_ns: futures::now_ns(),
        deaths_in_a_row: 0,
        restarting: false,
      });
    }
  });
}

/// Derived: how long the owner waits before starting a spawner again after `deaths_in_a_row` deaths that each came
/// within a liveness window of its start: none after one (a spawner that ran its window and then died is restarted at
/// once), and a whole liveness window after any more, so a spawner that cannot stay up is started at most twice a
/// window. Until 2026-11-25 the wait doubled a start's measured length, a few milliseconds, and a spawner that ended at
/// once was started 12 times in a window and a half before the doubling reached the window (main's question on A-924
/// (c)'s red).
pub(crate) fn back_off_ns(deaths_in_a_row: u32) -> u64 {
  if deaths_in_a_row <= 1 {
    0
  } else {
    crate::daemon::LIVENESS_BUDGET_NS
  }
}

/// Word on the owner's shard that a shard's end of the spawner of `generation` closed: a restart begun once for it, or
/// where this shard owns no spawner, the reporting shard's restart answered failed (a test's runtime).
fn spawner_gone(generation: u64) {
  let begun = with_owner(|owner| {
    if owner.generation != generation || owner.restarting {
      return false;
    }
    owner.restarting = true;
    let lived_ns = futures::now_ns().saturating_sub(owner.started_at_ns);
    owner.deaths_in_a_row = if lived_ns > crate::daemon::LIVENESS_BUDGET_NS {
      1
    } else {
      owner.deaths_in_a_row.saturating_add(1)
    };
    true
  });
  match begun {
    None => restart_failed(),
    Some(false) => {}
    Some(true) => {
      if futures::spawn(restart()).and_then(futures::detach).is_err() {
        let _ = with_owner(|owner| owner.restarting = false);
        tell_every_shard_failed();
      }
    }
  }
}

/// The owner's restart (the module doc): after the back-off, the dead spawner reaped and a new one started, each shard
/// sent its new end; a start refused tells every shard so and tries again after a longer back-off.
async fn restart() {
  loop {
    let wait_ns = with_owner(|owner| back_off_ns(owner.deaths_in_a_row)).unwrap_or(0);
    if wait_ns > 0 {
      let _ = futures::sleep(wait_ns).await;
    }
    let Some((dead, shards)) = with_owner(|owner| (owner.child.take(), owner.shards.clone()))
    else {
      return;
    };
    // Reaped by a task of its own, so a dead spawner slow to end (outside the group its kill reaches) delays no new one.
    if let Some(dead) = dead
      && let Err(_refused) = futures::spawn(reaped(dead)).and_then(futures::detach)
    {
      count_on_shard(SPAWNER_REAP_REFUSED);
    }
    match started_off_shard(shards.len()).await {
      Ok(started) => {
        hand_out(started, &shards);
        count_on_shard(SPAWNER_RESTARTED);
        return;
      }
      Err(_) => {
        tell_every_shard_failed();
        let _ = with_owner(|owner| owner.deaths_in_a_row = owner.deaths_in_a_row.saturating_add(1));
      }
    }
  }
}

/// A spawner started with `ends` ends by a thread of its own, the shard awaiting it: a `posix_spawn` of the daemon's
/// executable under load held the control shard past its window (main's store-wait at 1948dbdcc: "shard 0 is wedged,
/// unmoved for 1028 ms … spawning.rs:687"). The thread hands the spawner back and writes a byte to a pipe whose read
/// end the shard awaits, and is joined once it has answered. The start's length is counted
/// ([`SPAWNER_START_NS_MOST`], and [`SPAWNER_STARTS_PAST_WINDOW`] where it passed the liveness window).
async fn started_off_shard(ends: usize) -> std::io::Result<slates_ipc::spawner::Started> {
  let (wake_read, wake_write) = wake_pipe()?;
  let (sent, got) = std::sync::mpsc::channel();
  #[cfg(test)]
  let delay_ns = TEST_START_DELAY_NS.with(std::cell::Cell::get);
  let starter = std::thread::Builder::new()
    .name("slates-spawner-start".to_owned())
    .spawn(move || {
      let began = std::time::Instant::now();
      #[cfg(test)]
      std::thread::park_timeout(std::time::Duration::from_nanos(delay_ns));
      let started = slates_ipc::spawner::start_here(ends);
      let took = u64::try_from(began.elapsed().as_nanos()).unwrap_or(u64::MAX);
      let _ = sent.send((started, took));
      let _ = rustix::io::write(&wake_write, &[1]);
    })?;
  loop {
    match got.try_recv() {
      Ok((started, took)) => {
        let _ = starter.join();
        count_start(took);
        return started;
      }
      Err(std::sync::mpsc::TryRecvError::Disconnected) => {
        let _ = starter.join();
        return Err(std::io::Error::from(std::io::ErrorKind::BrokenPipe));
      }
      Err(std::sync::mpsc::TryRecvError::Empty) => {}
    }
    if slates_rt::readiness::readable(wake_read.as_raw_fd())
      .await
      .is_err()
    {
      futures::yield_now().await;
    }
  }
}

/// A pipe for a thread to wake a shard's task by: both ends close-on-exec, the read end non-blocking.
fn wake_pipe() -> std::io::Result<(OwnedFd, OwnedFd)> {
  let (read, write) = rustix::pipe::pipe().map_err(std::io::Error::from)?;
  for end in [&read, &write] {
    rustix::io::fcntl_setfd(end, rustix::io::FdFlags::CLOEXEC).map_err(std::io::Error::from)?;
  }
  let flags = rustix::fs::fcntl_getfl(&read).map_err(std::io::Error::from)?;
  rustix::fs::fcntl_setfl(&read, flags | rustix::fs::OFlags::NONBLOCK)
    .map_err(std::io::Error::from)?;
  Ok((read, write))
}

/// Counts a spawner's start that took `took_ns`.
fn count_start(took_ns: u64) {
  let _ = crate::state::with_state_counted(|state| {
    state.count_max(SPAWNER_START_NS_MOST, took_ns);
    if took_ns > crate::daemon::LIVENESS_BUDGET_NS {
      state.count(SPAWNER_STARTS_PAST_WINDOW, 1);
    }
  });
}

/// Counted most: the longest start of a spawner the owner made, in nanoseconds (A-924). Format: a counter name.
pub(crate) const SPAWNER_START_NS_MOST: &str = "spawner.start_ns_most";
/// Counter: the spawner's starts that took longer than the liveness window, each of which would have held the control
/// shard past it on the shard (A-924). Format: a counter name.
pub(crate) const SPAWNER_STARTS_PAST_WINDOW: &str = "spawner.starts_past_window";

#[cfg(test)]
thread_local! {
  /// A test's delay before each spawner's start begun from this shard's thread: what a start slowed by load looks like.
  pub(crate) static TEST_START_DELAY_NS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// A spawner just started: kept as the owner's next generation, and an end sent to each of `shards`.
fn hand_out(started: slates_ipc::spawner::Started, shards: &[u16]) {
  let slates_ipc::spawner::Started { child, ends } = started;
  let now = futures::now_ns();
  let Some(generation) = with_owner(|owner| {
    owner.child = Some(child);
    owner.generation = owner.generation.saturating_add(1);
    owner.started_at_ns = now;
    owner.restarting = false;
    owner.generation
  }) else {
    return;
  };
  let here = futures::shard_id().map(|shard| shard.0);
  for (shard, end) in shards.iter().copied().zip(ends) {
    match here {
      Some(here) if here != shard => crate::xshard::run_on_waiting(
        here,
        shard,
        move |_| reinstall(end, generation),
        SPAWNER_REPORT_REFUSED,
      ),
      _ => reinstall(end, generation),
    }
  }
}

/// Every shard told the owner's restart failed, its waiters refused.
fn tell_every_shard_failed() {
  let shards = with_owner(|owner| owner.shards.clone()).unwrap_or_default();
  let here = futures::shard_id().map(|shard| shard.0);
  for shard in shards {
    match here {
      Some(here) if here != shard => {
        crate::xshard::run_on_waiting(here, shard, |_| restart_failed(), SPAWNER_REPORT_REFUSED)
      }
      _ => restart_failed(),
    }
  }
}

/// Reaps a spawner whose end closed, never waiting on the shard (a blocking wait here wedged a shard past its liveness
/// window under load, and its watchdog ended the daemon, `reap_map` on 2026-11-25): killed with its group first if it
/// still runs (an end it closed itself, a frame it refused), then its exit awaited on its exit watch, a status not yet
/// readable after the watch's note read again at the shard's next turn.
async fn reaped(mut child: SpawnedChild) {
  if matches!(child.try_wait(), Ok(Some(_))) {
    return;
  }
  child.kill_group();
  let watch = slates_ipc::exit_watch::ExitWatch::on(child.id()).ok();
  loop {
    if !matches!(child.try_wait(), Ok(None)) {
      return;
    }
    let readiness = watch
      .as_ref()
      .filter(|watch| !watch.exited())
      .and_then(|watch| watch.readiness().map(|fd| fd.as_raw_fd()));
    match readiness {
      Some(fd) => {
        if slates_rt::readiness::readable(fd).await.is_err() {
          futures::yield_now().await;
        }
      }
      None => futures::yield_now().await,
    }
  }
}

/// Joins `child`, a spawner whose ends have closed, off every shard: it kills what it still holds and exits; one still
/// running a liveness window later is killed with its group, then waited for, so a stop never waits on it without end.
pub(crate) fn join_spawner(mut child: SpawnedChild) {
  if matches!(child.try_wait(), Ok(Some(_))) {
    return;
  }
  if let Ok(watch) = slates_ipc::exit_watch::ExitWatch::on(child.id())
    && let Some(readiness) = watch.readiness()
    && let Ok(window) = rustix::event::Timespec::try_from(std::time::Duration::from_nanos(
      crate::daemon::LIVENESS_BUDGET_NS,
    ))
  {
    let mut fds = [rustix::event::PollFd::new(
      &readiness,
      rustix::event::PollFlags::IN,
    )];
    let _ = rustix::event::poll(&mut fds, Some(&window));
  }
  if !matches!(child.try_wait(), Ok(Some(_))) {
    child.kill_group();
    let _ = child.wait();
  }
}

impl Drop for Owner {
  /// The daemon's stop: the spawner handed to the daemon's process thread, which joins it once every shard's end has
  /// closed with its shard ([`join_spawner`]); a test's runtime, with no daemon, or a daemon's thread already gone, has
  /// it joined on a thread of its own ([`join_off_the_shard`]).
  fn drop(&mut self) {
    let Some(child) = self.child.take() else {
      return;
    };
    match self.join_to.as_ref() {
      Some(join_to) => {
        if let Err(unsent) = join_to.send(child) {
          join_off_the_shard(unsent.0);
        }
      }
      None => join_off_the_shard(child),
    }
  }
}

/// Joins `child` ([`join_spawner`]) on a thread of its own, never the shard's: the join may wait a liveness window, and
/// then the kill's `wait`, on a spawner the machine has not yet run (main 01a20182's 8a loop at 523c522c2: a test's
/// shard wedged just past its window in the join). Joined here only where no thread can be made.
fn join_off_the_shard(child: SpawnedChild) {
  let slot = std::sync::Arc::new(std::sync::Mutex::new(Some(child)));
  let theirs = std::sync::Arc::clone(&slot);
  let joiner = std::thread::Builder::new()
    .name("spawner-join".to_owned())
    .spawn(move || {
      if let Some(child) = theirs.lock().ok().and_then(|mut held| held.take()) {
        join_spawner(child);
      }
    });
  if joiner.is_err()
    && let Some(child) = slot.lock().ok().and_then(|mut held| held.take())
  {
    join_spawner(child);
  }
}

/// Takes this shard's conversation away, its end closed and its task woken to end: what a test's runtime does before
/// it joins the spawner it started ([`TestSpawner`]).
#[cfg(test)]
fn uninstall() {
  let taken = CONVERSATION.with(|conversation| conversation.try_borrow_mut().ok()?.take());
  if let Some(mut conversation) = taken
    && let Some(task) = conversation.task.take()
  {
    task.wake();
  }
}

/// Derived: the spawns in flight a test's runtime allows: its task bound (`tasks_per_shard: 8` in each test's runtime),
/// as a daemon's shard's spawns are bounded by its tasks (the module doc).
#[cfg(test)]
pub(crate) const TEST_SPAWNS_IN_FLIGHT: usize = 8;

/// A spawner a test's own runtime starts and installs on its shard (a daemon's shards get theirs from its start): this
/// test binary in its role (`slates_ipc::spawner_role!` at the crate's root), its end installed on the current shard.
/// Owned ([`TestSpawner::owned`]), the shard is its owner and restarts it on its death; otherwise its death refuses
/// every spawn after. Dropped, it takes the end back and joins the spawner, which exits once its end has closed, on a
/// thread of its own ([`join_off_the_shard`]), never the shard's.
#[cfg(test)]
pub(crate) struct TestSpawner {
  /// The spawner, where this shard does not own it.
  child: Option<SpawnedChild>,
}

#[cfg(test)]
impl TestSpawner {
  /// Starts a spawner and installs its end on the current shard, [`TEST_SPAWNS_IN_FLIGHT`] its bound; call it on the
  /// shard's thread, inside a task.
  pub(crate) fn installed() -> TestSpawner {
    Self::bounded(TEST_SPAWNS_IN_FLIGHT)
  }

  /// [`TestSpawner::installed`] with `bound` spawns in flight.
  pub(crate) fn bounded(bound: usize) -> TestSpawner {
    let (child, _) = Self::start(bound);
    TestSpawner { child: Some(child) }
  }

  /// [`TestSpawner::installed`], this shard the spawner's owner ([`own`]).
  pub(crate) fn owned() -> TestSpawner {
    let (child, here) = Self::start(TEST_SPAWNS_IN_FLIGHT);
    own(child, vec![here], None);
    TestSpawner { child: None }
  }

  /// A spawner started and its end installed here: the spawner and this shard's id.
  fn start(bound: usize) -> (SpawnedChild, u16) {
    let here = futures::shard_id().map_or(0, |shard| shard.0);
    let started = slates_ipc::spawner::start_here(1).expect("a spawner started");
    let slates_ipc::spawner::Started { child, ends } = started;
    for end in ends {
      let placement = Placement {
        generation: 0,
        owner: here,
        bound,
      };
      install(end, placement).expect("the spawner's end installed");
    }
    (child, here)
  }

  /// The running spawner's pid.
  pub(crate) fn pid(&self) -> u32 {
    match &self.child {
      Some(child) => child.id(),
      None => with_owner(|owner| owner.child.as_ref().map_or(0, SpawnedChild::id)).unwrap_or(0),
    }
  }

  /// Kills the running spawner (not its children): what its death looks like to the shard.
  pub(crate) fn kill(&self) {
    if let Some(pid) = i32::try_from(self.pid())
      .ok()
      .and_then(rustix::process::Pid::from_raw)
    {
      let _ = rustix::process::kill_process(pid, rustix::process::Signal::KILL);
    }
  }
}

#[cfg(test)]
impl Drop for TestSpawner {
  fn drop(&mut self) {
    uninstall();
    match self.child.take() {
      Some(child) => join_off_the_shard(child),
      None => {
        let _ = OWNER.with(|owner| owner.try_borrow_mut().ok().map(|mut owner| owner.take()));
      }
    }
  }
}

#[cfg(test)]
mod tests;
