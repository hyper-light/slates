//! DIAGNOSTICS ONLY, never for main: every call of a named phase, for attributing a WinFsp request's time. A key is an
//! operation or phase name and a stage: see `STAGES`.
#![allow(missing_docs)]
use std::cell::Cell;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;
static PHASES: Mutex<BTreeMap<(&'static str, u32), Vec<u32>>> = Mutex::new(BTreeMap::new());
static START: OnceLock<Instant> = OnceLock::new();
static REPLY_SENT: AtomicU64 = AtomicU64::new(0);
thread_local! {
  /// The operation the shard is answering, and when its last stage ended.
  static SHARD: Cell<(&'static str, Option<Instant>)> = const { Cell::new(("none", None)) };
  /// When this shard last finished answering.
  static LAST_ANSWER: Cell<Option<Instant>> = const { Cell::new(None) };
}
pub const HOP: u32 = 1;
pub const SERVICE: u32 = 2;
pub const SEND_AND_WAKE: u32 = 3;
pub const QUEUE: u32 = 4;
pub const BEFORE_SERVICE: u32 = 5;
pub const SERVED_TO_ANSWER: u32 = 6;
pub const PUBLISH: u32 = 7;
pub const REPLY_SEND: u32 = 8;
pub const REPLY_WAKE: u32 = 9;
const STAGES: [&str; 15] = [
  "other", "hop", "service", "send_and_wake", "queue", "before_service", "served_to_answer", "publish", "reply_send",
  "reply_wake", "queue_idle_under_20us", "queue_idle_under_50us", "queue_idle_under_200us", "queue_idle_under_1ms", "queue_idle_longer",
];
pub fn note(name: &'static str, kind: u32, started: Instant) {
  let elapsed = u32::try_from(started.elapsed().as_nanos()).unwrap_or(u32::MAX);
  if let Ok(mut phases) = PHASES.lock() {
    phases.entry((name, kind)).or_default().push(elapsed);
  }
}
fn now_ns() -> u64 {
  u64::try_from(START.get_or_init(Instant::now).elapsed().as_nanos()).unwrap_or(u64::MAX)
}
/// The shard took a request sent at `sent`.
pub fn picked(name: &'static str, sent: Instant) {
  note(name, QUEUE, sent);
  // The queue wait by how long the shard had been idle when the request was sent.
  if let Some(last) = LAST_ANSWER.get() {
    let idle = sent.saturating_duration_since(last).as_micros();
    let bucket = match idle {
      0..20 => 10,
      20..50 => 11,
      50..200 => 12,
      200..1000 => 13,
      _ => 14,
    };
    note(name, bucket, sent);
  }
  SHARD.set((name, Some(Instant::now())));
}
/// The shard's service of the picked request begins; returns when, for its end.
pub fn service_begins() -> Instant {
  let (name, stage) = SHARD.get();
  if let Some(stage) = stage {
    note(name, BEFORE_SERVICE, stage);
  }
  Instant::now()
}
/// The service begun at `started` ended.
pub fn service_ended(started: Instant) {
  let (name, _) = SHARD.get();
  note(name, SERVICE, started);
  SHARD.set((name, Some(Instant::now())));
}
/// `answer` begins.
pub fn answer_begins() {
  let (name, stage) = SHARD.get();
  if let Some(stage) = stage {
    note(name, SERVED_TO_ANSWER, stage);
  }
  SHARD.set((name, Some(Instant::now())));
}
/// A barrier's publish ended, or there was none.
pub fn published(barrier: bool) {
  let (name, stage) = SHARD.get();
  if barrier && let Some(stage) = stage {
    note(name, PUBLISH, stage);
  }
  SHARD.set((name, Some(Instant::now())));
  REPLY_SENT.store(now_ns(), Ordering::Relaxed);
}
/// The reply is in its channel.
pub fn replied() {
  let (name, stage) = SHARD.get();
  if let Some(stage) = stage {
    note(name, REPLY_SEND, stage);
  }
  LAST_ANSWER.set(Some(Instant::now()));
}
/// The waiting thread has the reply.
pub fn reply_woke(name: &'static str) {
  let elapsed = now_ns().saturating_sub(REPLY_SENT.load(Ordering::Relaxed));
  if let Ok(mut phases) = PHASES.lock() {
    phases
      .entry((name, REPLY_WAKE))
      .or_default()
      .push(u32::try_from(elapsed).unwrap_or(u32::MAX));
  }
}
pub fn dump() {
  let Ok(mut phases) = PHASES.lock() else { return };
  for ((name, kind), samples) in phases.iter_mut() {
    samples.sort_unstable();
    let count = samples.len();
    let at = |permille: usize| samples.get(count.saturating_mul(permille) / 1000).copied().unwrap_or(0);
    let sum: u64 = samples.iter().map(|sample| u64::from(*sample)).sum();
    let stage = STAGES.get(usize::try_from(*kind).unwrap_or(0)).copied().unwrap_or("other");
    eprintln!("winfsp-phase\t{name}\t{stage}\t{count}\tp50 {}\tp90 {}\tp99 {}\tmean {}", at(500), at(900), at(990), sum / u64::try_from(count.max(1)).unwrap_or(1));
  }
  phases.clear();
}
pub(crate) fn op_name<T>(op: &crate::host::Op<T>) -> &'static str {
  use crate::host::Op;
  match op {
    Op::Open { .. } => "Open",
    Op::Create { .. } => "Create",
    Op::Read { .. } => "Read",
    Op::Write { .. } => "Write",
    Op::Info { .. } => "Info",
    Op::SetSize { .. } => "SetSize",
    Op::Rename { .. } => "Rename",
    Op::Flush { .. } => "Flush",
    Op::Cleanup { .. } => "Cleanup",
    Op::Release { .. } => "Release",
    Op::Resolve { .. } => "Resolve",
    Op::Confirm { .. } => "Confirm",
    Op::RenameName { .. } => "RenameName",
    _ => "other",
  }
}
