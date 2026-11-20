//! The relief receiver (B3, A-725; §4.6 Windows; amends A-198's single receiver). WinFsp completes an answered request
//! on the thread that sent the answer (WinFsp v2.1 `src/sys/volume.c` 1089-1102), and some completions wait on the
//! drive: a shrink's SetInformation completion runs the cache manager's `CcSetFileSizes` there (`src/sys/fileinfo.c`
//! 1407, `src/sys/file.c` 1852), which can wait on a paging request for the file's last page. With one receiver, that
//! request reached no one and the drive stopped: fsx seed 1 over the drive, every run, on both Windows architectures
//! (shown 2026-11-20 by a send that never returned, `docs/bugs/2026-11-05-winfstest-found-win32-semantics-the-drive-broke.md`).
//!
//! The fix keeps A-198's one operating thread and adds a second receiver that works only while the first is held in a
//! send. Each receiver has its own retained journal region ([`super::capture`]), and the state an operation needs (the
//! DLL's operation table, the private opens, the confirmation in flight; [`Station`]) belongs to one of them at a time,
//! moved, never shared (R2, D-7). The primary parks the station in a one-pointer slot ([`Hand`]) before each send and
//! takes it back after: two atomic exchanges a send, and no wake of anyone. The relief looks once a beat: when the
//! primary has been inside the same send for a whole beat, it takes the station from the slot, receives into its own
//! region and serves, until the primary, its send returned, asks for the station back. So no two operations ever run
//! at once, the shard channel's bound and the one outstanding confirmation are unchanged (the confirmation travels with
//! the station), and recovery settles both regions before any receive.
//!
//! Residual: a relief held in a send of its own is relieved by no one; the drive then stops as it did before, which
//! needs two completions that each wait on the drive at once.

use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU64, Ordering};
use std::thread::Thread;
use std::time::Duration;

use super::capture::Capture;
use super::settle::{self, Edges, Settled};
use super::{Refusal, Request, Scratch};

/// What one receiving thread does with the driver: receive into its own region, and send.
pub trait Port<E> {
  /// Receives the next batch into `buffer`; `None` when the owner is stopping.
  fn receive(&mut self, buffer: &mut [u8]) -> Result<Option<usize>, E>;
  /// Hands one complete, saved response to the driver. WinFsp completes it on this thread before this returns.
  fn send(&mut self, response: &[u8]) -> Result<(), E>;
  /// Runs once on the receiving thread before it serves (the reply spin window is learned per thread, A-560).
  fn started(&mut self) {}
}

/// The state an operation needs, moved between the receivers and never shared: [`settle::Edges`]' shard half.
pub trait Station<E> {
  /// Runs `request`'s operation into `response` and answers its length.
  fn operate(
    &mut self,
    request: &Request<'_>,
    response: &mut [u8],
    replaying: bool,
  ) -> Result<usize, E>;
  /// Tells the shard the driver accepted the open `context` names.
  fn post_confirm(&mut self, context: u64) -> Result<(), E>;
  /// Answers once the shard has applied the confirmation last posted.
  fn confirmed(&mut self) -> Result<(), E>;
  /// Finishes a successful rename's durable name updates.
  fn rename_open_names(&mut self, from: &str, to: &str) -> Result<(), E>;
}

/// A receiver's retained journal region.
pub trait Region {
  /// The region, checked, for one receive or recovery.
  fn capture(&mut self) -> Result<Capture<'_>, Refusal>;
}

/// The two receivers' meeting point: the slot the primary parks the station in while it sends, the slot the relief
/// gives it back by, the primary's send count (odd while inside a send), and the two flags.
pub struct Hand<S> {
  lent: AtomicPtr<S>,
  returned: AtomicPtr<S>,
  sends: AtomicU64,
  reclaim: AtomicBool,
  stopping: AtomicBool,
  relieved: AtomicU64,
}

impl<S> Default for Hand<S> {
  fn default() -> Self {
    Self {
      lent: AtomicPtr::new(std::ptr::null_mut()),
      returned: AtomicPtr::new(std::ptr::null_mut()),
      sends: AtomicU64::new(0),
      reclaim: AtomicBool::new(false),
      stopping: AtomicBool::new(false),
      relieved: AtomicU64::new(0),
    }
  }
}

impl<S> Hand<S> {
  /// How many times the relief took the station from a held send (a non-vacuity counter).
  pub fn relieved(&self) -> u64 {
    self.relieved.load(Ordering::Relaxed)
  }
}

impl<S> Drop for Hand<S> {
  fn drop(&mut self) {
    drop(take(&self.lent));
    drop(take(&self.returned));
  }
}

/// Puts `station` in `slot`, which must be empty (only its one holder puts, and it took what it put before).
fn put<S>(slot: &AtomicPtr<S>, station: Box<S>) -> Result<(), Refusal> {
  let previous = slot.swap(Box::into_raw(station), Ordering::AcqRel);
  if previous.is_null() {
    Ok(())
  } else {
    drop(take_pointer(previous));
    Err(Refusal::State)
  }
}

/// Takes what `slot` holds, if anything.
fn take<S>(slot: &AtomicPtr<S>) -> Option<Box<S>> {
  take_pointer(slot.swap(std::ptr::null_mut(), Ordering::AcqRel))
}

fn take_pointer<S>(pointer: *mut S) -> Option<Box<S>> {
  if pointer.is_null() {
    return None;
  }
  // SAFETY: every non-null pointer in a slot came from `Box::into_raw` in `put`, and the swap that read it out left
  // the slot empty, so this is its only owner.
  Some(unsafe { Box::from_raw(pointer) })
}

/// [`settle::Edges`] over one receiver's port and the station while this receiver holds it. The primary lends the
/// station for the length of each send; the relief sends holding it.
struct Relayed<'h, P, S, E> {
  port: P,
  station: Option<Box<S>>,
  hand: &'h Hand<S>,
  lends: bool,
  error: std::marker::PhantomData<fn() -> E>,
}

impl<P, S, E: From<Refusal>> Relayed<'_, P, S, E> {
  fn station(&mut self) -> Result<&mut S, E> {
    self.station.as_deref_mut().ok_or(E::from(Refusal::State))
  }

  /// The station back after a send: from the slot, unless the relief took it; then once the relief gives it back.
  fn take_back(&mut self) -> Box<S> {
    if let Some(station) = take(&self.hand.lent) {
      return station;
    }
    self.hand.reclaim.store(true, Ordering::Release);
    loop {
      if let Some(station) = take(&self.hand.returned) {
        return station;
      }
      std::thread::park();
    }
  }
}

impl<E, P, S> Edges for Relayed<'_, P, S, E>
where
  E: From<Refusal>,
  P: Port<E>,
  S: Station<E>,
{
  type Error = E;

  fn receive(&mut self, buffer: &mut [u8]) -> Result<Option<usize>, E> {
    self.port.receive(buffer)
  }

  fn send(&mut self, response: &[u8]) -> Result<(), E> {
    if !self.lends {
      return self.port.send(response);
    }
    let station = self.station.take().ok_or(E::from(Refusal::State))?;
    self.hand.sends.fetch_add(1, Ordering::AcqRel);
    put(&self.hand.lent, station)?;
    let sent = self.port.send(response);
    self.hand.sends.fetch_add(1, Ordering::AcqRel);
    self.station = Some(self.take_back());
    sent
  }

  fn operate(
    &mut self,
    request: &Request<'_>,
    response: &mut [u8],
    replaying: bool,
  ) -> Result<usize, E> {
    self.station()?.operate(request, response, replaying)
  }

  fn post_confirm(&mut self, context: u64) -> Result<(), E> {
    self.station()?.post_confirm(context)
  }

  fn confirmed(&mut self) -> Result<(), E> {
    self.station()?.confirmed()
  }

  fn rename_open_names(&mut self, from: &str, to: &str) -> Result<(), E> {
    self.station()?.rename_open_names(from, to)
  }
}

/// One receiving thread's own things: its port, its journal region, its frame index and its response packet.
pub struct Desk<'r, P, R> {
  /// The thread's transactions with the driver.
  pub port: P,
  /// The thread's retained journal region.
  pub region: &'r mut R,
  /// The thread's batch frame index.
  pub scratch: Scratch,
  /// The thread's response packet, `super::RESPONSE_MAX` bytes.
  pub response: &'r mut [u8],
}

/// Serves the drive with a primary receiver and a relief (module docs) until the primary's port reports the stop.
/// Recovery settles the primary's region, then the relief's, before any receive, and `recovered` hears what both came
/// to. `beat` is how long the primary must be inside one send before the relief takes over.
pub fn serve<E, P, S, R>(
  hand: &Hand<S>,
  (primary, relief): (Desk<'_, P, R>, Desk<'_, P, R>),
  station: Box<S>,
  beat: Duration,
  recovered: impl FnOnce(Settled) -> Result<(), E>,
) -> Result<(), E>
where
  E: From<Refusal> + Send,
  P: Port<E> + Send,
  S: Station<E> + Send,
  R: Region + Send,
{
  let Desk {
    port,
    region,
    mut scratch,
    response,
  } = primary;
  let mut edges = Relayed {
    port,
    station: Some(station),
    hand,
    lends: true,
    error: std::marker::PhantomData,
  };
  edges.port.started();
  // Both regions are settled before either receiver receives: a dead owner may have left a batch in each.
  let mut settled = Settled::default();
  for region in [&mut *region, &mut *relief.region] {
    if let Some(own) = settle::recover(region.capture()?, &mut scratch, &mut edges, response)? {
      settled.answered = settled.answered.saturating_add(own.answered);
      settled.refused = settled.refused.saturating_add(own.refused);
    }
  }
  recovered(settled)?;
  std::thread::scope(|scope| {
    let primary_thread = std::thread::current();
    let relief_thread = std::thread::Builder::new()
      .name("slates-winfsp-relief".to_owned())
      .spawn_scoped(scope, move || relieve(hand, relief, beat, &primary_thread))
      .map_err(|_| E::from(Refusal::State))?;
    let served = receive_until_stopped(&mut edges, region, &mut scratch, response);
    hand.stopping.store(true, Ordering::Release);
    relief_thread.thread().unpark();
    let relieved = relief_thread.join().unwrap_or(Err(E::from(Refusal::State)));
    served.and(relieved)
  })
}

/// The primary's receives into its own region until its port reports the stop.
fn receive_until_stopped<E, P, S, R>(
  edges: &mut Relayed<'_, P, S, E>,
  region: &mut R,
  scratch: &mut Scratch,
  response: &mut [u8],
) -> Result<(), E>
where
  E: From<Refusal>,
  P: Port<E>,
  S: Station<E>,
  R: Region,
{
  let mut used = None;
  loop {
    let receiving = region.capture()?.begin(used)?;
    match settle::receive(receiving, scratch, edges, response)? {
      Some(filled) => used = Some(filled),
      None => return Ok(()),
    }
  }
}

fn relieve<E, P, S, R>(
  hand: &Hand<S>,
  desk: Desk<'_, P, R>,
  beat: Duration,
  primary: &Thread,
) -> Result<(), E>
where
  E: From<Refusal>,
  P: Port<E>,
  S: Station<E>,
  R: Region,
{
  let Desk {
    port,
    region,
    mut scratch,
    response,
  } = desk;
  let mut edges = Relayed {
    port,
    station: None,
    hand,
    lends: false,
    error: std::marker::PhantomData,
  };
  edges.port.started();
  let mut used = None;
  let mut seen = 0;
  loop {
    if hand.stopping.load(Ordering::Acquire) {
      return Ok(());
    }
    std::thread::park_timeout(beat);
    let now = hand.sends.load(Ordering::Acquire);
    let held = now % 2 == 1 && now == seen;
    seen = now;
    if !held {
      continue;
    }
    let Some(station) = take(&hand.lent) else {
      continue;
    };
    hand.relieved.fetch_add(1, Ordering::Relaxed);
    edges.station = Some(station);
    let served = loop {
      if hand.reclaim.load(Ordering::Acquire) || hand.stopping.load(Ordering::Acquire) {
        break Ok(());
      }
      let receiving = match region.capture().and_then(|capture| capture.begin(used)) {
        Ok(receiving) => receiving,
        Err(refusal) => break Err(E::from(refusal)),
      };
      match settle::receive(receiving, &mut scratch, &mut edges, response) {
        Ok(Some(filled)) => used = Some(filled),
        Ok(None) => break Ok(()),
        Err(error) => break Err(error),
      }
    };
    hand.reclaim.store(false, Ordering::Release);
    if let Some(station) = edges.station.take() {
      put(&hand.returned, station)?;
    }
    primary.unpark();
    served?;
  }
}

#[cfg(test)]
mod tests;
