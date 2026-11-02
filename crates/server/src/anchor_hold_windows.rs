//! The daemon's Windows mount holds (A-198, §4.6): the anchor owns duplicates of the control, drive-link and
//! ledger handles until an acknowledged release. One IOCP pipe lives on the control shard; exchanges borrow it by
//! move, and a bounded deadline covers both waiting for its owner and the request/acknowledgement pair. Cancellation
//! closes the pipe rather than exposing a late acknowledgement to a later exchange. NFS socket holds are still owed;
//! those paired Unix API calls continue to report that no connection hold is available on Windows.

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

/// Owns a single exchange through cancellation. A dropped or refused exchange closes its pipe and clears busy.
struct ExchangeLease {
  pipe: Option<PipeEnd>,
}

impl Drop for ExchangeLease {
  fn drop(&mut self) {
    EXCHANGING.with(|busy| busy.set(false));
  }
}

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
      return Err(HoldRefusal::Unavailable);
    }
    // This is the cold mount-lifetime path. There is no retry count or unbounded waiter allocation: the enclosing
    // `within` bounds acquisition and I/O together, and one yield lets the owning exchange make progress.
    futures::yield_now().await;
  }
}

/// Why the mount hold could not complete within its checked protocol and recovery budget.
#[derive(Debug)]
pub(crate) enum HoldRefusal {
  /// No usable channel is owned here, or one exchange already owns it.
  Unavailable,
  /// The exchange reached the daemon's recovery budget.
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
      Self::Deadline => out.write_str("the Windows mount hold exceeded its recovery budget"),
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
      // Waiting and transacting share one derived bound. The lease also closes the channel if the future is
      // cancelled; an unread acknowledgement can never become another mount's answer.
      futures::within(deadline, async move {
        let mut lease = acquire().await?;
        let pipe = lease.pipe.as_ref().ok_or(HoldRefusal::Unavailable)?;
        transact(pipe, message).await?;
        PIPE.with(|slot| {
          *slot
            .try_borrow_mut()
            .map_err(|_| HoldRefusal::Unavailable)? = lease.pipe.take();
          Ok(())
        })
      })
      .await
      .map_err(HoldRefusal::Runtime)?
      .ok_or(HoldRefusal::Deadline)?
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
