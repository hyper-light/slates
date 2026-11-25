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
//! **The spawner's death.** Its end reads closed: every slot is answered [`SpawnRefused::SpawnerGone`], and a spawn
//! asked after is refused so. Its children's groups are the asker's to kill (`Spawned::kill_group`): a spawner that
//! died rather than ended has killed none of them.

use std::cell::RefCell;
use std::collections::{BTreeMap, VecDeque};
use std::future::Future;
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::pin::Pin;
use std::process::ExitStatus;
use std::task::{Context, Poll, Waker};

use slates_ipc::confined_spawn::Spawn;
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

/// Installs `end` as this shard's end of the spawner and starts its conversation task, which lives as long as the
/// shard's runtime or until the spawner's end closes.
pub(crate) fn install(end: OwnedFd) -> Result<(), slates_rt::RtError> {
  CONVERSATION.with(|conversation| {
    if let Ok(mut conversation) = conversation.try_borrow_mut() {
      *conversation = Some(Conversation {
        end,
        inbound: Inbound::default(),
        outbound: Outbound::default(),
        queue: VecDeque::new(),
        in_flight: None,
        slots: BTreeMap::new(),
        next_id: 1,
        gone: false,
        kicked: false,
        task: None,
      });
    }
  });
  futures::spawn(converse()).and_then(futures::detach)
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

  /// The spawner's end closed: every waiter woken to find it gone, nothing more asked of it.
  fn end_gone(&mut self) {
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
  let id = with(|conversation| {
    if conversation.gone {
      return Err(SpawnRefused::SpawnerGone);
    }
    let id = conversation.next_id;
    conversation.next_id = id.saturating_add(1);
    let bytes =
      wire::frame(FrameKind::Spawn, id, &body).map_err(|_| SpawnRefused::Unrequestable)?;
    conversation.slots.insert(id, Slot::default());
    conversation.queue.push_back((id, bytes, descriptors));
    conversation.kick();
    Ok(id)
  })
  .ok_or(SpawnRefused::NoSpawner)??;
  Answered { id, done: false }.await
}

/// Ready once spawn `id` is answered: the child, or why not.
struct Answered {
  id: u64,
  done: bool,
}

impl Future for Answered {
  type Output = Result<Spawned, SpawnRefused>;

  fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
    let id = self.id;
    let answered = with(|conversation| match conversation.slots.get_mut(&id) {
      Some(Slot {
        spawned: Some(Ok(pid)),
        ..
      }) => Some(Ok(Spawned { id, pid: *pid })),
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
    let id = self.id;
    let _ = with(|conversation| {
      if let Some(slot) = conversation.slots.get_mut(&id) {
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
    let id = self.id;
    with(|conversation| match conversation.slots.get(&id) {
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
    let id = self.id;
    let _ = with(|conversation| {
      conversation.slots.remove(&id);
      if !conversation.gone {
        conversation.release(id);
        conversation.kick();
      }
    });
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

/// A spawner a test's own runtime starts and installs on its shard (a daemon's shards get theirs from its start): this
/// test binary in its role (`slates_ipc::spawner_role!` at the crate's root), its end installed on the current shard.
/// Dropped, it takes the end back and joins the spawner, which exits once its end has closed.
#[cfg(test)]
pub(crate) struct TestSpawner {
  child: slates_ipc::confined_spawn::SpawnedChild,
}

#[cfg(test)]
impl TestSpawner {
  /// Starts a spawner and installs its end on the current shard; call it on the shard's thread, inside a task.
  pub(crate) fn installed() -> TestSpawner {
    let started = slates_ipc::spawner::start_here(1).expect("a spawner started");
    let slates_ipc::spawner::Started { child, ends } = started;
    for end in ends {
      install(end).expect("the spawner's end installed");
    }
    TestSpawner { child }
  }

  /// The spawner's pid.
  pub(crate) fn pid(&self) -> u32 {
    self.child.id()
  }

  /// Kills the spawner (not its children): what its death looks like to the shard.
  pub(crate) fn kill(&self) {
    if let Some(pid) = i32::try_from(self.child.id())
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
    let _ = self.child.wait();
  }
}

#[cfg(test)]
mod tests;
