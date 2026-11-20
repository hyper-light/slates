//! A test's wait on slates processes it started, judged in their own CPU (A-228's rule, as the anchor judges its
//! daemon's start; `live_chaos`' start wait, b25a5b7a6): the wait goes on while the process tree it watches lives and
//! gains CPU, and fails, naming what it waited for, once the tree is gone, has gained no CPU for [`STALL`], or has spent
//! [`CPU_BUDGET`] of it without the answer. A fixed
//! wall bound lost to load: a loaded box starves a daemon's start or an anchor's restart past any wall bound while both
//! keep running, and those tests failed for the box, not the code (goal 88's sweep; main's ruling, 2026-11-20).

// Each suite that includes this module by path uses part of it.
#![allow(dead_code)]

use std::time::{Duration, Instant};

/// Derived: how long a waited-on tree may gain no CPU before the wait fails: the anchor's own ceiling on judging a lapse
/// (`slates_server::daemon::OBSERVE_BUDGET_NS`, which its lapse bound never exceeds), past which a live anchor has
/// judged a stuck daemon and the tree would show its restart.
pub(crate) const STALL: Duration = Duration::from_nanos(slates_server::daemon::OBSERVE_BUDGET_NS);

/// Shape: the CPU a waited-on tree may spend, from the wait's start, before the wait fails without the answer:
/// `live_chaos`' `START_CPU`, over twice the 13 s of CPU a lone debug daemon's start measured on a loaded Mac
/// (`MachineProfile::measure_patiently`, 2026-11-07). In CPU, not the wall: a starved tree spends it slowly and is
/// waited on, and one that runs without bringing the answer about spends it at its own pace and fails.
pub(crate) const CPU_BUDGET: Duration = Duration::from_secs(30);

/// A wait on the process tree under `root` (an anchor and its daemon, or a daemon alone), judged in its own CPU.
pub(crate) struct Judged {
  root: u32,
  /// The tree's CPU at the last look that saw it gain some.
  mark: Option<Duration>,
  /// When that look was.
  since: Instant,
  /// The CPU the tree gained over the looks since the wait began (a successor's from its first reading).
  spent: Duration,
}

/// What a look at a waited-on tree came to.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Verdict {
  /// Still alive and gaining CPU within [`STALL`]; whether this look saw it gain some.
  Waiting { progressed: bool },
  /// The tree's root is gone.
  Gone,
  /// No CPU gained for [`STALL`].
  Stalled,
  /// [`CPU_BUDGET`] spent since the wait began.
  SpentItsBudget,
}

impl Judged {
  /// A wait on the tree under `root`, from now.
  pub(crate) fn of(root: u32) -> Judged {
    Judged {
      root,
      mark: tree_cpu(root),
      since: Instant::now(),
      spent: Duration::ZERO,
    }
  }

  /// One look: returns while the tree lives and has gained CPU within [`STALL`]; fails the test naming `what` once it is
  /// gone or stalled.
  pub(crate) fn look(&mut self, what: &str) {
    match self.judge() {
      Verdict::Waiting { .. } => {}
      gave_up => panic!(
        "{what}: the processes under {} are {gave_up:?} (no CPU gained for {:?}, the bound {STALL:?}; {:?} of CPU \
         spent, the budget {CPU_BUDGET:?})",
        self.root,
        self.since.elapsed(),
        self.spent
      ),
    }
  }

  /// The tree's verdict now, moving the mark on progress; for a waiter that ends its own wait when the root leaves
  /// (reaping it), where a root gone between its check and this look is no failure.
  pub(crate) fn judge(&mut self) -> Verdict {
    let now = tree_cpu(self.root);
    if let (Some(then), Some(now)) = (self.mark, now) {
      self.spent = self.spent.saturating_add(now.saturating_sub(then));
    }
    let judged = verdict(self.mark, now, self.since.elapsed(), self.spent);
    if judged == (Verdict::Waiting { progressed: true }) {
      self.mark = now;
      self.since = Instant::now();
    }
    judged
  }
}

/// [`Judged::look`]'s rule, apart from the clocks it reads: the tree's CPU at the last progress (`then`) and now, how
/// long since that progress, and the CPU it spent since the wait began.
pub(crate) fn verdict(
  then: Option<Duration>,
  now: Option<Duration>,
  since_progress: Duration,
  spent: Duration,
) -> Verdict {
  let Some(now) = now else {
    return Verdict::Gone;
  };
  if spent >= CPU_BUDGET {
    return Verdict::SpentItsBudget;
  }
  if then.is_none_or(|then| now > then) {
    return Verdict::Waiting { progressed: true };
  }
  if since_progress >= STALL {
    return Verdict::Stalled;
  }
  Verdict::Waiting { progressed: false }
}

/// The CPU (user and system) the process `root` and its children have run, or `None` once `root` is gone.
pub(crate) fn tree_cpu(root: u32) -> Option<Duration> {
  let own = process_cpu(root)?;
  Some(
    children_of(root)
      .into_iter()
      .filter_map(process_cpu)
      .fold(own, Duration::saturating_add),
  )
}

/// One process's CPU, as the anchor judges its daemon's start (`slates_machine::process_clock`, A-228).
fn process_cpu(pid: u32) -> Option<Duration> {
  slates_machine::process_clock::process_cpu(pid).map(|cpu| Duration::from_nanos(cpu.spent_ns))
}

/// The live children of `parent` (macOS: `proc_listchildpids`).
#[cfg(target_os = "macos")]
fn children_of(parent: u32) -> Vec<u32> {
  /// Shape: room for a tree's children: an anchor runs one daemon at a time.
  const ROOM: usize = 16;
  let mut pids = [0i32; ROOM];
  let Ok(parent) = i32::try_from(parent) else {
    return Vec::new();
  };
  // SAFETY: `pids` is this frame's own buffer of the byte length passed; the call writes at most that many.
  let filled = unsafe {
    libc::proc_listchildpids(
      parent,
      pids.as_mut_ptr().cast(),
      i32::try_from(std::mem::size_of_val(&pids)).unwrap_or(0),
    )
  };
  let count = usize::try_from(filled).unwrap_or(0).min(ROOM);
  pids
    .iter()
    .take(count)
    .filter_map(|pid| u32::try_from(*pid).ok())
    .filter(|pid| *pid != 0)
    .collect()
}

/// The live children of `parent` (Linux: each process's parent in `/proc/<pid>/stat`).
#[cfg(target_os = "linux")]
fn children_of(parent: u32) -> Vec<u32> {
  let Ok(entries) = std::fs::read_dir("/proc") else {
    return Vec::new();
  };
  entries
    .flatten()
    .filter_map(|entry| entry.file_name().to_str()?.parse::<u32>().ok())
    .filter(|pid| {
      std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .ok()
        .and_then(|stat| {
          stat
            .rsplit_once(')')
            .and_then(|(_, rest)| rest.split_whitespace().nth(1)?.parse::<u32>().ok())
        })
        == Some(parent)
    })
    .collect()
}

/// The live children of `parent` elsewhere: none known, so the root's own CPU is the tree's.
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn children_of(_parent: u32) -> Vec<u32> {
  Vec::new()
}

/// [`verdict`] (goal 88's sweep, 2026-11-20). Do: judge a tree that gained CPU long after the old wall bounds, one that
/// gained none for [`STALL`], one that gained none for less, one whose root is gone, and one that gained CPU until the
/// wait had spent [`CPU_BUDGET`] of it. Expect the first waited on with its progress marked, the second given up as
/// stalled, the third still waited on, the fourth given up as gone, and the last given up as having spent its budget:
/// a tree that runs and never brings the answer about is not waited on forever (`live_chaos`' `START_CPU`).
#[test]
fn a_tree_is_waited_on_while_it_gains_cpu_and_given_up_stalled_gone_or_spent() {
  let cpu = Duration::from_millis(5);
  let spent = cpu;
  assert_eq!(
    verdict(Some(cpu), Some(cpu * 2), STALL * 10, spent),
    Verdict::Waiting { progressed: true },
    "a tree gaining CPU is waited on however long it takes"
  );
  assert_eq!(
    verdict(Some(cpu), Some(cpu), STALL, spent),
    Verdict::Stalled
  );
  assert_eq!(
    verdict(Some(cpu), Some(cpu), STALL / 2, spent),
    Verdict::Waiting { progressed: false }
  );
  assert_eq!(
    verdict(Some(cpu), None, Duration::ZERO, spent),
    Verdict::Gone
  );
  assert_eq!(
    verdict(Some(cpu), Some(cpu * 2), Duration::ZERO, CPU_BUDGET),
    Verdict::SpentItsBudget,
    "a tree that spent the wait's CPU budget without the answer is given up"
  );
}
