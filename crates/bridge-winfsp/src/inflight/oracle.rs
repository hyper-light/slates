//! The receive protocol's crash oracle (A-198; audit step 7). The receive worker's protocol
//! ([`super::settle`]) runs over a modelled driver and a modelled owner shard, against the real receive journal
//! ([`super::capture`]) and the real open ledger ([`crate::opens::Ledger`]). A reference run records, in program order,
//! every store to held memory (the journal's and the ledger's) and every change of the driver's state. One fresh
//! scenario per recorded store then crashes the process there, and once more inside each store of more than one byte,
//! torn at its middle; the successor recovers and serves to quiescence, and so does a third owner after a second crash at
//! every store of that recovery. After each, the driver's view must agree with the ledger's.
//!
//! The model states the design's rules (A-198, §4.6 Windows), not this implementation's:
//! - the driver completes only a request it has in processing, by its hint; a response for any other hint is ignored, and
//!   a hint is reused for the next request as soon as its request completes (WinFsp's IRP address, sys/ioq.c);
//! - every request delivered is answered exactly once, and only with the response made for it or a cancellation;
//! - a file the driver opened stays open until its Close completes, whatever the Close answered, and the ledger holds a
//!   live context for exactly the files the driver holds open: none lost, none leaked;
//! - when the driver is quiet, the journal holds no unsettled batch and the ledger no unconfirmed open.
//!
//! Recording is the only test hook in production code: the journal tells [`journal_stored`] of each store.
//!
//! Red-checked 2026-11-07 against three protocol bugs, each put in by hand and caught: confirming an open after its
//! acknowledgement instead of before (a death between them retires an open the driver holds: "file 2's context ... was
//! lost at recovery"), a receive that keeps the previous saved response (a recycled hint's batch is refused), and a
//! recovery that cancels a saved Create instead of replaying it (the ledger and the driver disagree). The workload opens,
//! reads, cleans up and closes three files at once; it renames nothing, so a rename's durable name update is not covered
//! here.

#![allow(
  clippy::unwrap_used,
  clippy::unwrap_in_result,
  clippy::expect_used,
  clippy::panic,
  clippy::indexing_slicing
)]

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, VecDeque};

use slates_bridge_core::ObjectId;

use super::capture::Capture;
use super::settle::{self, Edges};
use super::{
  CANCELLED, CLEANUP_KIND, CLOSE_KIND, CREATE_KIND, HINT_AT, KIND_AT, OPEN_CONTEXT_AT,
  REQUEST_BASE, RESPONSE_BASE, RESPONSE_MAX, Refusal, Request, SIZE_AT, STATUS_AT, Scratch,
};
use crate::opens::{Ledger, Memory, Open};

/// Shape: the open ledger's slots, more than the files the workload holds open at once, so admission never refuses.
const BOUND: usize = 8;
/// Shape: the files the workload opens at once, so a batch carries several requests and a saved response can sit in its
/// middle.
const FILES: usize = 3;
/// Format: FspFsctlTransactReadKind (fsctl.h), a request whose response may name the dead process's buffer.
const READ_KIND: u32 = 5;
/// Format: Req.Cleanup.UserContext2 and Req.Close.UserContext2 follow the header and the UserContext word (fsctl.h).
const REQUEST_CONTEXT_AT: usize = HINT_AT + size_of::<u64>() + size_of::<u64>();
/// Shape: where the model writes a request's serial, inside the request union and before its file name descriptor; the
/// protocol never reads it.
const SERIAL_AT: usize = REQUEST_BASE - size_of::<u64>() * 3;
/// Shape: the model's first IRP address, and the distance between fresh ones; freed addresses are reused first.
const FIRST_HINT: u64 = 0x1000;
/// Shape: see [`FIRST_HINT`].
const HINT_STRIDE: u64 = 0x100;
/// Format: STATUS_DISK_FULL (ntstatus.h), the model shard's answer to an open the ledger has no room for.
const STATUS_DISK_FULL: u32 = 0xC000_007F;
/// Format: STATUS_INVALID_HANDLE (ntstatus.h), the model shard's answer to a context the ledger refuses.
const STATUS_INVALID_HANDLE: u32 = 0xC000_0008;
/// Shape: each file's requests in the workload: its Create, a Read, its Cleanup and its Close.
const REQUESTS_PER_FILE: usize = 4;
/// Format: the receive buffer follows the journal's eight-byte phase word (`super::capture`).
const RECEIVE_AT: usize = size_of::<u64>();
/// Format: IoStatus.Information, which the model's shard sets to the request's serial so the driver can tell which
/// request a response was made for.
const INFORMATION_AT: usize = STATUS_AT - size_of::<u32>();

/// One recorded event, in program order.
#[derive(Clone)]
enum Event {
  /// A store to the receive journal: its offset and the bytes it left there.
  Journal { offset: usize, bytes: Vec<u8> },
  /// A store to the open ledger's memory.
  Ledger { offset: usize, bytes: Vec<u8> },
  /// The driver's state after it changed (a delivery or a completion). The driver's own writes into the receive buffer
  /// are recorded as journal stores before it, and are never torn: a process dies only in user mode.
  Driver(Box<Driver>),
}

struct Recording {
  journal_at: usize,
  journal_len: usize,
  events: Vec<Event>,
}

thread_local! {
  static RECORDING: RefCell<Option<Recording>> = const { RefCell::new(None) };
}

/// The journal's hook: records a store when it lies in the journal being recorded, and ignores any other buffer.
pub(super) fn journal_stored(bytes: &[u8]) {
  RECORDING.with_borrow_mut(|recording| {
    if let Some(recording) = recording {
      let at = bytes.as_ptr() as usize;
      if at >= recording.journal_at
        && at + bytes.len() <= recording.journal_at + recording.journal_len
      {
        recording.events.push(Event::Journal {
          offset: at - recording.journal_at,
          bytes: bytes.to_vec(),
        });
      }
    }
  });
}

fn record(event: Event) {
  RECORDING.with_borrow_mut(|recording| {
    if let Some(recording) = recording {
      recording.events.push(event);
    }
  });
}

/// The ledger's held memory, recording each store.
struct HeldMemory {
  bytes: Vec<u8>,
}

impl Memory for HeldMemory {
  fn len(&self) -> usize {
    self.bytes.len()
  }
  fn read(&self, at: usize, into: &mut [u8]) -> Result<(), LedgerRefusal> {
    into.copy_from_slice(
      self
        .bytes
        .get(at..at + into.len())
        .ok_or(crate::opens::Refusal::Bound)?,
    );
    Ok(())
  }
  fn write(&mut self, at: usize, from: &[u8]) -> Result<(), LedgerRefusal> {
    self
      .bytes
      .get_mut(at..at + from.len())
      .ok_or(crate::opens::Refusal::Bound)?
      .copy_from_slice(from);
    record(Event::Ledger {
      offset: at,
      bytes: from.to_vec(),
    });
    Ok(())
  }
}

type LedgerRefusal = crate::opens::Refusal;

/// One request the driver issued.
#[derive(Clone, Copy, Debug)]
struct Irp {
  serial: u32,
  hint: u64,
  kind: u32,
  file: usize,
  context: u64,
}

/// The modelled driver: requests queued, in processing, and answered; the files it holds open; and what went wrong.
#[derive(Clone, Default)]
struct Driver {
  next_serial: u32,
  next_hint: u64,
  free_hints: Vec<u64>,
  pending: VecDeque<Irp>,
  processing: Vec<Irp>,
  answered: BTreeSet<u32>,
  held: BTreeMap<usize, u64>,
  ignored: u64,
  violations: Vec<String>,
  /// Opens whose confirmation the receive worker waited for the shard to apply before it went on (audit step 7, cut (c)).
  held_on_confirm: u64,
}

impl Driver {
  fn new() -> Self {
    let mut driver = Self {
      next_serial: 1,
      next_hint: FIRST_HINT,
      ..Self::default()
    };
    for file in 0..FILES {
      driver.issue(CREATE_KIND, file, 0);
    }
    driver
  }

  /// Issues a request, reusing the most recently freed hint, as a recycled IRP address would be.
  fn issue(&mut self, kind: u32, file: usize, context: u64) {
    let hint = self.free_hints.pop().unwrap_or_else(|| {
      self.next_hint += HINT_STRIDE;
      self.next_hint
    });
    let serial = self.next_serial;
    self.next_serial += 1;
    self.pending.push_back(Irp {
      serial,
      hint,
      kind,
      file,
      context,
    });
  }

  /// Writes every queued request into `buffer` and puts them in processing. Answers the bytes written.
  fn deliver(&mut self, buffer: &mut [u8]) -> usize {
    let mut at = 0;
    while let Some(irp) = self.pending.pop_front() {
      let frame = &mut buffer[at..at + REQUEST_BASE];
      frame.fill(0);
      frame[SIZE_AT..SIZE_AT + 2]
        .copy_from_slice(&u16::try_from(REQUEST_BASE).unwrap().to_le_bytes());
      frame[KIND_AT..KIND_AT + 4].copy_from_slice(&irp.kind.to_le_bytes());
      frame[HINT_AT..HINT_AT + 8].copy_from_slice(&irp.hint.to_le_bytes());
      frame[REQUEST_CONTEXT_AT..REQUEST_CONTEXT_AT + 8].copy_from_slice(&irp.context.to_le_bytes());
      frame[SERIAL_AT..SERIAL_AT + 4].copy_from_slice(&irp.serial.to_le_bytes());
      self.processing.push(irp);
      at += REQUEST_BASE;
    }
    at
  }

  /// Completes the request in processing that `response` names by its hint, and issues what its file does next.
  fn complete(&mut self, response: &[u8]) {
    let field = |at: usize, width: usize| -> u64 {
      let mut word = [0; 8];
      word[..width].copy_from_slice(&response[at..at + width]);
      u64::from_le_bytes(word)
    };
    let hint = field(HINT_AT, size_of::<u64>());
    let kind = u32::try_from(field(KIND_AT, size_of::<u32>())).unwrap();
    let status = u32::try_from(field(STATUS_AT, size_of::<u32>())).unwrap();
    let information = u32::try_from(field(INFORMATION_AT, size_of::<u32>())).unwrap();
    let Some(position) = self.processing.iter().position(|irp| irp.hint == hint) else {
      self.ignored += 1;
      return;
    };
    let irp = self.processing.remove(position);
    if irp.kind != kind {
      self.violations.push(format!(
        "request {} of kind {} answered as kind {kind}",
        irp.serial, irp.kind
      ));
    }
    let cancelled = status == CANCELLED && information == 0;
    if !cancelled && information != irp.serial {
      self.violations.push(format!(
        "request {} completed by the response made for request {information}",
        irp.serial
      ));
    }
    if !self.answered.insert(irp.serial) {
      self
        .violations
        .push(format!("request {} answered twice", irp.serial));
    }
    self.free_hints.push(irp.hint);
    match irp.kind {
      CREATE_KIND if status == 0 => {
        let context = field(OPEN_CONTEXT_AT, size_of::<u64>());
        if self.held.insert(irp.file, context).is_some() {
          self
            .violations
            .push(format!("file {} opened twice", irp.file));
        }
        self.issue(READ_KIND, irp.file, context);
      }
      READ_KIND => self.issue(CLEANUP_KIND, irp.file, irp.context),
      CLEANUP_KIND => self.issue(CLOSE_KIND, irp.file, irp.context),
      CLOSE_KIND => {
        self.held.remove(&irp.file);
      }
      _ => {}
    }
  }

  fn quiet(&self) -> bool {
    self.pending.is_empty() && self.processing.is_empty()
  }
}

/// The edges the protocol runs over here: the modelled driver, and a modelled owner shard over the real ledger whose
/// rules are host.rs's (`record_open_reply`, `confirm_reply`, `finish_context_effects`).
struct Model<'a> {
  driver: &'a mut Driver,
  ledger: &'a mut Ledger<HeldMemory>,
}

impl Edges for Model<'_> {
  type Error = String;

  fn receive(&mut self, buffer: &mut [u8]) -> Result<Option<usize>, String> {
    if self.driver.quiet() {
      return Ok(None);
    }
    let reported = self.driver.deliver(buffer);
    journal_stored(&buffer[..reported]);
    record(Event::Driver(Box::new(self.driver.clone())));
    Ok(Some(reported))
  }

  fn send(&mut self, response: &[u8]) -> Result<(), String> {
    self.driver.complete(response);
    record(Event::Driver(Box::new(self.driver.clone())));
    Ok(())
  }

  fn operate(
    &mut self,
    request: &Request<'_>,
    response: &mut [u8],
    replaying: bool,
  ) -> Result<usize, String> {
    let bytes = request.bytes();
    let serial = u32::from_le_bytes(bytes[SERIAL_AT..SERIAL_AT + 4].try_into().unwrap());
    let context = u64::from_le_bytes(
      bytes[REQUEST_CONTEXT_AT..REQUEST_CONTEXT_AT + 8]
        .try_into()
        .unwrap(),
    );
    response[..RESPONSE_BASE].copy_from_slice(&request.cancelled_response().map_err(text)?);
    response[INFORMATION_AT..INFORMATION_AT + 4].copy_from_slice(&serial.to_le_bytes());
    let status: i32 = match request.kind() {
      CREATE_KIND if self.ledger.has_room() => {
        let file = u64::from(serial);
        let open = Open {
          object: ObjectId::new(file, 0),
          fh: file,
          purge: false,
        };
        let opened = self
          .ledger
          .opened_for(open, "\\file", request.hint())
          .map_err(text)?;
        response[OPEN_CONTEXT_AT..OPEN_CONTEXT_AT + 8].copy_from_slice(&opened.to_le_bytes());
        0
      }
      CREATE_KIND => i32::from_le_bytes(STATUS_DISK_FULL.to_le_bytes()),
      // Every other request names a file the driver holds open, by the context it was handed: the shard binds it
      // (host.rs `bind`). A replayed Close whose open the dead daemon had already ended is done; any other refusal is a
      // context the driver holds that the ledger lost.
      kind => match self.ledger.get(context) {
        Ok(_) if kind == CLOSE_KIND => {
          self.ledger.closed(context).map_err(text)?;
          0
        }
        Ok(_) => 0,
        Err(crate::opens::Refusal::Stale) if replaying && kind == CLOSE_KIND => 0,
        Err(refusal) => {
          self.driver.violations.push(format!(
            "request {serial} of kind {kind}: the held context {context:#x} was refused: {refusal}"
          ));
          i32::from_le_bytes(STATUS_INVALID_HANDLE.to_le_bytes())
        }
      },
    };
    super::finish_response(response, status).map_err(text)
  }

  fn confirm_open(&mut self, context: u64) -> Result<(), String> {
    // The worker waits here for the shard to apply the confirmation: a round on its path.
    self.driver.held_on_confirm += 1;
    self.ledger.confirmed(context).map_err(text)
  }

  fn rename_open_names(&mut self, _from: &str, _to: &str) -> Result<(), String> {
    Err("the workload renames nothing".to_owned())
  }
}

impl From<Refusal> for String {
  fn from(refusal: Refusal) -> Self {
    refusal.to_string()
  }
}

fn text(error: impl std::fmt::Display) -> String {
  error.to_string()
}

/// What survives a process: the journal, the ledger's memory and the driver.
#[derive(Clone)]
struct Held {
  journal: Vec<u8>,
  ledger: Vec<u8>,
  driver: Driver,
}

impl Held {
  fn apply(&mut self, event: &Event) {
    match event {
      Event::Journal { offset, bytes } => {
        self.journal[*offset..*offset + bytes.len()].copy_from_slice(bytes)
      }
      Event::Ledger { offset, bytes } => {
        self.ledger[*offset..*offset + bytes.len()].copy_from_slice(bytes)
      }
      Event::Driver(driver) => self.driver = (**driver).clone(),
    }
  }

  /// The state a death after `events[..at]` leaves, with half of `events[at]` stored too when `torn`.
  fn crashed(&self, events: &[Event], at: usize, torn: bool) -> Self {
    let mut held = self.clone();
    for event in &events[..at] {
      held.apply(event);
    }
    if torn {
      match &events[at] {
        Event::Journal { offset, bytes } => {
          let half = bytes.len() / 2;
          held.journal[*offset..*offset + half].copy_from_slice(&bytes[..half]);
        }
        Event::Ledger { offset, bytes } => {
          let half = bytes.len() / 2;
          held.ledger[*offset..*offset + half].copy_from_slice(&bytes[..half]);
        }
        Event::Driver(_) => {}
      }
    }
    held
  }
}

/// Every place a death can fall in `events`: after each event, and torn inside each store this process made of more than
/// one byte. The driver's delivery is one step: WinFsp's transaction copies each request into the receive buffer and
/// puts it in processing inside one kernel loop (sys/volume.c FspVolumeTransact), and a process dies only at the
/// boundary back to user mode, so no death falls between a delivery's bytes and its requests' processing, nor inside
/// those bytes.
fn crash_points(events: &[Event]) -> Vec<(usize, bool)> {
  let mut points = Vec::new();
  for (at, event) in events.iter().enumerate() {
    let after_delivery_bytes = at > 0 && is_delivery(events, at - 1);
    if !after_delivery_bytes {
      points.push((at, false));
    }
    let tearable = match event {
      Event::Journal { bytes, .. } => bytes.len() > 1 && !is_delivery(events, at),
      Event::Ledger { bytes, .. } => bytes.len() > 1,
      Event::Driver(_) => false,
    };
    if tearable {
      points.push((at, true));
    }
  }
  points.push((events.len(), false));
  points
}

/// Whether the journal store at `at` is the driver's delivery into the receive buffer: the store of the receive
/// buffer's bytes that its driver snapshot follows at once.
fn is_delivery(events: &[Event], at: usize) -> bool {
  matches!(
    (&events[at], events.get(at + 1)),
    (Event::Journal { offset, .. }, Some(Event::Driver(_))) if *offset == RECEIVE_AT
  )
}

/// One daemon's life over `held`: recover the ledger (or create it), settle the one unconfirmed open against the journal,
/// settle the journal's kept batch, then receive and serve until the driver is quiet. Records into `events` when given.
fn life(held: &mut Held, fresh: bool, events: Option<&mut Vec<Event>>) -> Result<(), String> {
  let memory = HeldMemory {
    bytes: std::mem::take(&mut held.ledger),
  };
  let recording = events.is_some();
  if recording {
    RECORDING.set(Some(Recording {
      journal_at: held.journal.as_ptr() as usize,
      journal_len: held.journal.len(),
      events: Vec::new(),
    }));
  }
  let outcome = serve(held, memory, fresh);
  let recorded = RECORDING.take();
  if let (Some(events), Some(recorded)) = (events, recorded) {
    *events = recorded.events;
  }
  outcome
}

fn serve(held: &mut Held, memory: HeldMemory, fresh: bool) -> Result<(), String> {
  let mut ledger = if fresh {
    Ledger::create(memory, BOUND).map_err(text)?
  } else {
    Ledger::recover(memory, BOUND).map_err(text)?
  };
  let outcome = serve_with(held, &mut ledger);
  held.ledger = ledger.into_memory().bytes;
  outcome
}

fn serve_with(held: &mut Held, ledger: &mut Ledger<HeldMemory>) -> Result<(), String> {
  let mut scratch = Scratch::new().map_err(text)?;
  let mut copy = held.journal.clone();
  let saved_open = settle::saved_open(&mut copy, &mut scratch).map_err(text)?;
  ledger.settle_pending(saved_open).map_err(text)?;
  // Once the one unconfirmed open is settled, every file the driver holds open must have a live context, but one whose
  // Close the dead daemon had taken: it may have retired the open (or torn the retiring frame, which quarantines it)
  // before it died, and the replayed Close finds it done.
  let closing: BTreeSet<usize> = held
    .driver
    .processing
    .iter()
    .filter(|irp| irp.kind == CLOSE_KIND)
    .map(|irp| irp.file)
    .collect();
  for (file, context) in &held.driver.held {
    if closing.contains(file) {
      continue;
    }
    if let Err(refusal) = ledger.get(*context) {
      let lost = format!("file {file}'s context {context:#x} was lost at recovery: {refusal}");
      held.driver.violations.push(lost);
    }
  }
  let mut response = vec![0u8; RESPONSE_MAX];
  let mut model = Model {
    driver: &mut held.driver,
    ledger,
  };
  settle::recover(
    Capture::new(&mut held.journal).map_err(text)?,
    &mut scratch,
    &mut model,
    &mut response,
  )?;
  let mut used = None;
  loop {
    let receiving = Capture::new(&mut held.journal)
      .map_err(text)?
      .begin(used)
      .map_err(text)?;
    match settle::receive(receiving, &mut scratch, &mut model, &mut response)? {
      Some(filled) => used = Some(filled),
      None => return Ok(()),
    }
  }
}

/// The design's rules, checked once the driver is quiet.
fn check(held: &mut Held, scenario: &str) {
  let driver = &held.driver;
  assert!(
    driver.violations.is_empty(),
    "{scenario}: {:?}",
    driver.violations
  );
  assert!(driver.quiet(), "{scenario}: the driver still has requests");
  let ledger = Ledger::recover(
    HeldMemory {
      bytes: held.ledger.clone(),
    },
    BOUND,
  )
  .unwrap();
  let live: BTreeSet<u64> = ledger.live().map(|(context, _)| context).collect();
  let open: BTreeSet<u64> = driver.held.values().copied().collect();
  assert_eq!(
    live, open,
    "{scenario}: the ledger's live contexts are not the files the driver holds open"
  );
  assert!(
    ledger.has_room(),
    "{scenario}: the ledger kept an unconfirmed open"
  );
  // A receive that found the driver quiet leaves the journal receiving over an empty buffer, which a successor settles
  // as nothing; any request left in it is unsettled.
  let mut scratch = Scratch::new().unwrap();
  let mut journal = held.journal.clone();
  if let Some(mut batch) = Capture::new(&mut journal)
    .unwrap()
    .resume(&mut scratch)
    .unwrap()
  {
    assert!(
      batch.next_request().unwrap().is_none(),
      "{scenario}: the journal kept an unsettled request"
    );
  }
}

/// A fresh daemon's held state: a zeroed journal, a zeroed ledger section, and the driver with its first requests.
fn fresh() -> Held {
  Held {
    journal: vec![0; super::capture::BYTES],
    ledger: vec![0; crate::opens::bytes(BOUND).unwrap()],
    driver: Driver::new(),
  }
}

/// A-198, audit step 7. Do: run the workload to quiescence once, recording every store to held memory; then, for each
/// store, kill the daemon there (and torn inside it), let a successor recover and serve to quiescence, and for each
/// store of that recovery kill it too and let a third owner finish. Expect: every request answered once, by its own
/// response or a cancellation; the ledger's live contexts exactly the files the driver holds open; no unconfirmed open
/// and no unsettled batch left.
#[test]
fn a_crash_after_any_store_to_held_memory_loses_no_open_and_answers_every_request_once() {
  let start = fresh();
  let mut reference = start.clone();
  let mut events = Vec::new();
  life(&mut reference, true, Some(&mut events)).unwrap();
  check(&mut reference, "the reference");
  assert!(
    reference.driver.answered.len() >= FILES * REQUESTS_PER_FILE,
    "the reference answered {} requests",
    reference.driver.answered.len()
  );
  // The ledger's own creation is recorded too; a death before its header is whole leaves no ledger to recover, so the
  // first crash point follows it.
  let created = events
    .iter()
    .position(|event| matches!(event, Event::Journal { .. }))
    .unwrap();
  let after_creation = start.crashed(&events, created, false);
  let points = crash_points(&events[created..]);
  let tail = &events[created..];
  let mut scenarios = 0usize;
  for (at, torn) in points {
    let mut crashed = after_creation.crashed(tail, at, torn);
    let before_recovery = crashed.clone();
    let mut recovery = Vec::new();
    life(&mut crashed, false, Some(&mut recovery))
      .unwrap_or_else(|error| panic!("crash at {at} (torn {torn}): recovery refused: {error}"));
    check(&mut crashed, &format!("crash at {at} (torn {torn})"));
    scenarios += 1;
    for (again, torn_again) in crash_points(&recovery) {
      let mut twice = before_recovery.crashed(&recovery, again, torn_again);
      life(&mut twice, false, None).unwrap_or_else(|error| {
        panic!("crash at {at} (torn {torn}), then at {again} (torn {torn_again}): {error}")
      });
      check(
        &mut twice,
        &format!("crash at {at} (torn {torn}), then at {again} (torn {torn_again})"),
      );
      scenarios += 1;
    }
  }
  assert!(scenarios > tail.len(), "only {scenarios} scenarios ran");
}

/// Audit step 7, cut (c). Do: serve the workload, whose three files are each opened. Expect: the receive worker never waits
/// for the shard to confirm an open it has just handed the driver: it posts the confirmation and takes its next request,
/// and the shard's FIFO channel applies the confirmation ahead of that request. Before, every open held the worker for a
/// shard round: 9.9 us at the mean on arm64 quiet and 139.6 us loaded (diag-winfsp-overlap-50, run 42506936806).
#[test]
fn an_opens_confirmation_does_not_hold_the_receive_worker() {
  let mut held = fresh();
  life(&mut held, true, None).unwrap();
  check(&mut held, "the workload");
  assert_eq!(
    held.driver.held_on_confirm, 0,
    "the receive worker waited for {} confirmations of opens it had handed the driver",
    held.driver.held_on_confirm
  );
}
