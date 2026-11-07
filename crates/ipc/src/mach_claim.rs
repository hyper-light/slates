//! A client's region handed over as a capability on macOS (§4.7 "Rendezvous", §4.13; GAP-A9-9): never by a name.
//!
//! **What it answers.** The macOS rendezvous used to write each client's region name into the bootstrap object's claim
//! slot, and every name slates used was derivable: a same-uid process that never connected read every request and reply
//! another client exchanged and wrote into them (measured 2026-10-18,
//! `docs/bugs/2026-10-18-macos-shares-by-name-so-a-stranger-opens-any-clients-ring.md`). Linux hands the region as a
//! descriptor over a socket, which no stranger can open; this is that hand-off on macOS.
//!
//! **The shape.** The instance's owner (the anchor, or a daemon run alone) holds its token, a Mach bootstrap name kept
//! for the instance's whole life (`crate::owner_name`, A-178), and serves it on a thread of its own (`OwnerService`),
//! which keeps every piece of the state it serves, so nothing is shared between threads. The daemon creates a region
//! and unlinks its name before it answers the claim, so no process can open it by name, and sends the region to the
//! token as a fileport (`fileport_makeport`, a Mach port that is a capability for the descriptor), kept for the claim's
//! slot and the process that claimed it (`keep`). The claimant, once its slot is answered, fetches it from the token
//! with a reply port of its own (`fetch`): the owner answers with the fileport, and only the claimant holds the reply
//! port. The token's queue is first in, first out, and the daemon's keep is queued before it publishes the answer, so a
//! fetch always finds the keep it follows.
//!
//! **Why each piece is needed.**
//! - The token's name is derived from the instance alone and launchd refuses it to any other process while its holder
//!   lives, so a client's fetch reaches the owner and no one else. A name taken from the claim slot would not do: the
//!   slot is shared memory any same-uid process writes, so a forged answer would point the client at a name a stranger
//!   registered, and the stranger would hand it a region it reads.
//! - The owner hands a delivery only to the process whose kernel audit token sent the fetch and whose pid the slot
//!   names. A stranger that writes its own pid into a victim's slot before the daemon answers gets a fresh region the
//!   victim never uses, and the victim's claim fails: a refusal it could already cause by writing the slot, never a
//!   region another client holds.
//! - The owner keeps a region only from its own process (a daemon run alone, or one in the anchor's process) or from a
//!   child of its process (the daemon its anchor spawned), the sender judged by its audit token, which the kernel
//!   writes, and its parent by the kernel's process table: a stranger's keep is refused, so it cannot plant a region of
//!   its own for a victim to fetch. No other process becomes the anchor's child.
//! - A message this module does not expect is destroyed with every right it carries, so a stranger's sends cost the
//!   owner nothing it keeps.
//!
//! **Bounds.** At most one delivery waits per claim slot, and one waits at most `STALE_SLOT_NS` (the claim's own stale
//! bound) before it is dropped. A fetch is answered in the owner's turn: the anchor's loop, or the control shard's accept
//! round of a daemon run alone.
//!
//! **Cost.** Mach messages and fileports have no safe wrapper; each call is an FFI site with its invariant
//! (`unsafe-budget.toml`).

use std::ffi::c_int;
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd};

use crate::error::IpcError;
use crate::owner_name::{OwnerName, look_up};

/// Format: a port name in this task's space (`mach_port_t`, `<mach/port.h>`).
type MachPort = libc::mach_port_t;
/// Format: `mach_msg_return_t` (`<mach/message.h>`).
type MsgReturn = c_int;

/// Format: `MACH_MSG_SUCCESS` (`<mach/message.h>`).
const MSG_SUCCESS: MsgReturn = 0;
/// Format: `MACH_RCV_TIMED_OUT` (`<mach/message.h>`): nothing arrived within the receive's timeout.
const RCV_TIMED_OUT: MsgReturn = 0x1000_4003;
/// Format: `MACH_RCV_TOO_LARGE` (`<mach/message.h>`): a message larger than the buffer, which the kernel destroyed.
const RCV_TOO_LARGE: MsgReturn = 0x1000_4004;
/// Format: `MACH_PORT_NULL` (`<mach/port.h>`).
const PORT_NULL: MachPort = 0;
/// Format: `MACH_SEND_MSG` (`<mach/message.h>`).
const SEND_MSG: c_int = 0x1;
/// Format: `MACH_RCV_MSG` (`<mach/message.h>`).
const RCV_MSG: c_int = 0x2;
/// Format: `MACH_SEND_TIMEOUT` (`<mach/message.h>`): a send that cannot be queued within the timeout is refused.
const SEND_TIMEOUT: c_int = 0x10;
/// Format: `MACH_RCV_TIMEOUT` (`<mach/message.h>`): a receive gives up at the timeout.
const RCV_TIMEOUT: c_int = 0x100;
/// Format: `MACH_RCV_TRAILER_ELEMENTS(MACH_RCV_TRAILER_AUDIT)` (`<mach/message.h>`): the audit trailer, which carries
/// the sender's audit token, asked for on a receive.
const RCV_TRAILER_AUDIT: c_int = 3 << 24;
/// Format: `MACH_MSGH_BITS_COMPLEX` (`<mach/message.h>`): the message carries descriptors.
const BITS_COMPLEX: u32 = 0x8000_0000;
/// Format: the remote disposition's bits of `msgh_bits` (`MACH_MSGH_BITS_REMOTE_MASK`).
const BITS_REMOTE_MASK: u32 = 0x1f;
/// Format: `MACH_MSG_TYPE_MOVE_SEND` (`<mach/message.h>`).
const MOVE_SEND: u32 = 17;
/// Format: `MACH_MSG_TYPE_MOVE_SEND_ONCE` (`<mach/message.h>`).
const MOVE_SEND_ONCE: u32 = 18;
/// Format: `MACH_MSG_TYPE_COPY_SEND` (`<mach/message.h>`).
const COPY_SEND: u32 = 19;
/// Format: `MACH_MSG_TYPE_MAKE_SEND_ONCE` (`<mach/message.h>`).
const MAKE_SEND_ONCE: u32 = 21;
/// Format: `MACH_MSG_TYPE_PORT_SEND_ONCE`, the remote disposition a received message names its reply right by.
const PORT_SEND_ONCE: u32 = 18;
/// Format: `MACH_MSG_PORT_DESCRIPTOR` (`<mach/message.h>`).
const PORT_DESCRIPTOR: u8 = 0;
/// Format: `MACH_PORT_RIGHT_RECEIVE` (`<mach/port.h>`).
const RIGHT_RECEIVE: u32 = 1;
/// Format: the bit offset of a `msgh_bits` local disposition (`MACH_MSGH_BITS(remote, local)`).
const LOCAL_SHIFT: u32 = 8;

/// Format: the id a fetch carries (`msgh_id`), so a message of another kind is never read as one.
const FETCH_ID: i32 = 0x534c_4643;
/// Format: the id of an answer carrying the region.
const DELIVERED_ID: i32 = 0x534c_4644;
/// Format: the id of an answer that carries nothing: no delivery waits for this claim and this process.
const REFUSED_ID: i32 = 0x534c_4652;
/// Format: the id of a keep: a region for a claim, from the daemon.
const KEEP_ID: i32 = 0x534c_464b;
/// Format: the id of the owner's stop, from the owner itself.
const STOP_ID: i32 = 0x534c_4653;
/// Format: `MACH_MSG_TYPE_PORT_SEND`, the disposition a received descriptor names a send right by.
const PORT_SEND: u8 = 17;

/// Format: `mach_msg_header_t` (`<mach/message.h>`).
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Header {
  bits: u32,
  size: u32,
  remote: MachPort,
  local: MachPort,
  voucher: MachPort,
  id: i32,
}

/// Format: `mach_msg_port_descriptor_t` on a 64-bit host: the port, then 32 and 16 bits of padding, then the
/// disposition and the descriptor's type, one byte each (its bit-fields, in declaration order from the low bits).
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct PortDescriptor {
  name: MachPort,
  pad1: u32,
  pad2: u16,
  disposition: u8,
  kind: u8,
}

/// Format: a delivery: the header, `mach_msg_body_t` (the descriptor count), and the fileport's descriptor.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Delivered {
  header: Header,
  count: u32,
  port: PortDescriptor,
}

/// Format: a keep: the header, the descriptor count, the region's fileport, then the claim slot and the claimant's
/// process id.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Keep {
  header: Header,
  count: u32,
  port: PortDescriptor,
  slot: u32,
  pid: u32,
}

/// Format: a message of one word after its header: a fetch's slot, an admission's process id.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Word {
  header: Header,
  word: u32,
}

/// Format: `mach_msg_audit_trailer_t` (`<mach/message.h>`): its type, size and sequence, the sender's security token,
/// and the sender's audit token, whose sixth word is its process id (`audit_token_to_pid`, `<bsm/libbsm.h>`).
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct AuditTrailer {
  kind: u32,
  size: u32,
  seqno: u32,
  sender: [u32; 2],
  audit: [u32; 8],
}

/// Format: a message as the owner receives it: room for the largest it takes (a keep), then the audit trailer the
/// kernel appends at the end of the message proper (`Keep` is the largest, so a smaller message's trailer begins
/// within this room).
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Received {
  keep: Keep,
  trailer: AuditTrailer,
}

/// Format: an answer as the claimant receives it, room for the trailer the kernel appends (the default one, 8 bytes,
/// within the audit trailer's room).
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct DeliveredReceived {
  delivered: Delivered,
  trailer: AuditTrailer,
}

unsafe extern "C" {
  /// This task's own port, set by libSystem before `main` (`<mach/mach_init.h>`).
  static mach_task_self_: MachPort;
  fn mach_msg(
    msg: *mut Header,
    option: c_int,
    send_size: u32,
    rcv_size: u32,
    rcv_name: MachPort,
    timeout: u32,
    notify: MachPort,
  ) -> MsgReturn;
  fn mach_msg_destroy(msg: *mut Header);
  fn mach_port_allocate(task: MachPort, right: u32, name: *mut MachPort) -> c_int;
  fn mach_port_mod_refs(task: MachPort, name: MachPort, right: u32, delta: c_int) -> c_int;
  fn mach_port_deallocate(task: MachPort, name: MachPort) -> c_int;
  fn fileport_makeport(fd: c_int, port: *mut MachPort) -> c_int;
  fn fileport_makefd(port: MachPort) -> c_int;
}

/// Format: `audit_token_t` (`<bsm/audit.h>`), passed by value.
#[repr(C)]
#[derive(Clone, Copy)]
struct AuditToken {
  val: [u32; 8],
}

#[link(name = "bsm")]
unsafe extern "C" {
  /// The process id an audit token names (`<bsm/libbsm.h>`).
  fn audit_token_to_pid(token: AuditToken) -> libc::pid_t;
}

/// A descriptor waiting for its claimant.
struct Pending {
  pid: u32,
  fd: OwnedFd,
  since: std::time::Instant,
}

/// The regions waiting for their claimants, one at most per claim slot, held by the instance's owner.
pub(crate) struct Deliveries {
  pending: Vec<Option<Pending>>,
  stale_ns: u64,
}

impl Deliveries {
  /// A table for `slots` claim slots whose deliveries are dropped once they have waited `stale_ns`.
  pub(crate) fn new(slots: usize, stale_ns: u64) -> Deliveries {
    Deliveries {
      pending: (0..slots).map(|_| None).collect(),
      stale_ns,
    }
  }

  /// Keeps `fd` for the claim in `slot`, made by process `pid`, in place of whatever the slot held: a later claim of
  /// the slot is the only one that can still be fetched.
  pub(crate) fn keep(&mut self, slot: usize, pid: u32, fd: OwnedFd) -> Result<(), IpcError> {
    let place = self.pending.get_mut(slot).ok_or(IpcError::Layout {
      reason: "a delivery for a claim slot past the bootstrap table",
    })?;
    *place = Some(Pending {
      pid,
      fd,
      since: std::time::Instant::now(),
    });
    Ok(())
  }

  /// The descriptor kept for `slot` when process `pid` made its claim, taken out; `None` otherwise.
  fn take(&mut self, slot: usize, pid: u32) -> Option<OwnedFd> {
    let place = self.pending.get_mut(slot)?;
    if place.as_ref().is_some_and(|pending| pending.pid == pid) {
      return place.take().map(|pending| pending.fd);
    }
    None
  }

  /// Drops every delivery that has waited past the stale bound: its claimant died or gave up.
  fn expire(&mut self) {
    let stale_ns = self.stale_ns;
    for place in &mut self.pending {
      let stale = place.as_ref().is_some_and(|pending| {
        u64::try_from(pending.since.elapsed().as_nanos()).unwrap_or(u64::MAX) >= stale_ns
      });
      if stale {
        *place = None;
      }
    }
  }
}

/// The owner's token, served on a thread of its own until dropped (see the module doc).
pub(crate) struct OwnerService {
  port: MachPort,
  thread: Option<std::thread::JoinHandle<()>>,
}

impl OwnerService {
  /// Serves `token` on a new thread, keeping at most one region for each of `slots` claim slots, each for at most
  /// `stale_ns`.
  pub(crate) fn start(
    token: OwnerName,
    slots: usize,
    stale_ns: u64,
  ) -> Result<OwnerService, IpcError> {
    let port = token.port();
    let wake_ms = u32::try_from(stale_ns.div_ceil(NS_PER_MS))
      .unwrap_or(u32::MAX)
      .max(1);
    let thread = std::thread::Builder::new()
      .name("slates-rendezvous-owner".to_owned())
      .spawn(move || run(token, Deliveries::new(slots, stale_ns), wake_ms))
      .map_err(|error| IpcError::OsRefused {
        call: "spawn the rendezvous owner's thread",
        code: error.raw_os_error(),
      })?;
    Ok(OwnerService {
      port,
      thread: Some(thread),
    })
  }

  /// Sends the owner's thread a one-word message from this, the owner's, process.
  fn send_word(&self, id: i32, word: u32) -> Result<(), IpcError> {
    let mut message = Word {
      header: Header {
        bits: COPY_SEND,
        size: u32::try_from(size_of::<Word>()).unwrap_or(0),
        remote: self.port,
        local: PORT_NULL,
        voucher: PORT_NULL,
        id,
      },
      word,
    };
    // SAFETY: `message` is a live one-word message to the token's port, for which this process holds a send right
    // beside the receive right its thread holds (`owner_name::receive_right`); the right is copied, not moved; the send
    // waits at most `CONTROL_WAIT_MS`.
    let sent = unsafe {
      mach_msg(
        &mut message.header,
        SEND_MSG | SEND_TIMEOUT,
        message.header.size,
        0,
        PORT_NULL,
        CONTROL_WAIT_MS,
        PORT_NULL,
      )
    };
    if sent != MSG_SUCCESS {
      return Err(IpcError::OsRefused {
        call: "mach_msg (the rendezvous owner's control)",
        code: Some(sent),
      });
    }
    Ok(())
  }
}

impl Drop for OwnerService {
  /// Stops the thread and joins it; the thread then drops the token, and the name is free.
  fn drop(&mut self) {
    if self.send_word(STOP_ID, 0).is_ok()
      && let Some(thread) = self.thread.take()
    {
      let _ = thread.join();
    }
  }
}

/// Format: nanoseconds in a millisecond, a Mach timeout's unit.
const NS_PER_MS: u64 = 1_000_000;
/// Shape: how long the owner's own control message may wait for room in its token's queue, in milliseconds: the
/// thread drains the queue as it fills, so this is reached only when strangers keep it full; a second is the liveness
/// budget a control step is held to elsewhere (§4.7).
const CONTROL_WAIT_MS: u32 = 1_000;

/// The owner's thread: serves the token until its stop, waking at least every `wake_ms` to let stale regions go.
fn run(token: OwnerName, mut deliveries: Deliveries, wake_ms: u32) {
  let own = std::process::id();
  loop {
    let mut message = Received::default();
    let rcv_size = u32::try_from(size_of::<Received>()).unwrap_or(0);
    // SAFETY: `message` is a live, writable buffer of `rcv_size` bytes; the receive is on the port whose receive right
    // `token` holds, waiting at most `wake_ms`; the kernel writes at most `rcv_size` bytes and refuses a larger message
    // (`MACH_RCV_TOO_LARGE`), destroying it.
    let received = unsafe {
      mach_msg(
        &mut message.keep.header,
        RCV_MSG | RCV_TIMEOUT | RCV_TRAILER_AUDIT,
        0,
        rcv_size,
        token.port(),
        wake_ms,
        PORT_NULL,
      )
    };
    if received == RCV_TIMED_OUT {
      deliveries.expire();
      continue;
    }
    if received == RCV_TOO_LARGE {
      continue;
    }
    if received != MSG_SUCCESS {
      // The port is gone or the kernel refuses it: nothing more can be served.
      return;
    }
    let sender = sender_pid(&message);
    let header = message.keep.header;
    let size = usize::try_from(header.size).unwrap_or(0);
    let complex = header.bits & BITS_COMPLEX != 0;
    match header.id {
      FETCH_ID
        if !complex
          && size == size_of::<Word>()
          && header.bits & BITS_REMOTE_MASK == PORT_SEND_ONCE =>
      {
        deliveries.expire();
        let slot = message.keep.count;
        let found = usize::try_from(slot)
          .ok()
          .zip(sender)
          .and_then(|(slot, pid)| deliveries.take(slot, pid));
        answer(header.remote, found);
      }
      KEEP_ID
        if complex
          && size == size_of::<Keep>()
          && message.keep.count == 1
          && message.keep.port.kind == PORT_DESCRIPTOR
          && message.keep.port.disposition == PORT_SEND
          && sender.is_some_and(|pid| pid == own || parent_of(pid) == Some(own)) =>
      {
        let fileport = message.keep.port.name;
        let kept = descriptor_of(fileport).ok().and_then(|fd| {
          usize::try_from(message.keep.slot)
            .ok()
            .and_then(|slot| deliveries.keep(slot, message.keep.pid, fd).ok())
        });
        let _ = kept;
      }
      STOP_ID if !complex && size == size_of::<Word>() && sender == Some(own) => return,
      _ => {
        // SAFETY: `message` holds a message the kernel just delivered whole into this buffer; destroying it gives back
        // every right it carried, once, and nothing reads it after.
        unsafe { mach_msg_destroy(&mut message.keep.header) };
      }
    }
  }
}

/// The process that sent `message`, from the audit trailer the kernel appended at the end of the message proper.
fn sender_pid(message: &Received) -> Option<u32> {
  let size = usize::try_from(message.keep.header.size).ok()?;
  // The trailer starts where the message ends; every message this module reads is a whole number of words.
  let words: &[u8] = bytes_of(message);
  let at = size.checked_add(TRAILER_AUDIT_OFFSET)?;
  let raw = words.get(at..at.checked_add(size_of::<[u32; 8]>())?)?;
  let mut val = [0u32; 8];
  let (words, _) = raw.as_chunks::<{ size_of::<u32>() }>();
  for (word, bytes) in val.iter_mut().zip(words) {
    *word = u32::from_ne_bytes(*bytes);
  }
  // SAFETY: a plain function of the audit token's words, copied out of the trailer the kernel wrote.
  let pid = unsafe { audit_token_to_pid(AuditToken { val }) };
  u32::try_from(pid).ok().filter(|pid| *pid != 0)
}

/// The parent of process `pid`, from the kernel's process table; `None` when the kernel answers nothing for it.
fn parent_of(pid: u32) -> Option<u32> {
  let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::zeroed();
  let size = c_int::try_from(size_of::<libc::proc_bsdinfo>()).ok()?;
  // SAFETY: `info` is a live, writable, zero-initialized `proc_bsdinfo` of `size` bytes; the call writes at most
  // `size` bytes into it and returns how many it wrote.
  let written = unsafe {
    libc::proc_pidinfo(
      c_int::try_from(pid).ok()?,
      libc::PROC_PIDTBSDINFO,
      0,
      info.as_mut_ptr().cast(),
      size,
    )
  };
  if written != size {
    return None;
  }
  // SAFETY: zero-initialized and then written whole by the kernel (`written == size`); every field is a plain integer
  // or byte array, for which any bytes are a value.
  let info = unsafe { info.assume_init() };
  Some(info.pbi_ppid)
}

/// Format: the audit token's offset in `mach_msg_audit_trailer_t`: after its type, size, sequence (4 bytes each) and
/// security token (8).
const TRAILER_AUDIT_OFFSET: usize = 20;

/// The bytes of a received buffer, read by copy for the trailer that may begin inside the room of a larger message.
fn bytes_of(message: &Received) -> &[u8] {
  // SAFETY: `Received` is `repr(C)` of plain integers with no padding (every field a `u32`, `i32` or a pair of a
  // `u16` and two `u8`s completing a word), so each of its bytes is initialized; the slice borrows `message` for its
  // own life and is only read.
  unsafe {
    std::slice::from_raw_parts(
      std::ptr::from_ref(message).cast::<u8>(),
      size_of::<Received>(),
    )
  }
}

/// The descriptor a received fileport names; the fileport's send right is given back.
fn descriptor_of(fileport: MachPort) -> Result<OwnedFd, IpcError> {
  // SAFETY: `fileport` is the send right to a fileport a message moved into this task; making a descriptor from it
  // takes nothing from it, and it is given back just after.
  let fd = unsafe { fileport_makefd(fileport) };
  deallocate(fileport);
  if fd < 0 {
    return Err(IpcError::OsRefused {
      call: "fileport_makefd",
      code: std::io::Error::last_os_error().raw_os_error(),
    });
  }
  // SAFETY: `fileport_makefd` returned a new descriptor that this process owns and nothing else names.
  let fd = unsafe { OwnedFd::from_raw_fd(fd) };
  rustix::io::fcntl_setfd(&fd, rustix::io::FdFlags::CLOEXEC).map_err(|e| IpcError::OsRefused {
    call: "fcntl(FD_CLOEXEC)",
    code: Some(e.raw_os_error()),
  })?;
  Ok(fd)
}

/// Sends the region behind `region` to the owner of the token `owner_name`, kept for claim slot `slot`, claimed by
/// process `claimant`; the owner takes it only from its own process or a child of it (the daemon its anchor spawned).
pub(crate) fn keep(
  owner_name: &str,
  slot: usize,
  claimant: u32,
  region: BorrowedFd<'_>,
) -> Result<(), IpcError> {
  let owner = look_up(owner_name)?.ok_or(IpcError::Layout {
    reason: "no process holds the instance's token to keep the region",
  })?;
  let mut fileport: MachPort = PORT_NULL;
  // SAFETY: `region` is a live descriptor this process holds; `fileport` is a live out-parameter, written only on
  // success.
  let made = unsafe { fileport_makeport(region.as_raw_fd(), &mut fileport) };
  if made != 0 || fileport == PORT_NULL {
    return Err(IpcError::OsRefused {
      call: "fileport_makeport",
      code: std::io::Error::last_os_error().raw_os_error(),
    });
  }
  let mut message = Keep {
    header: Header {
      bits: COPY_SEND | BITS_COMPLEX,
      size: u32::try_from(size_of::<Keep>()).unwrap_or(0),
      remote: owner.port(),
      local: PORT_NULL,
      voucher: PORT_NULL,
      id: KEEP_ID,
    },
    count: 1,
    port: PortDescriptor {
      name: fileport,
      pad1: 0,
      pad2: 0,
      disposition: u8::try_from(MOVE_SEND).unwrap_or(0),
      kind: PORT_DESCRIPTOR,
    },
    slot: u32::try_from(slot).unwrap_or(u32::MAX),
    pid: claimant,
  };
  // SAFETY: `message` is a live complex message to a send right this process was given (copied), whose one descriptor
  // moves the send right to the fileport just made; the send waits at most `CONTROL_WAIT_MS`.
  let sent = unsafe {
    mach_msg(
      &mut message.header,
      SEND_MSG | SEND_TIMEOUT,
      message.header.size,
      0,
      PORT_NULL,
      CONTROL_WAIT_MS,
      PORT_NULL,
    )
  };
  if sent != MSG_SUCCESS {
    // SAFETY: `message` is the message just refused, still naming the rights it was given; destroyed once.
    unsafe { mach_msg_destroy(&mut message.header) };
    return Err(IpcError::OsRefused {
      call: "mach_msg (keep the region)",
      code: Some(sent),
    });
  }
  Ok(())
}

/// Sends the answer to a fetch on its reply right: the region as a fileport when `found`, else the refusal. Whether
/// the region was sent. A claimant gone before its answer leaves the send refused, and the fileport is given back.
fn answer(reply: MachPort, found: Option<OwnedFd>) -> bool {
  let Some(fd) = found else {
    let mut refusal = Header {
      bits: MOVE_SEND_ONCE,
      size: u32::try_from(size_of::<Header>()).unwrap_or(0),
      remote: reply,
      local: PORT_NULL,
      voucher: PORT_NULL,
      id: REFUSED_ID,
    };
    // SAFETY: `refusal` is a live header-only message whose remote port is the send-once right the fetch carried, moved
    // here once; the send has a zero timeout, so it never blocks.
    let sent = unsafe {
      mach_msg(
        &mut refusal,
        SEND_MSG | SEND_TIMEOUT,
        refusal.size,
        0,
        PORT_NULL,
        0,
        PORT_NULL,
      )
    };
    if sent != MSG_SUCCESS {
      // A refused send leaves its rights with this process: destroyed here, so none outlives the refusal.
      // SAFETY: `refusal` is the message just refused, still naming the rights it was given; destroyed once.
      unsafe { mach_msg_destroy(&mut refusal) };
    }
    return false;
  };
  let mut fileport: MachPort = PORT_NULL;
  // SAFETY: `fd` is a live descriptor this process owns; `fileport` is a live out-parameter, written only on success.
  let made = unsafe { fileport_makeport(fd.as_raw_fd(), &mut fileport) };
  drop(fd);
  if made != 0 || fileport == PORT_NULL {
    return answer(reply, None);
  }
  let mut delivered = Delivered {
    header: Header {
      bits: MOVE_SEND_ONCE | BITS_COMPLEX,
      size: u32::try_from(size_of::<Delivered>()).unwrap_or(0),
      remote: reply,
      local: PORT_NULL,
      voucher: PORT_NULL,
      id: DELIVERED_ID,
    },
    count: 1,
    port: PortDescriptor {
      name: fileport,
      pad1: 0,
      pad2: 0,
      disposition: u8::try_from(MOVE_SEND).unwrap_or(0),
      kind: PORT_DESCRIPTOR,
    },
  };
  // SAFETY: `delivered` is a live complex message: its remote port is the fetch's send-once right and its one
  // descriptor moves the send right to the fileport just made, each moved here once; a zero timeout never blocks.
  let sent = unsafe {
    mach_msg(
      &mut delivered.header,
      SEND_MSG | SEND_TIMEOUT,
      delivered.header.size,
      0,
      PORT_NULL,
      0,
      PORT_NULL,
    )
  };
  if sent != MSG_SUCCESS {
    // A refused send leaves its rights with this process, the fileport's among them: destroyed here, so no capability
    // for the region outlives the refusal.
    // SAFETY: `delivered` is the message just refused, still naming the rights it was given; destroyed once.
    unsafe { mach_msg_destroy(&mut delivered.header) };
    return false;
  }
  true
}

/// Fetches the region kept for claim slot `slot` from the owner of the token `owner_name`, waiting up to
/// `timeout_ms` for the answer; the region's descriptor. Refused `DaemonUnavailable` when no owner holds the token, the
/// owner keeps nothing for this claim, or no answer comes in time.
pub(crate) fn fetch(
  instance: &str,
  owner_name: &str,
  slot: usize,
  timeout_ms: u32,
) -> Result<OwnedFd, IpcError> {
  let unavailable = |why: &'static str| IpcError::DaemonUnavailable {
    endpoint: instance.to_owned(),
    why,
  };
  let owner =
    look_up(owner_name)?.ok_or_else(|| unavailable("no process holds the instance's token"))?;
  let reply = ReplyPort::new()?;
  let mut message = Word {
    header: Header {
      bits: COPY_SEND | (MAKE_SEND_ONCE << LOCAL_SHIFT),
      size: u32::try_from(size_of::<Word>()).unwrap_or(0),
      remote: owner.port(),
      local: reply.port,
      voucher: PORT_NULL,
      id: FETCH_ID,
    },
    word: u32::try_from(slot).unwrap_or(u32::MAX),
  };
  // SAFETY: `message` is a live fetch: its remote port is a send right this process was given (copied, not moved), its
  // local port a receive right this process holds, from which a send-once right is made; the send waits at most
  // `timeout_ms`.
  let sent = unsafe {
    mach_msg(
      &mut message.header,
      SEND_MSG | SEND_TIMEOUT,
      message.header.size,
      0,
      PORT_NULL,
      timeout_ms,
      PORT_NULL,
    )
  };
  if sent != MSG_SUCCESS {
    return Err(unavailable(
      "the instance's owner did not take the region's fetch",
    ));
  }
  let mut answer = DeliveredReceived::default();
  let rcv_size = u32::try_from(size_of::<DeliveredReceived>()).unwrap_or(0);
  // SAFETY: `answer` is a live, writable buffer of `rcv_size` bytes; the receive is on the reply port whose receive
  // right this process holds, waiting at most `timeout_ms`; a larger message is refused and destroyed by the kernel.
  let received = unsafe {
    mach_msg(
      &mut answer.delivered.header,
      RCV_MSG | RCV_TIMEOUT,
      0,
      rcv_size,
      reply.port,
      timeout_ms,
      PORT_NULL,
    )
  };
  if received != MSG_SUCCESS {
    return Err(unavailable(
      "the instance's owner did not answer the region's fetch in time",
    ));
  }
  let delivered = answer.delivered;
  let well_formed = delivered.header.id == DELIVERED_ID
    && delivered.header.bits & BITS_COMPLEX != 0
    && usize::try_from(delivered.header.size).ok() == Some(size_of::<Delivered>())
    && delivered.count == 1
    && delivered.port.kind == PORT_DESCRIPTOR
    && delivered.port.name != PORT_NULL;
  if !well_formed {
    // SAFETY: `answer` holds a message the kernel just delivered whole into this buffer; destroying it gives back
    // every right it carried, once.
    unsafe { mach_msg_destroy(&mut answer.delivered.header) };
    return Err(unavailable(
      "the instance's owner keeps no region for this claim",
    ));
  }
  descriptor_of(delivered.port.name)
}

/// A receive right for one answer, given back when dropped.
struct ReplyPort {
  port: MachPort,
}

impl ReplyPort {
  fn new() -> Result<ReplyPort, IpcError> {
    let mut port: MachPort = PORT_NULL;
    // SAFETY: `mach_task_self_` is this task's own port, set by libSystem before `main` and never changed; `port` is a
    // live out-parameter, written only on success.
    let allocated = unsafe { mach_port_allocate(mach_task_self_, RIGHT_RECEIVE, &mut port) };
    if allocated != 0 {
      return Err(IpcError::OsRefused {
        call: "mach_port_allocate",
        code: Some(allocated),
      });
    }
    Ok(ReplyPort { port })
  }
}

impl Drop for ReplyPort {
  fn drop(&mut self) {
    // SAFETY: `mach_task_self_` as above; `port` names the receive right this value allocated and gives back once.
    unsafe {
      mach_port_mod_refs(mach_task_self_, self.port, RIGHT_RECEIVE, -1);
    }
  }
}

/// Gives back one send right this process holds.
fn deallocate(port: MachPort) {
  // SAFETY: `mach_task_self_` as above; `port` names a send right this process holds and gives back here once.
  unsafe {
    mach_port_deallocate(mach_task_self_, port);
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use slates_mem::{SharedObject, Words};

  /// Format: the variable that runs this test binary as a stranger: the owner token's name it plants a region under.
  const STRANGER_ENV: &str = "SLATES_TEST_MACH_STRANGER";
  /// Format: the variable naming the process the stranger forges its claim for.
  const VICTIM_ENV: &str = "SLATES_TEST_MACH_VICTIM";
  /// Shape: a region's bytes in these tests, a page.
  const REGION_BYTES: usize = 4096;
  /// Shape: claim slots in these tests.
  const SLOTS: usize = 4;
  /// Shape: how long a delivery may wait in these tests: past any test's own wait, in nanoseconds (a minute).
  const STALE_NS: u64 = 60_000_000_000;
  /// Shape: a fetch's wait in these tests, in milliseconds.
  const WAIT_MS: u32 = 2_000;
  /// Format: the byte a planted region carries, so a fetch that returns it shows the plant took.
  const PLANTED: u8 = 0x5a;

  fn token(tag: &str) -> (String, OwnerService) {
    let name = format!("com.slates.test.mach-claim.{tag}.{}", std::process::id());
    let held = crate::owner_name::claim(&name)
      .unwrap()
      .expect("the test's token is free");
    (name, OwnerService::start(held, SLOTS, STALE_NS).unwrap())
  }

  fn region(tag: &str, fill: u8) -> SharedObject {
    let mut object = SharedObject::create(
      &format!("mc-{tag}-{}", std::process::id()),
      REGION_BYTES,
      Words::new(),
    )
    .unwrap();
    object.write(0, &[fill; 16]).unwrap();
    object.unlink_name().unwrap();
    object
  }

  /// GAP-A9-9. Do: keep a region for slot 1 claimed by this process, fetch slot 1, then fetch it again. Expect: the
  /// first fetch maps the very bytes kept (the descriptor crossed as a fileport), and the second gets nothing, since a
  /// delivery is handed over once.
  #[test]
  fn a_kept_region_is_fetched_once_by_its_claimant() {
    let (name, _service) = token("once");
    let kept = region("once", 7);
    keep(&name, 1, std::process::id(), kept.as_fd()).unwrap();
    let fd = fetch("test", &name, 1, WAIT_MS).unwrap();
    let fetched = SharedObject::from_descriptor(fd, REGION_BYTES, Words::new()).unwrap();
    let mut first = [0u8; 16];
    fetched.read(0, &mut first).unwrap();
    assert_eq!(first, [7u8; 16], "the fetched region is the kept one");
    assert!(
      fetch("test", &name, 1, WAIT_MS).is_err(),
      "a delivery is handed over once"
    );
  }

  /// GAP-A9-9. Do: keep a region for slot 2 claimed by another process (this pid plus one), and fetch slot 2 as this
  /// process. Expect: nothing: a region goes only to the process that claimed it, by the kernel's audit token.
  #[test]
  fn a_region_kept_for_another_process_is_refused_to_this_one() {
    let (name, _service) = token("other");
    let kept = region("other", 9);
    keep(&name, 2, std::process::id() + 1, kept.as_fd()).unwrap();
    assert!(fetch("test", &name, 2, WAIT_MS).is_err());
  }

  /// GAP-A9-9 (the keep's sender). Do: from a stranger, a process that is neither the token's holder nor its child (this
  /// test binary run again under a shell that forks it, so its parent is the shell), keep a region of its own for slot 0
  /// as if this process had claimed it; then fetch slot 0. Expect: the stranger's keep was sent, and the fetch gets
  /// nothing: the holder took no region from it, so a stranger cannot plant one for a client to fetch.
  #[test]
  fn a_stranger_cannot_plant_a_region_for_a_client() {
    let (name, _service) = token("plant");
    let exe = std::env::current_exe().unwrap();
    let status = std::process::Command::new("sh")
      .arg("-c")
      .arg(format!(
        "'{}' --exact mach_claim::tests::stranger_role --nocapture; status=$?; exit $status",
        exe.display()
      ))
      .env(STRANGER_ENV, &name)
      .env(VICTIM_ENV, std::process::id().to_string())
      .status()
      .unwrap();
    assert!(status.success(), "the stranger ran and sent its keep");
    let fetched = fetch("test", &name, 0, WAIT_MS);
    assert!(
      fetched.is_err(),
      "a region the stranger kept was handed to its victim"
    );
  }

  /// The stranger of `a_stranger_cannot_plant_a_region_for_a_client`: does nothing unless run as one.
  #[test]
  fn stranger_role() {
    let (Ok(name), Ok(victim)) = (std::env::var(STRANGER_ENV), std::env::var(VICTIM_ENV)) else {
      return;
    };
    let planted = region("planted", PLANTED);
    keep(&name, 0, victim.parse().unwrap(), planted.as_fd()).unwrap();
  }
}
