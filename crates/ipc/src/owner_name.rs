//! The owner's token on macOS (§4.7, A-176; GAPS "macOS takes a live owner's rendezvous over"): a Mach bootstrap name
//! that launchd gives to its first registrant and takes back when that registrant's port dies, so at most one live
//! process owns an instance's rendezvous, as the named Event token makes it on Windows (`rendezvous.rs` `Owner`) and the
//! abstract address on Linux.
//!
//! **Why a bootstrap name.** The rendezvous's bootstrap object is a POSIX shared-memory object: its name lives until it
//! is unlinked, and no lock can be taken on it (measured 2026-10-26 on macOS 26: `flock` on its descriptor answers
//! `ENOTSUP`, `fcntl` record locks `EBADF`). Without a token, an owner unlinked whatever object held the name, so a
//! second anchor, or a second daemon run alone, took a live instance's rendezvous over, and the two served one instance
//! with separate state. A bootstrap name ends with its holder and touches no disk. Measured unprivileged, 2026-10-26:
//! - while the first holder lives, a second process's registration is refused (1100) and a second registration from
//!   the same process too (1101), and `bootstrap_look_up` finds the holder;
//! - the name is free the moment its holder's receive right is gone: 200 of 200 registrations made right after a
//!   holder dropped its right were accepted at once (the worst 0.105 ms), and 20 of 20 made right after a holder killed
//!   with `SIGKILL` was reaped (the worst call 10.25 ms, none refused).
//!
//! **Where.** At the top of the caller's bootstrap chain, the namespace that is its own parent: the widest one this
//! user's processes register in, so the user's every login session (a GUI one, an ssh one) meets the same token, as they
//! meet the same shared-memory names. Measured: a chain three deep from a GUI session, and a registration accepted at
//! every level of it. The name carries the user's id, so two users' instances never meet.
//!
//! **Cost.** `bootstrap_register` is deprecated in the SDK's headers and nothing wraps these calls safely: they are the
//! crate's Mach FFI, each with its invariant (`unsafe-budget.toml`).

use std::ffi::{CString, c_char, c_int, c_uint};

use crate::error::IpcError;

/// Format: `kern_return_t` (`<mach/kern_return.h>`).
type KernReturn = c_int;
/// Format: a port name in this task's space (`mach_port_t`, `<mach/port.h>`).
type MachPort = libc::mach_port_t;

/// Format: `KERN_SUCCESS` (`<mach/kern_return.h>`).
const KERN_SUCCESS: KernReturn = 0;
/// Format: `MACH_PORT_NULL` (`<mach/port.h>`).
const PORT_NULL: MachPort = 0;
/// Format: `MACH_PORT_RIGHT_SEND` (`<mach/port.h>`).
const RIGHT_SEND: c_uint = 0;
/// Format: `MACH_PORT_RIGHT_RECEIVE` (`<mach/port.h>`).
const RIGHT_RECEIVE: c_uint = 1;
/// Format: `MACH_MSG_TYPE_MAKE_SEND` (`<mach/message.h>`): a send right made from the receive right this task holds.
const MAKE_SEND: c_uint = 20;
/// Format: the bytes of a bootstrap name, its terminating zero included (`name_t`, `<servers/bootstrap.h>`).
const NAME_BYTES: usize = 128;
/// Shape: the most namespaces the walk to the top of the chain passes. launchd nests the system, user, GUI-session and
/// login domains, four; twice that ends a chain that never reaches a namespace that is its own parent.
const CHAIN_DEPTH: usize = 8;

unsafe extern "C" {
  /// This process's bootstrap port, set by libSystem before `main` (`<servers/bootstrap.h>`).
  static bootstrap_port: MachPort;
  /// This task's own port, set by libSystem before `main`; what the C macro `mach_task_self()` reads (`<mach/mach_init.h>`).
  static mach_task_self_: MachPort;
  fn bootstrap_parent(bp: MachPort, parent_port: *mut MachPort) -> KernReturn;
  fn bootstrap_register(bp: MachPort, service_name: *const c_char, sp: MachPort) -> KernReturn;
  fn bootstrap_look_up(bp: MachPort, service_name: *const c_char, sp: *mut MachPort) -> KernReturn;
  fn mach_port_allocate(task: MachPort, right: c_uint, name: *mut MachPort) -> KernReturn;
  fn mach_port_insert_right(
    task: MachPort,
    name: MachPort,
    poly: MachPort,
    poly_poly: c_uint,
  ) -> KernReturn;
  fn mach_port_mod_refs(task: MachPort, name: MachPort, right: c_uint, delta: c_int) -> KernReturn;
  fn mach_port_deallocate(task: MachPort, name: MachPort) -> KernReturn;
}

/// A held owner's token: the port whose receive right is the registration's life. Dropping it frees the name.
pub(crate) struct OwnerName {
  port: MachPort,
}

impl Drop for OwnerName {
  fn drop(&mut self) {
    destroy(self.port, true);
  }
}

/// Claims the token `name` at the top of this process's bootstrap chain: `Ok(Some)` while this process holds it,
/// `Ok(None)` when a live process holds it already, and an error when the bootstrap server refuses for another reason.
pub(crate) fn claim(name: &str) -> Result<Option<OwnerName>, IpcError> {
  let name = token_name(name)?;
  let top = top_namespace()?;
  let port = receive_right()?;
  // SAFETY: `top.port` is a send right to a bootstrap namespace this process holds for the call; `name` is a live
  // zero-terminated string shorter than `NAME_BYTES`; `port` names a receive right this process made, with a send
  // right beside it, which the server copies.
  let registered = unsafe { bootstrap_register(top.port, name.as_ptr(), port) };
  if registered == KERN_SUCCESS {
    return Ok(Some(OwnerName { port }));
  }
  destroy(port, true);
  if held_by_another(&top, &name) {
    return Ok(None);
  }
  Err(IpcError::OsRefused {
    call: "bootstrap_register",
    code: Some(registered),
  })
}

/// `name` as a bootstrap name: zero-terminated and within `NAME_BYTES`, else refused.
fn token_name(name: &str) -> Result<CString, IpcError> {
  if name.len() >= NAME_BYTES {
    return Err(IpcError::OsRefused {
      call: "bootstrap name longer than name_t",
      code: None,
    });
  }
  CString::new(name).map_err(|_| IpcError::OsRefused {
    call: "bootstrap name with a zero byte",
    code: None,
  })
}

/// Whether a live process holds `name` in `top`: the server finds a registration for it. A refusal to register that
/// finds none is another refusal, not a holder.
fn held_by_another(top: &Namespace, name: &CString) -> bool {
  let mut found: MachPort = PORT_NULL;
  // SAFETY: `top.port` is a send right this process holds; `name` is a live zero-terminated string; `found` is a live
  // out-parameter, written only on success.
  let looked = unsafe { bootstrap_look_up(top.port, name.as_ptr(), &mut found) };
  if looked != KERN_SUCCESS {
    return false;
  }
  deallocate_send(found);
  true
}

/// A send right to a bootstrap namespace: this process's own, kept, or one `bootstrap_parent` gave, given back when
/// dropped.
struct Namespace {
  port: MachPort,
  given: bool,
}

impl Drop for Namespace {
  fn drop(&mut self) {
    if self.given {
      deallocate_send(self.port);
    }
  }
}

/// The top of this process's bootstrap chain: the namespace that is its own parent. Every send right the walk is given
/// on the way is given back.
fn top_namespace() -> Result<Namespace, IpcError> {
  // SAFETY: libSystem sets `bootstrap_port` before `main`, and nothing in slates sets it again; a plain read of a
  // port name.
  let own = unsafe { bootstrap_port };
  let mut current = Namespace {
    port: own,
    given: false,
  };
  for _ in 0..CHAIN_DEPTH {
    let mut parent: MachPort = PORT_NULL;
    // SAFETY: `current.port` is a send right this process holds (its own bootstrap port, or one a previous turn was
    // given); `parent` is a live out-parameter, written only on success.
    let asked = unsafe { bootstrap_parent(current.port, &mut parent) };
    if asked != KERN_SUCCESS || parent == PORT_NULL {
      return Ok(current);
    }
    if parent == current.port {
      // The top: the server gave one more reference to the right this process already holds.
      deallocate_send(parent);
      return Ok(current);
    }
    current = Namespace {
      port: parent,
      given: true,
    };
  }
  Err(IpcError::OsRefused {
    call: "bootstrap_parent: no namespace is its own parent",
    code: None,
  })
}

/// A new port: its receive right and one send right, both this process's.
fn receive_right() -> Result<MachPort, IpcError> {
  let mut port: MachPort = PORT_NULL;
  // SAFETY: `mach_task_self_` is this task's own port, which libSystem sets before `main` and nothing changes after,
  // so the read is plain; `port` is a live out-parameter, written only on success.
  let allocated = unsafe { mach_port_allocate(mach_task_self_, RIGHT_RECEIVE, &mut port) };
  if allocated != KERN_SUCCESS {
    return Err(IpcError::OsRefused {
      call: "mach_port_allocate",
      code: Some(allocated),
    });
  }
  // SAFETY: `mach_task_self_` as above; the send right is made from the receive right just allocated, under the name
  // the allocation returned.
  let inserted = unsafe { mach_port_insert_right(mach_task_self_, port, port, MAKE_SEND) };
  if inserted != KERN_SUCCESS {
    destroy(port, false);
    return Err(IpcError::OsRefused {
      call: "mach_port_insert_right",
      code: Some(inserted),
    });
  }
  Ok(port)
}

/// Gives back `port`'s receive right, and its send right when `with_send`: the port dies, and a bootstrap name
/// registered on it is free. A refusal here would mean a right was already gone, which this module never causes;
/// nothing to recover.
fn destroy(port: MachPort, with_send: bool) {
  // SAFETY: `mach_task_self_` is this task's own port, set before `main` and never changed; `port` names a port whose
  // receive right (and, when `with_send`, one send right) this process made (`receive_right`) and gives back here
  // once; the calls take no memory.
  unsafe {
    let task = mach_task_self_;
    if with_send {
      mach_port_mod_refs(task, port, RIGHT_SEND, -1);
    }
    mach_port_mod_refs(task, port, RIGHT_RECEIVE, -1);
  }
}

/// Gives back one reference to a send right this process was given. A refusal would mean the right was already gone;
/// nothing to recover.
fn deallocate_send(port: MachPort) {
  // SAFETY: `mach_task_self_` is this task's own port, set before `main` and never changed; `port` names a send right
  // this process was given (a namespace or a looked-up registration) and gives back here once; the call takes no
  // memory.
  unsafe {
    mach_port_deallocate(mach_task_self_, port);
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  /// GAPS "macOS takes a live owner's rendezvous over". Do: claim a token, claim it again from this process, drop the
  /// first, claim it a third time. Expect: the second claim finds it held, and the third holds it, at once.
  #[test]
  fn a_token_is_held_until_its_holder_drops_it() {
    let name = format!("com.slates.test.owner-name.{}", std::process::id());
    let first = claim(&name).unwrap();
    assert!(first.is_some(), "the first claim holds the token");
    assert!(
      claim(&name).unwrap().is_none(),
      "a second claim finds it held"
    );
    drop(first);
    assert!(
      claim(&name).unwrap().is_some(),
      "the token is free once its holder drops it"
    );
  }

  /// Hostile input. Do: claim names past a bootstrap name's bytes and with a zero byte. Expect: each refused typed,
  /// none sent to the server.
  #[test]
  fn a_name_no_bootstrap_name_can_hold_is_refused() {
    let long = "n".repeat(NAME_BYTES);
    assert!(matches!(claim(&long), Err(IpcError::OsRefused { .. })));
    assert!(matches!(
      claim("com.slates.test\0zero"),
      Err(IpcError::OsRefused { .. })
    ));
  }
}
