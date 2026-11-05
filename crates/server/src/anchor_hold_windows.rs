//! The daemon's Windows mount holds (A-198, §4.6): the anchor owns duplicates of the control, drive-link and
//! ledger handles until an acknowledged release. One IOCP pipe lives on the control shard; exchanges borrow it by
//! move, and a bounded deadline covers both waiting for its owner and the request/acknowledgement pair. NFS socket
//! holds are still owed; those paired Unix API calls continue to report that no connection hold is available on Windows.
//!
//! **The channel outlives a missed deadline.** A deadline that passes ends an exchange, not the pipe: the pipe goes back
//! for the next exchange, whose read passes over any answer that is not its own, so a late acknowledgement can never
//! answer another mount. Until 2026-11-04 a passed deadline closed the daemon's only pipe, and every later hold and
//! release in that daemon's life was refused, which left each later unmount's drive held
//! (`docs/bugs/2026-11-04-a-refused-hold-release-left-a-drive-held.md`). The same record: the deadline was the clock's
//! reading plus the budget, given to waits that take a span, so it lasted the daemon's whole uptime and more.

use slates_anchor::handles::{self, HandleMessage, MESSAGE_BYTES, WHAT_WINFSP_MOUNT};
use slates_rt::futures;
use slates_rt::pipe::{PipeEnd, PipeRead, PipeWrite};
use slates_rt::tcp::{OwnedStream, TcpStream};
use std::cell::{Cell, RefCell};
use std::os::windows::io::{FromRawHandle, OwnedHandle};
use std::sync::OnceLock;

/// The shard owning the single request/acknowledgement stream. Set once at the daemon's start.
static OWNER: OnceLock<u16> = OnceLock::new();
thread_local! {
  /// One exchange at a time. Taking the pipe grants ownership across awaits without a lock or shared mutable borrow.
  static PIPE: RefCell<Option<PipeEnd>> = const { RefCell::new(None) };
  /// An exchange owns the pipe while this is true. Waiting exchanges yield under the shared recovery deadline.
  static EXCHANGING: Cell<bool> = const { Cell::new(false) };
}

/// Owns a single exchange through cancellation. However the exchange ends, answered, refused or dropped at its deadline,
/// the pipe goes back for the next one: it is the daemon's only channel to the anchor.
struct ExchangeLease {
  pipe: Option<PipeEnd>,
}

impl Drop for ExchangeLease {
  fn drop(&mut self) {
    let pipe = self.pipe.take();
    // `try_with`: a shard's end drops its tasks while its thread's locals are going away, when no exchange follows.
    let _ = PIPE.try_with(|slot| {
      if let Ok(mut slot) = slot.try_borrow_mut() {
        *slot = pipe;
      }
    });
    let _ = EXCHANGING.try_with(|busy| busy.set(false));
  }
}

/// Derived: the pause between looks at a pipe another exchange owns: one tick of the daemon's pacing, the heartbeat over
/// the fleet's polls a period, far under any exchange's budget.
const ACQUIRE_PACE_NS: u64 = crate::daemon::HEARTBEAT_NS / crate::fleet::POLL_PER_PERIOD;

async fn acquire() -> Result<ExchangeLease, HoldRefusal> {
  loop {
    let pipe = PIPE.with(|slot| {
      slot
        .try_borrow_mut()
        .map(|mut slot| slot.take())
        .map_err(|_| HoldRefusal::Unavailable)
    })?;
    if let Some(pipe) = pipe {
      EXCHANGING.with(|busy| busy.set(true));
      return Ok(ExchangeLease { pipe: Some(pipe) });
    }
    if !EXCHANGING.with(Cell::get) {
      return Err(HoldRefusal::NoChannel);
    }
    // This is the cold mount-lifetime path, and the exchange holding the pipe may be waiting out an anchor that does
    // not answer, so wait a tick instead of spinning the control shard. The enclosing `within` bounds acquisition and
    // I/O together.
    futures::sleep(ACQUIRE_PACE_NS)
      .await
      .map_err(HoldRefusal::Runtime)?;
  }
}

/// Why the mount hold could not complete within its checked protocol and recovery budget.
#[derive(Debug)]
pub(crate) enum HoldRefusal {
  /// This daemon has no channel to an anchor, so the anchor holds nothing of its.
  NoChannel,
  /// The channel could not be used for this exchange.
  Unavailable,
  /// The exchange reached its budget without the anchor's answer.
  Deadline,
  /// A checked hold-channel message was refused.
  Protocol(handles::HandleRefusal),
  /// The IOCP driver refused the pipe or its I/O.
  Runtime(slates_rt::RtError),
}

impl std::fmt::Display for HoldRefusal {
  fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      Self::NoChannel => out.write_str("this daemon has no anchor to hold a Windows mount"),
      Self::Unavailable => out.write_str("the Windows mount hold channel is unavailable"),
      Self::Deadline => out.write_str("the anchor did not answer within the exchange's budget"),
      Self::Protocol(error) => write!(out, "{error}"),
      Self::Runtime(error) => write!(out, "{error}"),
    }
  }
}

/// A mount the anchor handed on: its raw objects are adopted before another operation can reuse their values.
#[derive(Debug)]
pub(crate) struct HandedMount {
  pub(crate) attachment: u64,
  pub(crate) handles: Vec<OwnedHandle>,
  pub(crate) record: Vec<u8>,
  pub(crate) adoptable: bool,
}

/// A connection the anchor handed back: its id and the socket. None is handed back on Windows.
#[derive(Debug)]
pub(crate) struct HandedConnection {
  /// The connection's id, kept for its life.
  pub(crate) connection: u64,
  /// The connected socket.
  pub(crate) socket: OwnedStream,
}

/// The inherited mount objects and the fresh control channel; Windows NFS connections are still owed.
#[derive(Debug, Default)]
pub(crate) struct Handed {
  /// The held NFS connections.
  pub(crate) connections: Vec<HandedConnection>,
  pub(crate) mounts: Vec<HandedMount>,
  pub(crate) channel: Option<OwnedHandle>,
}

/// Adopts the explicitly inherited channel and held mounts exactly once, before handle values can be reused.
pub(crate) fn adopt(bound: usize) -> Handed {
  let mut handed = Handed::default();
  let Ok(value) = std::env::var(handles::ENV_HANDLES) else {
    return handed;
  };
  // This bound is also the parser's allocation cap, derived from the admitted mount count at boot. The environment
  // itself has Windows' fixed process-environment cap; no peer-controlled allocation precedes parse_env's checks.
  let (channel, entries) = match handles::parse_env(&value, bound) {
    Ok(parsed) => parsed,
    Err(error) => {
      eprintln!("slates-server: Windows hold handoff refused: {error}");
      return handed;
    }
  };
  handed.channel = match adopted(channel) {
    Ok(channel) => Some(channel),
    Err(error) => {
      eprintln!("slates-server: Windows hold channel refused: {error}");
      None
    }
  };
  for entry in entries {
    let mut owned = Vec::new();
    let mut refused = false;
    for value in entry.values {
      match adopted(value) {
        Ok(handle) => owned.push(handle),
        Err(error) => {
          eprintln!("slates-server: Windows held handle refused: {error}");
          refused = true;
        }
      }
    }
    let adoptable =
      !refused && entry.what == WHAT_WINFSP_MOUNT && owned.len() == handles::MAX_HANDLES;
    if !adoptable {
      eprintln!(
        "slates-server: held entry {} has an unsupported or unusable mount shape",
        entry.id
      );
    }
    // Keep even a refused entry's id until the control shard releases the anchor's copies. Dropping only this
    // process's partial adoption would leave a bounded but uncollectable hold in every later successor.
    handed.mounts.push(HandedMount {
      attachment: entry.id,
      handles: owned,
      record: entry.record,
      adoptable,
    });
  }
  handed
}

fn adopted(value: u64) -> Result<OwnedHandle, HoldRefusal> {
  let value = usize::try_from(value).map_err(|_| HoldRefusal::Unavailable)?;
  if value == 0 || value == usize::MAX {
    return Err(HoldRefusal::Unavailable);
  }
  // SAFETY: parse_env checked distinct ownership for every inherited value, and adoption runs once at start.
  let handle =
    unsafe { OwnedHandle::from_raw_handle(std::ptr::with_exposed_provenance_mut(value)) };
  slates_ipc::win_spawn::clear_inheritance(&handle)
    .map_err(|error| HoldRefusal::Protocol(handles::HandleRefusal::Ipc(error)))?;
  Ok(handle)
}

/// Publishes the control shard before any initialization task can ask it for a mount hold.
pub(crate) fn set_owner(owner: u16) -> Result<(), handles::HandleRefusal> {
  OWNER
    .set(owner)
    .map_err(|_| handles::HandleRefusal::Malformed {
      reason: "the Windows hold owner was configured twice",
    })
}

impl HoldRefusal {
  pub(crate) fn into_server(self) -> crate::error::ServerError {
    match self {
      Self::Runtime(error) => crate::error::ServerError::Runtime(error),
      Self::Protocol(error) => {
        crate::error::ServerError::Anchor(slates_anchor::AnchorError::Hold(error))
      }
      Self::Deadline => crate::error::ServerError::Anchor(slates_anchor::AnchorError::Hold(
        handles::HandleRefusal::TimedOut,
      )),
      Self::NoChannel | Self::Unavailable => crate::error::ServerError::Anchor(
        slates_anchor::AnchorError::Hold(handles::HandleRefusal::Closed),
      ),
    }
  }
}

/// Installs the daemon's only IOCP hold pipe on its control shard, before restored mounts start their tasks.
pub(crate) fn install(channel: Option<OwnedHandle>, owner: u16) -> Result<(), HoldRefusal> {
  let Some(channel) = channel else {
    return Ok(());
  };
  if OWNER.get() != Some(&owner) {
    return Err(HoldRefusal::Unavailable);
  }
  let pipe = PipeEnd::adopt(channel, MESSAGE_BYTES, MESSAGE_BYTES).map_err(HoldRefusal::Runtime)?;
  PIPE.with(|slot| {
    *slot
      .try_borrow_mut()
      .map_err(|_| HoldRefusal::Unavailable)? = Some(pipe);
    Ok(())
  })
}

/// Derived: how long a mount waits for the anchor to take its hold: the recovery budget a restart is bounded by, plus
/// the liveness budget, which covers the anchor's own wait between looks at the channel (at most half of it,
/// `lapse_wait` in `crates/cli/src/anchor.rs`). The same span a client gives a restarting daemon (`Deadlines::derive`).
const HOLD_BUDGET_NS: u64 =
  slates_db::replay::RECOVERY_BUDGET_NS.saturating_add(crate::daemon::LIVENESS_BUDGET_NS);
/// Derived: one attempt at a release: the liveness budget, twice the anchor's longest wait between looks at the channel,
/// so a live anchor answers the first attempt. Two attempts make [`RELEASE_BOUND_NS`].
const RELEASE_ATTEMPT_NS: u64 = crate::daemon::LIVENESS_BUDGET_NS;
/// Derived: how long a detach retries the release before it answers a refusal: the same span as [`HOLD_BUDGET_NS`].
pub(crate) const RELEASE_BOUND_NS: u64 = HOLD_BUDGET_NS;

/// Holds a mount before its attachment commits. Exactly one bounded exchange owns the channel at a time.
pub(crate) async fn hold_mount(
  id: u64,
  values: Vec<u64>,
  record: Vec<u8>,
) -> Result<(), HoldRefusal> {
  exchange(
    HandleMessage::Hold {
      id,
      what: WHAT_WINFSP_MOUNT,
      values,
      record,
    },
    HOLD_BUDGET_NS,
  )
  .await
}

/// One attempt to close the anchor's copies before the file system is deleted. A release is idempotent at the anchor (a
/// second finds nothing and is answered the same), so the caller repeats it until it is answered; a refusal is returned
/// for the caller to count.
pub(crate) async fn release_mount(id: u64) -> Result<(), HoldRefusal> {
  exchange(HandleMessage::Release { id }, RELEASE_ATTEMPT_NS).await
}

async fn exchange(message: HandleMessage, budget_ns: u64) -> Result<(), HoldRefusal> {
  let owner = *OWNER.get().ok_or(HoldRefusal::NoChannel)?;
  let origin = crate::state::with_state(|s| s.shard).ok_or(HoldRefusal::Unavailable)?;
  crate::xshard::task_within(
    origin,
    owner,
    move || async move {
      // Waiting and transacting share one derived bound. The lease gives the channel back however this ends, and a
      // later exchange's read passes over whatever this one left unread.
      futures::within(budget_ns, async move {
        let lease = acquire().await?;
        let pipe = lease.pipe.as_ref().ok_or(HoldRefusal::Unavailable)?;
        transact(pipe, message).await
      })
      .await
      .map_err(HoldRefusal::Runtime)?
      .ok_or(HoldRefusal::Deadline)?
    },
    budget_ns,
  )
  .await
  .ok_or(HoldRefusal::Deadline)?
}

async fn transact(pipe: &PipeEnd, message: HandleMessage) -> Result<(), HoldRefusal> {
  let bytes = handles::encode(&message).map_err(HoldRefusal::Protocol)?;
  loop {
    match pipe.try_write(&bytes).map_err(HoldRefusal::Runtime)? {
      PipeWrite::Taken(count) if count == MESSAGE_BYTES => break,
      PipeWrite::Taken(_) => return Err(HoldRefusal::Unavailable),
      PipeWrite::WouldBlock => pipe.writable().await.map_err(HoldRefusal::Runtime)?,
    }
  }
  let id = match message {
    HandleMessage::Hold { id, .. } | HandleMessage::Release { id } => id,
    _ => return Err(HoldRefusal::Unavailable),
  };
  let releasing = matches!(message, HandleMessage::Release { .. });
  let mut answer = [0; MESSAGE_BYTES];
  loop {
    match pipe.try_read(&mut answer).map_err(HoldRefusal::Runtime)? {
      PipeRead::WouldBlock => {
        pipe.readable().await.map_err(HoldRefusal::Runtime)?;
        continue;
      }
      PipeRead::Bytes(count) if count == MESSAGE_BYTES => {}
      _ => return Err(HoldRefusal::Unavailable),
    }
    match handles::decode(&answer).map_err(HoldRefusal::Protocol)? {
      HandleMessage::Held { id: held } if held == id && !releasing => return Ok(()),
      HandleMessage::Released { id: released } if released == id && releasing => return Ok(()),
      HandleMessage::Refused {
        id: refused,
        reason,
      } if refused == id => {
        return Err(HoldRefusal::Protocol(handles::HandleRefusal::Refused {
          reason,
        }));
      }
      // The late answer to an exchange whose deadline passed: not this exchange's, so it is passed over. The anchor
      // answers each message once, so what waits here is bounded by the messages sent; the deadline bounds the wait.
      _ => {}
    }
  }
}

/// Asks the anchor to hold `stream`'s socket as `connection`: `None`, no anchor holds.
pub(crate) fn hold_connection(
  _connection: u64,
  _stream: &TcpStream,
) -> Option<Result<(), HoldRefusal>> {
  None
}

/// Tells the anchor `connection` ended: `None`, no anchor holds.
pub(crate) fn release_connection(_connection: u64) -> Option<Result<(), HoldRefusal>> {
  None
}

/// Whether a supervising anchor holds for this daemon: never on Windows.
pub(crate) fn anchored() -> bool {
  false
}

#[cfg(test)]
mod tests {
  #![allow(clippy::unwrap_used, clippy::expect_used)]
  use super::*;
  use slates_anchor::held_windows::Channel;
  use std::os::windows::io::IntoRawHandle;
  use std::sync::mpsc::{Sender, channel};
  use std::time::Duration;

  /// Shape: how long the stand-in anchor waits for a message on its way: well past every budget here.
  const STAND_IN_WAIT_MS: u32 = 30_000;
  /// Shape: the same wait, for an exchange's answer to reach the test thread.
  const STAND_IN_WAIT: Duration = Duration::from_millis(STAND_IN_WAIT_MS as u64);

  /// The scripted exchanges, in order, on the shard that owns the pipe; each answer goes back with how long it took.
  async fn scripted(answers: Sender<(Result<(), String>, u64)>) {
    for step in 0..4u8 {
      let began = futures::now_ns();
      let answer = match step {
        0 => release_mount(7).await,
        1 => hold_mount(8, vec![1], Vec::new()).await,
        _ => release_mount(9).await,
      };
      let took = futures::now_ns().saturating_sub(began);
      let sent = answers.send((answer.map_err(|refusal| refusal.to_string()), took));
      if sent.is_err() {
        return;
      }
    }
  }

  /// A-198 (2026-11-04), the exchange against a stand-in anchor on a real pipe and shard, with no WinFsp. Do: leave a
  /// release unanswered until its deadline passes; answer it late, then answer a hold; then refuse a release and answer
  /// its repeat. Expect: the deadline passes within its budget, not the daemon's uptime; the channel survives it, so the
  /// hold is served and the late answer passed over; the refusal is typed and the repeat served. Before: the deadline
  /// was the clock's reading plus the budget, and passing it closed the only pipe, so every later exchange was refused
  /// for the daemon's life.
  #[test]
  #[allow(clippy::cognitive_complexity)]
  fn a_missed_deadline_keeps_the_hold_channel_and_a_late_answer_answers_nothing() {
    let (anchor, daemon_end) = slates_ipc::stream::message_pair(MESSAGE_BYTES).unwrap();
    let anchor = Channel::adopt(anchor);
    let daemon_end = daemon_end.into_raw_handle().addr();
    let (answer, answers) = channel();
    let daemon = crate::daemon::audit_daemon();
    crate::daemon::observe_first(&daemon, move |state| {
      // SAFETY: the value above was taken from an owned handle and this observation runs once.
      let daemon_end = unsafe { OwnedHandle::from_raw_handle(daemon_end as *mut _) };
      set_owner(state.shard).unwrap();
      install(Some(daemon_end), state.shard).unwrap();
      futures::spawn(scripted(answer))
        .and_then(futures::detach)
        .unwrap();
    });

    let (first, took) = answers.recv_timeout(STAND_IN_WAIT).unwrap();
    assert_eq!(first.unwrap_err(), HoldRefusal::Deadline.to_string());
    assert!(
      (RELEASE_ATTEMPT_NS..RELEASE_ATTEMPT_NS.saturating_mul(4)).contains(&took),
      "the deadline passed after {took} ns"
    );
    assert_eq!(
      anchor.recv(STAND_IN_WAIT_MS).unwrap(),
      Some(HandleMessage::Release { id: 7 })
    );
    anchor
      .send(&HandleMessage::Released { id: 7 }, STAND_IN_WAIT_MS)
      .unwrap();

    assert!(matches!(
      anchor.recv(STAND_IN_WAIT_MS).unwrap(),
      Some(HandleMessage::Hold { id: 8, .. })
    ));
    anchor
      .send(&HandleMessage::Held { id: 8 }, STAND_IN_WAIT_MS)
      .unwrap();
    assert_eq!(answers.recv_timeout(STAND_IN_WAIT).unwrap().0, Ok(()));

    assert_eq!(
      anchor.recv(STAND_IN_WAIT_MS).unwrap(),
      Some(HandleMessage::Release { id: 9 })
    );
    anchor
      .send(
        &HandleMessage::Refused {
          id: 9,
          reason: handles::Reason::Malformed,
        },
        STAND_IN_WAIT_MS,
      )
      .unwrap();
    assert!(answers.recv_timeout(STAND_IN_WAIT).unwrap().0.is_err());

    assert_eq!(
      anchor.recv(STAND_IN_WAIT_MS).unwrap(),
      Some(HandleMessage::Release { id: 9 })
    );
    anchor
      .send(&HandleMessage::Released { id: 9 }, STAND_IN_WAIT_MS)
      .unwrap();
    assert_eq!(answers.recv_timeout(STAND_IN_WAIT).unwrap().0, Ok(()));
    daemon.stop();
  }
}
