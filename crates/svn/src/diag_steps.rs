//! DIAG (w-bugs, never for main): each kind of step's time, kept per thread, so a diagnostics run names what holds a
//! shard past its quantum.

use std::cell::RefCell;
use std::collections::BTreeMap;

thread_local! {
  /// Per kind: steps, past 100 us, past 1 ms, longest ns, total ns.
  static KINDS: RefCell<BTreeMap<&'static str, (u64, u64, u64, u64, u64)>> = const { RefCell::new(BTreeMap::new()) };
}

/// Notes one step of `kind` that took `ns`.
pub fn record(kind: &'static str, ns: u64) {
  KINDS.with(|kinds| {
    let mut kinds = kinds.borrow_mut();
    let entry = kinds.entry(kind).or_insert((0, 0, 0, 0, 0));
    entry.0 += 1;
    entry.1 += u64::from(ns > 100_000);
    entry.2 += u64::from(ns > 1_000_000);
    entry.3 = entry.3.max(ns);
    entry.4 += ns;
  });
}

/// Times `run` as a step of `kind`.
pub fn timed<T>(kind: &'static str, run: impl FnOnce() -> T) -> T {
  let began = std::time::Instant::now();
  let made = run();
  record(kind, u64::try_from(began.elapsed().as_nanos()).unwrap_or(u64::MAX));
  made
}

/// The kinds so far, one line, and the table emptied.
pub fn take_summary() -> String {
  KINDS.with(|kinds| {
    let kinds = std::mem::take(&mut *kinds.borrow_mut());
    kinds
      .iter()
      .map(|(kind, (steps, past100, past1ms, longest, total))| {
        format!("{kind}: n={steps} >100us={past100} >1ms={past1ms} max={longest}ns total={total}ns")
      })
      .collect::<Vec<_>>()
      .join(" | ")
  })
}
