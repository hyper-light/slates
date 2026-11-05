//! DIAGNOSTICS ONLY, never for main: every call of a named phase, for attributing a WinFsp request's time.
use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::Instant;
static PHASES: Mutex<BTreeMap<(&'static str, u32), Vec<u32>>> = Mutex::new(BTreeMap::new());
pub(crate) fn note(name: &'static str, kind: u32, started: Instant) {
  let elapsed = u32::try_from(started.elapsed().as_nanos()).unwrap_or(u32::MAX);
  if let Ok(mut phases) = PHASES.lock() {
    phases.entry((name, kind)).or_default().push(elapsed);
  }
}
pub(crate) fn dump() {
  let Ok(mut phases) = PHASES.lock() else { return };
  for ((name, kind), samples) in phases.iter_mut() {
    samples.sort_unstable();
    let count = samples.len();
    let at = |permille: usize| samples.get(count.saturating_mul(permille) / 1000).copied().unwrap_or(0);
    let sum: u64 = samples.iter().map(|sample| u64::from(*sample)).sum();
    eprintln!("winfsp-phase\t{name}\t{kind}\t{count}\tp50 {}\tp99 {}\tmean {}", at(500), at(990), sum / u64::try_from(count.max(1)).unwrap_or(1));
  }
  phases.clear();
}
pub(crate) fn op_name(op: &crate::host::Op) -> &'static str {
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
    _ => "other",
  }
}
