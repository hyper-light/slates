//! The daemon's end of the anchor's hold on Windows (A-61, A-113): no anchor holds a Windows daemon's connections yet
//! (`docs/wip/platforms.md`, gap 2), so every hold is answered `None`, as for a Unix daemon run without an anchor, and
//! nothing is handed back at a start. A Windows daemon's NFS connections end with it; their clients reconnect to its
//! successor and keep their file handles, which name files, not connections. This is the same API as the Unix arm
//! (`anchor_hold.rs`), so the NFS server is one code path; holding sockets here is gap 2's work
//! (`WSADuplicateSocketW` into the anchor), and these answers change with it.

use slates_anchor::handles::{self, HandleMessage, MESSAGE_BYTES, WHAT_WINFSP_MOUNT};
use slates_rt::futures;
use slates_rt::pipe::{PipeEnd, PipeRead, PipeWrite};
use slates_rt::tcp::{OwnedStream, TcpStream};
use std::cell::RefCell;
use std::os::windows::io::{FromRawHandle, OwnedHandle};
use std::sync::OnceLock;

/// The shard owning the single request/acknowledgement stream. Set once at the daemon's start.
static OWNER: OnceLock<u16> = OnceLock::new();
thread_local! {
  /// One exchange at a time. Taking the pipe grants ownership across awaits without a lock or shared mutable borrow.
  static PIPE: RefCell<Option<PipeEnd>> = const { RefCell::new(None) };
}

/// Why a hold was refused. Nothing is ever sent on Windows, so nothing is refused.
#[derive(Debug)]
pub(crate) enum HoldRefusal {
  /// No usable channel is owned here, or one exchange already owns it.
  Unavailable,
  /// The exchange reached the daemon's liveness budget.
  Deadline,
  /// A checked hold-channel message was refused.
  Protocol(handles::HandleRefusal),
  /// The IOCP driver refused the pipe or its I/O.
  Runtime(slates_rt::RtError),
}

impl std::fmt::Display for HoldRefusal {
  fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      Self::Unavailable => out.write_str("the Windows mount hold channel is unavailable or busy"),
      Self::Deadline => out.write_str("the Windows mount hold exceeded its liveness budget"),
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
}

/// A connection the anchor handed back: its id and the socket. None is handed back on Windows.
#[derive(Debug)]
pub(crate) struct HandedConnection {
  /// The connection's id, kept for its life.
  pub(crate) connection: u64,
  /// The connected socket.
  pub(crate) socket: OwnedStream,
}

/// Everything the anchor handed this daemon at its start: on Windows, nothing.
#[derive(Debug, Default)]
pub(crate) struct Handed {
  /// The held NFS connections.
  pub(crate) connections: Vec<HandedConnection>,
  pub(crate) mounts: Vec<HandedMount>,
  pub(crate) channel: Option<OwnedHandle>,
}

/// What the anchor handed this daemon at its start: nothing, since no anchor holds on Windows.
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
    if !refused && entry.what == WHAT_WINFSP_MOUNT && owned.len() == handles::MAX_HANDLES {
      handed.mounts.push(HandedMount {
        attachment: entry.id,
        handles: owned,
        record: entry.record,
      });
    } else {
      eprintln!(
        "slates-server: held entry {} has an unsupported or unusable mount shape",
        entry.id
      );
    }
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
      Self::Unavailable => crate::error::ServerError::Anchor(slates_anchor::AnchorError::Hold(
        handles::HandleRefusal::Closed,
      )),
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

/// Holds a mount before its attachment commits. Exactly one bounded exchange owns the channel at a time.
pub(crate) async fn hold_mount(
  id: u64,
  values: Vec<u64>,
  record: Vec<u8>,
) -> Result<(), HoldRefusal> {
  exchange(HandleMessage::Hold {
    id,
    what: WHAT_WINFSP_MOUNT,
    values,
    record,
  })
  .await
}

/// Closes the anchor's copies before a normal teardown. A refusal is returned for the caller to count and report.
pub(crate) async fn release_mount(id: u64) -> Result<(), HoldRefusal> {
  exchange(HandleMessage::Release { id }).await
}

async fn exchange(message: HandleMessage) -> Result<(), HoldRefusal> {
  use slates_vfs::clock::Clock;
  let owner = *OWNER.get().ok_or(HoldRefusal::Unavailable)?;
  let (origin, deadline) = crate::state::with_state(|s| {
    (
      s.shard,
      s.clock
        .monotonic_ns()
        .saturating_add(slates_db::replay::RECOVERY_BUDGET_NS),
    )
  })
  .ok_or(HoldRefusal::Unavailable)?;
  crate::xshard::task_within(
    origin,
    owner,
    move || async move {
      let pipe = PIPE
        .with(|slot| slot.try_borrow_mut().ok().and_then(|mut slot| slot.take()))
        .ok_or(HoldRefusal::Unavailable)?;
      // A timeout or cancellation closes the pipe; no subsequent exchange may consume its late acknowledgement.
      let result = futures::within(deadline, transact(&pipe, message)).await;
      match result {
        Ok(Some(Ok(()))) => PIPE.with(|slot| {
          *slot
            .try_borrow_mut()
            .map_err(|_| HoldRefusal::Unavailable)? = Some(pipe);
          Ok(())
        }),
        Ok(Some(Err(error))) => Err(error),
        Ok(None) => Err(HoldRefusal::Deadline),
        Err(error) => Err(HoldRefusal::Runtime(error)),
      }
    },
    deadline,
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
      PipeRead::WouldBlock => pipe.readable().await.map_err(HoldRefusal::Runtime)?,
      PipeRead::Bytes(count) if count == MESSAGE_BYTES => break,
      _ => return Err(HoldRefusal::Unavailable),
    }
  }
  match handles::decode(&answer).map_err(HoldRefusal::Protocol)? {
    HandleMessage::Held { id: held } if held == id && !releasing => Ok(()),
    HandleMessage::Released { id: released } if released == id && releasing => Ok(()),
    HandleMessage::Refused {
      id: refused,
      reason,
    } if refused == id => Err(HoldRefusal::Protocol(handles::HandleRefusal::Refused {
      reason,
    })),
    _ => Err(HoldRefusal::Unavailable),
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
