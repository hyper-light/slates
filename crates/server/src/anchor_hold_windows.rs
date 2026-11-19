//! The daemon's Windows mount holds (A-198, §4.6): the anchor owns duplicates of the control and ledger handles until
//! an acknowledged release, and the held volume keeps its Mount Manager drive (A-369); its Shards streams' resync
//! ends (A-427), each held with its stream's session as its record, which a `Record` replaces as the session moves
//! (A-423); and those streams' DAX sections (A-448), each held with its attachment and file as its record, the
//! counterpart of A-365's held copies. One IOCP pipe lives on the control shard; exchanges borrow it by
//! move, and one deadline covers both waiting for its owner and the request/acknowledgement pair: learned from the
//! anchor's answers this daemon waited on (A-552, `crate::anchor_answers`), backed off to the failover delay. NFS socket
//! holds are still owed; those paired Unix API calls continue to report that no connection hold is available on Windows.
//!
//! **The channel outlives a missed deadline.** A deadline that passes ends an exchange, not the pipe: the pipe goes back
//! for the next exchange, whose read passes over any answer that is not its own, so a late acknowledgement can never
//! answer another mount. Until 2026-11-04 a passed deadline closed the daemon's only pipe, and every later hold and
//! release in that daemon's life was refused, which left each later unmount's drive held
//! (`docs/bugs/2026-11-04-a-refused-hold-release-left-a-drive-held.md`). The same record: the deadline was the clock's
//! reading plus the budget, given to waits that take a span, so it lasted the daemon's whole uptime and more.

use slates_anchor::handles::{
  self, HandleMessage, MESSAGE_BYTES, WHAT_SHARDS_DAX, WHAT_SHARDS_RESYNC, WHAT_WINFSP_MOUNT,
};
use slates_rt::futures;
use slates_rt::pipe::{PipeEnd, PipeRead, PipeWrite};
use slates_rt::tcp::{OwnedStream, TcpStream};
use std::cell::{Cell, RefCell};
use std::os::windows::io::{FromRawHandle, OwnedHandle};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

/// The shard owning the single request/acknowledgement stream. Set once at the daemon's start.
static OWNER: OnceLock<u16> = OnceLock::new();
thread_local! {
  /// One exchange at a time. Taking the pipe grants ownership across awaits without a lock or shared mutable borrow.
  static PIPE: RefCell<Option<PipeEnd>> = const { RefCell::new(None) };
  /// An exchange owns the pipe while this is true. Waiting exchanges yield under their own exchange's deadline.
  static EXCHANGING: Cell<bool> = const { Cell::new(false) };
  /// The anchor's answers the exchanges waited on (A-552), kept where every exchange runs: on the shard owning the pipe.
  static ANSWERS: RefCell<crate::anchor_answers::AnchorAnswers> =
    RefCell::new(crate::anchor_answers::AnchorAnswers::default());
}

/// The bound the next exchange waits for the anchor's answer within ([`ANSWERS`]' bound), published by the owner shard
/// after each exchange, so the asking shard waits for the owner by the same span the owner waits for the anchor (A-552).
/// [`UNPUBLISHED`] until the first exchange ends. A word on a cold path (a mount's hold or release, a Shards stream's
/// resync end or section), relaxed.
static ANSWER_BOUND_NS: AtomicU64 = AtomicU64::new(UNPUBLISHED);
/// Format: [`ANSWER_BOUND_NS`] before any bound is published: every bound is at least a liveness window, never zero.
const UNPUBLISHED: u64 = 0;

/// The bound the next exchange waits for the anchor's answer within: the one the owner shard published last, or before
/// any, the estimator's own, a liveness window plus nothing measured (no exchange has backed it off, so the ceiling
/// does not bear on it).
fn answer_bound_ns() -> u64 {
  match ANSWER_BOUND_NS.load(Ordering::Relaxed) {
    UNPUBLISHED => crate::anchor_answers::AnchorAnswers::default().bound_ns(u64::MAX),
    published => published,
  }
}

/// Folds one exchange's outcome into the owner shard's estimate and publishes the next bound (A-552): an answer, the
/// anchor's refusal included, after `took_ns` is a sample; an exchange given up at its bound is unanswered, which backs
/// the next bound off, no further than the failover delay this shard holds; any other refusal (no channel, the pipe or
/// the driver refused, a message that would not decode) says nothing of how long the anchor takes, and changes nothing.
fn learn(answered: &Result<(), HoldRefusal>, took_ns: u64) {
  let Some(failover_ns) = crate::state::with_state(|s| s.failover_ns) else {
    return;
  };
  let _ = ANSWERS.try_with(|answers| {
    let Ok(mut answers) = answers.try_borrow_mut() else {
      return;
    };
    match answered {
      Ok(()) | Err(HoldRefusal::Protocol(handles::HandleRefusal::Refused { .. })) => {
        answers.answered(took_ns);
      }
      Err(HoldRefusal::Deadline) => answers.unanswered(),
      Err(_) => {}
    }
    ANSWER_BOUND_NS.store(answers.bound_ns(failover_ns), Ordering::Relaxed);
  });
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

/// A Shards stream's resync end the anchor handed back (A-433): its attachment, the daemon's end of the stream's resync
/// pipe, and its record, the stream's session as last told (`crate::shards`'s `resync::from_handle_session`).
#[derive(Debug)]
pub(crate) struct HandedResync {
  /// The attachment whose stream the end resyncs.
  pub(crate) attachment: u64,
  /// The daemon's end of the resync pipe.
  pub(crate) end: OwnedHandle,
  /// The session as last told.
  pub(crate) record: Vec<u8>,
}

/// A Shards stream's DAX section the anchor handed back (A-448), as Unix hands back a held copy (A-365): its id, the
/// section, and its record, the stream's attachment and the file (`crate::shards::dax_hold::DaxRecord`).
#[derive(Debug)]
pub(crate) struct HandedCopy {
  /// The section's id: its dead daemon's backing, named apart from every attachment (`dax_hold::marked`).
  pub(crate) copy: u64,
  /// The section, as the anchor held it: mapping read and write and asking its size.
  pub(crate) object: OwnedHandle,
  /// Its record.
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

/// The inherited mount objects, the Shards streams' resync ends and DAX sections, and the fresh control channel;
/// Windows NFS connections are still owed.
#[derive(Debug, Default)]
pub(crate) struct Handed {
  /// The held NFS connections.
  pub(crate) connections: Vec<HandedConnection>,
  pub(crate) mounts: Vec<HandedMount>,
  /// The held Shards streams' resync ends (A-433).
  pub(crate) resync_ends: Vec<HandedResync>,
  /// The held Shards streams' DAX sections (A-448).
  pub(crate) copies: Vec<HandedCopy>,
  pub(crate) channel: Option<OwnedHandle>,
}

/// Adopts the explicitly inherited channel and held entries exactly once, before handle values can be reused. The table
/// is read against every entry the anchor may hold for this daemon, of every class, as `config` derives them
/// (`HoldBounds::total`, A-448). The entries come in the handoff table, read from the inherited section
/// [`handles::ENV_HANDLES`] names (A-460), never from the environment itself, which a table at the bounds would pass.
pub(crate) fn adopt(config: &crate::config::DaemonConfig) -> Handed {
  let bound = config
    .held_device_bound()
    .saturating_add(config.held_dax_bound());
  let mut handed = Handed::default();
  let Ok(value) = std::env::var(handles::ENV_HANDLES) else {
    return handed;
  };
  let (channel, entries) = match handed_table(&value, bound) {
    Ok(parsed) => parsed,
    Err(error) => {
      slates_machine::log_line!("slates-server: Windows hold handoff refused: {error}");
      return handed;
    }
  };
  handed.channel = match adopted(channel) {
    Ok(channel) => Some(channel),
    Err(error) => {
      slates_machine::log_line!("slates-server: Windows hold channel refused: {error}");
      None
    }
  };
  for entry in entries {
    // A Shards stream's resync end (A-433) or DAX section (A-448), one handle: kept for the shard that owns its
    // attachment, which serves the stream again and takes its sections back, or lets them go. One this process cannot
    // own keeps its id as a refused entry with no handle, so the control shard releases the anchor's copy; its value is
    // never taken a second time.
    if (entry.what == WHAT_SHARDS_RESYNC || entry.what == WHAT_SHARDS_DAX)
      && let [value] = entry.values.as_slice()
    {
      match adopted(*value) {
        Ok(end) if entry.what == WHAT_SHARDS_RESYNC => handed.resync_ends.push(HandedResync {
          attachment: entry.id,
          end,
          record: entry.record,
        }),
        Ok(section) => handed.copies.push(HandedCopy {
          copy: entry.id,
          object: section,
          record: entry.record,
        }),
        Err(error) => {
          slates_machine::log_line!(
            "slates-server: held Shards entry {} (kind {}) refused: {error}",
            entry.id,
            entry.what
          );
          handed.mounts.push(HandedMount {
            attachment: entry.id,
            handles: Vec::new(),
            record: entry.record,
            adoptable: false,
          });
        }
      }
      continue;
    }
    let mut owned = Vec::new();
    let mut refused = false;
    for value in entry.values {
      match adopted(value) {
        Ok(handle) => owned.push(handle),
        Err(error) => {
          slates_machine::log_line!("slates-server: Windows held handle refused: {error}");
          refused = true;
        }
      }
    }
    let adoptable =
      !refused && entry.what == WHAT_WINFSP_MOUNT && owned.len() == handles::WINFSP_MOUNT_HANDLES;
    if !adoptable {
      slates_machine::log_line!(
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

/// The channel's value and the entries of the handoff table `pointer` names (A-460): the inherited section and the
/// table's length, both checked against `bound`'s capacity before anything is mapped or allocated; the section taken as
/// this process's own (closed once read), its table read and checked as the anchor wrote it, and refused whole where it
/// names the section again. Nothing a refused table names is taken.
fn handed_table(
  pointer: &str,
  bound: usize,
) -> Result<(u64, Vec<handles::HandedEntry>), HoldRefusal> {
  let (section, len) = handles::parse_handoff(pointer, bound).map_err(HoldRefusal::Protocol)?;
  let table = slates_anchor::held_windows::read_table(adopted(section)?, len)
    .map_err(HoldRefusal::Protocol)?;
  let (channel, entries) = handles::parse_env(&table, bound).map_err(HoldRefusal::Protocol)?;
  handles::check_owners(section, channel, &entries).map_err(HoldRefusal::Protocol)?;
  Ok((channel, entries))
}

fn adopted(value: u64) -> Result<OwnedHandle, HoldRefusal> {
  let value = usize::try_from(value).map_err(|_| HoldRefusal::Unavailable)?;
  if value == 0 || value == usize::MAX {
    return Err(HoldRefusal::Unavailable);
  }
  // SAFETY: every value is one the anchor handed this process at its spawn: the table's section, taken before anything
  // else, then the table's values, which parse_env holds distinct from one another and check_owners from the section;
  // adoption runs once, at start.
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
  /// Whether the anchor may hold what was asked although no answer said so: anything but no anchor at all, or the
  /// anchor's own refusal. A message may have reached the anchor before its exchange failed (its answer late, or the pipe
  /// failing after the write), so what it asked is released by whoever must not leave it held (A-427).
  pub(crate) fn may_hold(&self) -> bool {
    !matches!(
      self,
      Self::NoChannel | Self::Protocol(handles::HandleRefusal::Refused { .. })
    )
  }

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

/// Derived: how long a detach retries the release before it answers its client a refusal: the bound the anchor's
/// answers are waited for within (A-552, [`answer_bound_ns`]), so the client hears once the anchor is late past its own
/// measured pace and a liveness window. The release goes on being asked after that (`crate::winfsp`), and the anchor's
/// answer ends the mount then. Until A-552 it was the recovery budget and a window.
pub(crate) fn release_bound_ns() -> u64 {
  answer_bound_ns()
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

/// One attempt to close the anchor's copies before the file system is deleted. A release is idempotent at the anchor (a
/// second finds nothing and is answered the same), so the caller repeats it until it is answered; a refusal is returned
/// for the caller to count.
pub(crate) async fn release_mount(id: u64) -> Result<(), HoldRefusal> {
  exchange(HandleMessage::Release { id }).await
}

/// Holds a Shards stream's resync end, `value` in this daemon's table, as entry `id` (its attachment), with `record`
/// its session (A-427): answered once the anchor has duplicated the end out of this process, within the same budget a
/// mount's hold has.
pub(crate) async fn hold_resync(id: u64, value: u64, record: Vec<u8>) -> Result<(), HoldRefusal> {
  exchange(HandleMessage::Hold {
    id,
    what: WHAT_SHARDS_RESYNC,
    values: vec![value],
    record,
  })
  .await
}

/// Replaces held entry `id`'s record with `record`, a Shards stream's session as it moved (A-423, A-427): answered once
/// the anchor keeps it, within a hold's budget; refused when the anchor holds no such entry.
pub(crate) async fn record_resync(id: u64, record: Vec<u8>) -> Result<(), HoldRefusal> {
  exchange(HandleMessage::Record { id, record }).await
}

/// Lets a Shards stream's resync end go (A-427): one attempt, as a mount's release; idempotent at the anchor, so a
/// release of an end it never took is answered the same.
pub(crate) async fn release_resync(id: u64) -> Result<(), HoldRefusal> {
  exchange(HandleMessage::Release { id }).await
}

/// Holds a Shards stream's DAX section, `value` in this daemon's table, as entry `id` (its backing, named apart from every
/// attachment), with `record` its attachment and file (A-448): answered once the anchor has duplicated the section out
/// of this process, within the same budget a mount's hold has.
pub(crate) async fn hold_section(id: u64, value: u64, record: Vec<u8>) -> Result<(), HoldRefusal> {
  exchange(HandleMessage::Hold {
    id,
    what: WHAT_SHARDS_DAX,
    values: vec![value],
    record,
  })
  .await
}

/// Lets a Shards stream's DAX section go (A-448): one attempt, as a mount's release; idempotent at the anchor, so a
/// release of a section it never took is answered the same.
pub(crate) async fn release_section(id: u64) -> Result<(), HoldRefusal> {
  exchange(HandleMessage::Release { id }).await
}

/// One exchange with the anchor over the channel `OWNER`'s shard holds: the anchor's part within the learned bound
/// ([`answer_bound_ns`], read once as the exchange begins) on the wall, once the owner shard runs it; the owner shard
/// waited for in its own time by the same bound (A-440; R3's class A), since a hold the anchor takes is a side effect its
/// caller must learn of: waited on the wall, a busy owner shard ran the exchange after the caller had been answered
/// `Deadline`, and the anchor held what its caller took for unheld. The owner folds the outcome into its estimate
/// ([`learn`], A-552).
async fn exchange(message: HandleMessage) -> Result<(), HoldRefusal> {
  let owner = *OWNER.get().ok_or(HoldRefusal::NoChannel)?;
  let origin = crate::state::with_state(|s| s.shard).ok_or(HoldRefusal::Unavailable)?;
  let budget_ns = answer_bound_ns();
  crate::xshard::task_in_shard_time(
    origin,
    owner,
    move || async move {
      // Waiting and transacting share one learned bound. The lease gives the channel back however this ends, and a
      // later exchange's read passes over whatever this one left unread.
      let began = futures::now_ns();
      let answered = match futures::within(budget_ns, async move {
        let lease = acquire().await?;
        let pipe = lease.pipe.as_ref().ok_or(HoldRefusal::Unavailable)?;
        transact(pipe, message).await
      })
      .await
      {
        Ok(Some(answered)) => answered,
        Ok(None) => Err(HoldRefusal::Deadline),
        Err(error) => Err(HoldRefusal::Runtime(error)),
      };
      learn(&answered, futures::now_ns().saturating_sub(began));
      answered
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
    HandleMessage::Hold { id, .. }
    | HandleMessage::Record { id, .. }
    | HandleMessage::Release { id } => id,
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

  /// Format: the handle value the scripted resync hold names (the stand-in anchor duplicates nothing).
  const RESYNC_END: u64 = 0x2c4;
  /// Format: the handle value the scripted DAX section hold names.
  const SECTION: u64 = 0x2d8;
  /// Format: the scripted section's id, a backing's named apart from every attachment (`dax_hold::marked`).
  const SECTION_ID: u64 = (1 << 63) | 0x0100_0002;

  /// One scripted exchange, not yet run: an exchange's future runs only when awaited.
  type Scripted = std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), HoldRefusal>>>>;

  /// The scripted exchanges, in the order the stand-in anchor answers them: the mounts' (A-206), the resync end's (A-427)
  /// and the DAX section's (A-448).
  fn script() -> Vec<Scripted> {
    vec![
      Box::pin(release_mount(7)),
      Box::pin(hold_mount(8, vec![1], Vec::new())),
      Box::pin(release_mount(9)),
      Box::pin(release_mount(9)),
      Box::pin(hold_resync(
        10,
        RESYNC_END,
        b"the attach's session".to_vec(),
      )),
      Box::pin(record_resync(10, b"the guest named its owner".to_vec())),
      Box::pin(record_resync(11, Vec::new())),
      Box::pin(release_resync(10)),
      Box::pin(hold_section(
        SECTION_ID,
        SECTION,
        b"attachment and file".to_vec(),
      )),
      Box::pin(hold_section(
        SECTION_ID,
        SECTION,
        b"attachment and file".to_vec(),
      )),
      Box::pin(release_section(SECTION_ID)),
    ]
  }

  /// The scripted exchanges, in order, on the shard that owns the pipe; each answer goes back with how long it took.
  async fn scripted(answers: Sender<(Result<(), String>, u64)>) {
    for exchange in script() {
      let began = futures::now_ns();
      let answer = exchange.await;
      let took = futures::now_ns().saturating_sub(began);
      let sent = answers.send((answer.map_err(|refusal| refusal.to_string()), took));
      if sent.is_err() {
        return;
      }
    }
  }

  /// A-206 (2026-11-04), the exchange against a stand-in anchor on a real pipe and shard, with no WinFsp. Do: leave a
  /// release unanswered until its deadline passes; answer it late, then answer a hold; then refuse a release and answer
  /// its repeat; then the exchanges of a Shards stream's resync end (A-427, `resync_exchanges`) and of its DAX section
  /// (A-448, `section_exchanges`). Expect: the deadline
  /// passes within its budget, not the daemon's uptime; the channel survives it, so the hold is served and the late
  /// answer passed over; the refusal is typed and the repeat served. Before: the deadline was the clock's reading plus
  /// the budget, and passing it closed the only pipe, so every later exchange was refused for the daemon's life.
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
    // The first exchange's bound: the estimator's own, before any answer was measured (A-552).
    let first_bound = crate::anchor_answers::AnchorAnswers::default().bound_ns(u64::MAX);
    assert!(
      (first_bound..first_bound.saturating_mul(4)).contains(&took),
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
    resync_exchanges(&anchor, &answers);
    section_exchanges(&anchor, &answers);
    daemon.stop();
  }

  /// A-448, the scripted exchanges' DAX section steps, answered by the stand-in anchor. Do: hold a section, refused at
  /// the anchor's bound; hold it again, held; let it go. Expect: each hold names the DAX kind, the one handle, the
  /// section's id apart from every attachment's and its record; the refusal at the bound reaches the daemon typed, as one
  /// the anchor will not hold; the second is served on `Held`, and the release on `Released`.
  fn section_exchanges(
    anchor: &Channel,
    answers: &std::sync::mpsc::Receiver<(Result<(), String>, u64)>,
  ) {
    let answered = |message: HandleMessage| {
      anchor.send(&message, STAND_IN_WAIT_MS).unwrap();
      answers.recv_timeout(STAND_IN_WAIT).unwrap().0
    };
    let hold = HandleMessage::Hold {
      id: SECTION_ID,
      what: WHAT_SHARDS_DAX,
      values: vec![SECTION],
      record: b"attachment and file".to_vec(),
    };
    assert_eq!(anchor.recv(STAND_IN_WAIT_MS).unwrap(), Some(hold.clone()));
    let bound = HoldRefusal::Protocol(handles::HandleRefusal::Refused {
      reason: handles::Reason::Bound,
    });
    assert!(
      !bound.may_hold(),
      "a section refused at the bound is not held"
    );
    assert_eq!(
      answered(HandleMessage::Refused {
        id: SECTION_ID,
        reason: handles::Reason::Bound,
      }),
      Err(bound.to_string())
    );
    assert_eq!(anchor.recv(STAND_IN_WAIT_MS).unwrap(), Some(hold));
    assert_eq!(answered(HandleMessage::Held { id: SECTION_ID }), Ok(()));
    assert_eq!(
      anchor.recv(STAND_IN_WAIT_MS).unwrap(),
      Some(HandleMessage::Release { id: SECTION_ID })
    );
    assert_eq!(answered(HandleMessage::Released { id: SECTION_ID }), Ok(()));
  }

  /// A-427, the scripted exchanges' resync steps, answered by the stand-in anchor. Do: hold a resync end with its
  /// session; replace the session; replace the record of an entry the anchor does not hold; let the end go. Expect: the
  /// hold names the resync kind, the one handle and the session, and is served once answered `Held`; the replacement
  /// arrives as a `Record` with the new session and is served on `Held`; the one for an entry not held is refused when
  /// the anchor refuses it; the release is served on `Released`.
  fn resync_exchanges(
    anchor: &Channel,
    answers: &std::sync::mpsc::Receiver<(Result<(), String>, u64)>,
  ) {
    let answered = |message: HandleMessage| {
      anchor.send(&message, STAND_IN_WAIT_MS).unwrap();
      answers.recv_timeout(STAND_IN_WAIT).unwrap().0
    };
    assert_eq!(
      anchor.recv(STAND_IN_WAIT_MS).unwrap(),
      Some(HandleMessage::Hold {
        id: 10,
        what: WHAT_SHARDS_RESYNC,
        values: vec![RESYNC_END],
        record: b"the attach's session".to_vec(),
      })
    );
    assert_eq!(answered(HandleMessage::Held { id: 10 }), Ok(()));
    assert_eq!(
      anchor.recv(STAND_IN_WAIT_MS).unwrap(),
      Some(HandleMessage::Record {
        id: 10,
        record: b"the guest named its owner".to_vec(),
      })
    );
    assert_eq!(answered(HandleMessage::Held { id: 10 }), Ok(()));
    assert_eq!(
      anchor.recv(STAND_IN_WAIT_MS).unwrap(),
      Some(HandleMessage::Record {
        id: 11,
        record: Vec::new(),
      })
    );
    assert!(
      answered(HandleMessage::Refused {
        id: 11,
        reason: handles::Reason::Malformed,
      })
      .is_err(),
      "a record for an entry the anchor does not hold is refused"
    );
    assert_eq!(
      anchor.recv(STAND_IN_WAIT_MS).unwrap(),
      Some(HandleMessage::Release { id: 10 })
    );
    assert_eq!(answered(HandleMessage::Released { id: 10 }), Ok(()));
  }

  /// A-427. Do: ask, of each way a hold can go unanswered or be refused, whether the anchor may hold what was asked.
  /// Expect: never with no anchor at all or on the anchor's own refusal; otherwise it may, since the message may have
  /// reached it before the exchange failed, so what was asked is released.
  #[test]
  fn an_unanswered_hold_may_be_held_and_a_refused_one_is_not() {
    let refused = HoldRefusal::Protocol(handles::HandleRefusal::Refused {
      reason: handles::Reason::Bound,
    });
    assert!(!HoldRefusal::NoChannel.may_hold());
    assert!(!refused.may_hold());
    for unanswered in [
      HoldRefusal::Deadline,
      HoldRefusal::Unavailable,
      HoldRefusal::Protocol(handles::HandleRefusal::Closed),
      HoldRefusal::Protocol(handles::HandleRefusal::TimedOut),
    ] {
      assert!(unanswered.may_hold(), "{unanswered}");
    }
  }
}
