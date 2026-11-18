//! Out-of-band observation of a shard (§4.14 "observability"; §4.3 "admission"): a question asked of
//! a daemon from another thread — the membership its fleet sees, a counter, a fault to inject for a
//! test — carried to the shard as a one-shot task and answered on a channel the asker owns.
//!
//! The question's journey has three stages, and every way it can end short of an answer is a typed
//! stage ([`ObserveError`]), never a bare `None`: **submission** into the shard's bounded control
//! channel (refused `ControlFull` under load, `ShardGone` once the shard exited or its registry slot
//! was reused by a later daemon — the submission is pinned to the shard's registration,
//! [`slates_rt::SlotHolder`], so it never reaches a stranger); **admission** into the shard's task arena,
//! reported on the request's receipt ([`slates_rt::Admission`]: admitted, refused for a full arena, or
//! terminated by a shutdown); and **execution**, the task borrowing the shard's state
//! ([`crate::state::try_with_state`], whose own refusals — absent, fenced, borrowed, or a retention
//! check that discarded the answer — are carried through) and answering.
//!
//! One absolute deadline spans all three stages. Only a **capacity** refusal is retried inside it — a
//! full control channel at submission, a full arena at admission — because those clear as the shard
//! drains; a gone shard, a terminated request, an absent state are final at once. The deadline names
//! the stage it elapsed at and the refusal that kept the observation waiting there, so a starved shard
//! is told apart from a dead one and from a live one that answered "nothing yet". A reply that arrives
//! after the asker's deadline is discarded and counted on the shard's refusal ledger
//! ([`OBSERVE_LATE_REPLY`]), so a late answer never satisfies a later question.
//!
//! Evidence for the shape: a test-facing observation that silently dropped on a full control channel
//! read a *starved* shard as a *fact* — an injected death that never landed, a leader that "was not"
//! (`docs/bugs/2026-09-13-consensus-voters-outside-record-neighbourhood.md`); and with every failure
//! folded into `None`, the 2026-09-16 diagnosis of three CI-red fleet tests first misread a starved
//! shard's silence as "observations failing fast" (`docs/bugs/2026-09-17-observations-are-typed-to-their-stage.md`).

use std::marker::PhantomData;
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::time::{Duration, Instant};

use slates_rt::{Admission, CpuReading, RtError, RunReading, RunState, SlotHolder, TaskId};

use crate::daemon::HEARTBEAT_NS;
use crate::fleet::POLL_PER_PERIOD;
use crate::state::{self, StateAccess};

/// The refusal-ledger key under which a shard counts an observation's reply that arrived after the
/// asker's deadline and was discarded (§4.14): a test reads it to prove a late reply was refused,
/// never delivered to a later question.
pub const OBSERVE_LATE_REPLY: &str = "observe.late_reply";
/// Format: nanoseconds per millisecond — the unit the refusal messages report waits and budgets in.
const NANOS_PER_MILLI: u64 = 1_000_000;

/// Where an observation was when it ended short of an answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ObserveStage {
  /// Submitting the question to the shard's control channel.
  Submission,
  /// Submitted; waiting for the shard to drain the request and admit the task.
  Admission,
  /// Admitted; waiting for the task to run the question and answer.
  Execution,
}

impl std::fmt::Display for ObserveStage {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.write_str(match self {
      ObserveStage::Submission => "submission",
      ObserveStage::Admission => "admission",
      ObserveStage::Execution => "execution",
    })
  }
}

/// Why an observation ended without an answer: the stage it reached and what ended it there
/// (§4.14). [`ObserveError::is_terminal`] is the reading a poll needs — whether asking again could
/// ever answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ObserveError {
  /// The daemon has no shard for the question: an owner shard past its shard list, or a shard index
  /// it does not have.
  NoTarget,
  /// The daemon has no runtime: it is stopping or stopped.
  NoRuntime,
  /// The shard is gone — its registry slot is free, or held by a later registration — at `stage`:
  /// it exited, or the daemon stopped while the observation was pending. Never retried.
  ShardGone {
    /// The shard's runtime id.
    shard: u16,
    /// The stage the observation was at.
    stage: ObserveStage,
  },
  /// Submission refused for a reason that cannot clear (anything but a full control channel), with
  /// the runtime's own refusal.
  Submission {
    /// The runtime's refusal.
    refusal: RtError,
    /// Submissions attempted, this one included.
    attempts: u32,
    /// Nanoseconds since the observation began.
    waited_ns: u64,
  },
  /// Admission refused for a reason that cannot clear (anything but a full arena), with the runtime's
  /// own refusal.
  Admission {
    /// The runtime's refusal.
    refusal: RtError,
    /// Submissions attempted, this one included.
    attempts: u32,
    /// Nanoseconds since the observation began.
    waited_ns: u64,
  },
  /// Ended unanswered at `stage`: the request was terminated unadmitted (the shard shutting down), or
  /// the admitted task was dropped before it answered (cancelled by a shutdown).
  Terminated {
    /// The stage the observation was at.
    stage: ObserveStage,
    /// Submissions attempted, this one included.
    attempts: u32,
  },
  /// The budget elapsed at `stage`. `last_refusal` is the capacity refusal that kept the observation
  /// waiting there — a full control channel at submission, a full arena at admission — or none when
  /// the request was submitted and simply never drained, or admitted and never run, within the budget.
  Deadline {
    /// The stage the budget elapsed at.
    stage: ObserveStage,
    /// The whole budget, nanoseconds.
    budget_ns: u64,
    /// Submissions attempted, the last included.
    attempts: u32,
    /// Nanoseconds since the observation began.
    waited_ns: u64,
    /// The capacity refusal the last attempt met, if any.
    last_refusal: Option<RtError>,
  },
  /// The question ran on the shard, but its state was out of reach — absent, fenced, borrowed — or the
  /// retention check after the borrow refused and the answer was discarded; which one is named.
  State(StateAccess),
}

impl ObserveError {
  /// Whether the daemon can never answer this question: nothing to ask (no target, no runtime), the
  /// shard gone, a refusal that cannot clear, the request terminated, or the state absent or fenced.
  /// Everything else — a budget spent waiting on a capacity refusal, a borrowed state, a retention
  /// refusal — can clear, and a poll keeps asking.
  pub fn is_terminal(&self) -> bool {
    match self {
      ObserveError::NoTarget
      | ObserveError::NoRuntime
      | ObserveError::ShardGone { .. }
      | ObserveError::Submission { .. }
      | ObserveError::Admission { .. }
      | ObserveError::Terminated { .. }
      | ObserveError::State(StateAccess::Absent)
      | ObserveError::State(StateAccess::Fenced) => true,
      ObserveError::Deadline { .. }
      | ObserveError::State(StateAccess::Borrowed)
      | ObserveError::State(StateAccess::Retention(_)) => false,
    }
  }

  /// The stage the observation reached, for the refusals that have one.
  pub fn stage(&self) -> Option<ObserveStage> {
    match self {
      ObserveError::ShardGone { stage, .. }
      | ObserveError::Terminated { stage, .. }
      | ObserveError::Deadline { stage, .. } => Some(*stage),
      ObserveError::Submission { .. } => Some(ObserveStage::Submission),
      ObserveError::Admission { .. } => Some(ObserveStage::Admission),
      ObserveError::State(_) => Some(ObserveStage::Execution),
      ObserveError::NoTarget | ObserveError::NoRuntime => None,
    }
  }
}

impl std::fmt::Display for ObserveError {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      ObserveError::NoTarget => f.write_str("the daemon has no shard for this question"),
      ObserveError::NoRuntime => f.write_str("the daemon has no runtime (it is stopping)"),
      ObserveError::ShardGone { shard, stage } => {
        write!(
          f,
          "shard {shard} is gone (its slot free or reused) at {stage}"
        )
      }
      ObserveError::Submission {
        refusal,
        attempts,
        waited_ns,
      } => write!(
        f,
        "submission refused after {attempts} attempt(s) over {} ms: {refusal}",
        waited_ns / NANOS_PER_MILLI
      ),
      ObserveError::Admission {
        refusal,
        attempts,
        waited_ns,
      } => write!(
        f,
        "admission refused after {attempts} attempt(s) over {} ms: {refusal}",
        waited_ns / NANOS_PER_MILLI
      ),
      ObserveError::Terminated { stage, attempts } => {
        write!(f, "terminated unanswered at {stage} (attempt {attempts})")
      }
      ObserveError::Deadline {
        stage,
        budget_ns,
        attempts,
        waited_ns,
        last_refusal,
      } => {
        write!(
          f,
          "the budget of {} ms elapsed at {stage} after {attempts} attempt(s) ({} ms)",
          budget_ns / NANOS_PER_MILLI,
          waited_ns / NANOS_PER_MILLI
        )?;
        match last_refusal {
          Some(refusal) => write!(f, "; the last refusal there: {refusal}"),
          None => Ok(()),
        }
      }
      ObserveError::State(access) => write!(f, "the question ran, but the state was {access}"),
    }
  }
}

/// An observation begun on a daemon's shard, owned by the asker until it is run. `Send`: it may be
/// run from another thread, and it outlives the daemon — a daemon stopped while it is pending answers
/// it `ShardGone` or `Terminated`, never leaves it waiting, because its submission is pinned to the
/// shard's registration ([`SlotHolder`]) and its receipt is answered by the runtime. Two phases:
/// [`Observation::admit`] carries the question through submission and admission and hands back the
/// [`Admitted`] task, whose [`Admitted::answer`] waits for the reply; [`Observation::wait`] is both.
#[derive(Debug)]
pub struct Observation<T, Q> {
  holder: SlotHolder,
  budget_ns: u64,
  question: Q,
  answer: PhantomData<fn() -> T>,
}

/// An observation's task, admitted to its shard: the reply is pending under what is left of the one
/// budget the observation began with.
#[derive(Debug)]
pub struct Admitted<T> {
  task: TaskId,
  reply: Receiver<Result<T, StateAccess>>,
  began: Instant,
  deadline: Instant,
  budget_ns: u64,
  attempts: u32,
  time: ShardTime,
}

/// An observation's budget counted in the observed shard's own time (§4.14, A-65): the shard's CPU clock when the
/// observation began, and at the last window's end. At a wall deadline the observation goes on — for the part of the
/// budget the shard has not yet run, as wall time — only while the shard is working: it ran on the CPU during the
/// window just ended, or was waiting for one (A-228). A starved shard, runnable but given little CPU, keeps consuming
/// some, or stands still in the run queue, and is answered; a wedged one, which consumes none and waits on no CPU, ends
/// the observation at the end of the first whole window it spends so (one window if it was wedged when the observation
/// began, two if it wedged partway through the first); a busy one that never answers ends it once it has run
/// the budget, which an uncontended shard does at the wall clock's pace. A reading at or below the mark (a thread id the
/// OS reused once the shard's thread had ended, which reads lower) counts as no progress. Under Miri, which has no thread
/// clock, the wall budget stands alone.
///
/// A cross-shard call's wait judged in its target's own time uses the same rule (`crate::xshard::within_shard_time`,
/// the R3 debt's class A).
#[derive(Clone, Copy, Debug)]
pub(crate) struct ShardTime {
  holder: SlotHolder,
  /// The rule, apart from the shard it reads.
  own: OwnTime,
}

/// What [`ShardTime`] judges each window's end by, apart from the shard it reads: the budget, the CPU time the shard had
/// run when the wait began, and its progress at the last window's end. Its rule takes its readings through closures, in
/// the order [`OwnTime::next_window_ns`] states, so its tests run it over scripted readings with no thread under it, as
/// the watchdog's replays run `crate::watchdog::ShardWatch::judge`.
#[derive(Clone, Copy, Debug)]
struct OwnTime {
  budget_ns: u64,
  /// The CPU time the shard had run when the observation began.
  began: Option<u64>,
  /// The shard's progress at the last window's end (first: at the observation's start); it only rises.
  mark: Option<u64>,
}

impl ShardTime {
  pub(crate) fn new(holder: SlotHolder, budget_ns: u64) -> Self {
    let began = slates_rt::registry::shard_cpu(holder);
    ShardTime {
      holder,
      own: OwnTime {
        budget_ns,
        began: began.map(|reading| reading.spent_ns),
        mark: began.map(|reading| reading.progress),
      },
    }
  }

  /// The next wall deadline when the shard worked through the window just ended within its budget; `None` ends the
  /// observation.
  fn extend(&mut self) -> Option<Instant> {
    Instant::now().checked_add(Duration::from_nanos(self.next_window_ns()?))
  }

  /// The next window's wall length when the shard worked through the window just ended within its budget, or waited for
  /// a CPU through it: the budget it has not yet run. `None` ends the wait: the shard made no progress in the window and
  /// waited on no CPU, it has run its budget, or its clock cannot be read. The shard's CPU clock is
  /// `slates_rt::registry::shard_cpu`; what the scheduler says of it is `slates_rt::registry::shard_run_readings`, the
  /// watchdog's source, whose state decides on every platform: a thread held off its CPU stays runnable, and Linux
  /// credits a run-queue wait only once the thread reaches a CPU (A-228's notes). No reading (Miri, or a source that
  /// cannot be read) says not runnable, as before A-228.
  pub(crate) fn next_window_ns(&mut self) -> Option<u64> {
    let holder = self.holder;
    self.own.next_window_ns(
      || slates_rt::registry::shard_cpu(holder),
      || {
        slates_rt::registry::shard_run_readings(&[holder])
          .pop()
          .flatten()
      },
    )
  }
}

impl OwnTime {
  /// The rule at a window's end, over the shard's CPU `clock` and what the `scheduler` says of its thread, read in this
  /// order: the clock; when it shows no progress since the mark, the scheduler; then the clock again.
  ///
  /// The second read is what makes the scheduler's answer mean anything. A thread leaves the run queue for a block only
  /// by running first, which its clock shows: so a shard the scheduler finds blocked, whose clock read after that answer
  /// still shows no progress, was blocked or waiting for a CPU through the whole window, never running, and the answer
  /// stands for the window. A clock read only before the answer cannot say so. A starved shard released between the two
  /// reads runs, answers and parks before the scheduler looks, and the old order judged it wedged and gave up on an
  /// observation already answered. On CI's Windows runner, where one look at the scheduler is a whole-system snapshot of
  /// about 3 ms, `an_observation_of_a_starved_shard_is_answered` ended so: its failing tree, rerun with every decision
  /// logged, ended `Deadline { stage: Admission }` 3.019 s after it asked in 1 of 32 runs, the clock read at the mark and
  /// the scheduler finding the shard parked (`ThreadState` 5, `WrQueue`) within 4 ms of its poll's end
  /// (`docs/bugs/2026-11-18-an-observation-read-the-clock-before-the-scheduler.md`).
  fn next_window_ns(
    &mut self,
    mut clock: impl FnMut() -> Option<CpuReading>,
    scheduler: impl FnOnce() -> Option<RunReading>,
  ) -> Option<u64> {
    let first = clock()?;
    if self.progressed_by(first) {
      return self.worked_to(first);
    }
    // No CPU in the window at that read. Starved is not stuck (A-228): a shard the scheduler has runnable, waiting for a
    // CPU, is given the window again; only one that waits on something else, a deadlock or a call that never returns,
    // ends the wait. On adacachy2 an observation of a shard held off its CPU for 3.0 s ended at its one-window deadline
    // 3 of 3 before A-228's sibling fix.
    let runnable = scheduler().is_some_and(|run| run.state == RunState::Runnable);
    let after = clock()?;
    if self.progressed_by(after) {
      return self.worked_to(after);
    }
    if runnable {
      self.unspent_at(after)
    } else {
      None
    }
  }

  /// Whether the shard's clock rose past the mark: it ran since the last window's end.
  fn progressed_by(&self, reading: CpuReading) -> bool {
    self.mark.is_some_and(|mark| reading.progress > mark)
  }

  /// The budget the shard has not yet run at `reading`, as wall time: an uncontended shard runs at the wall clock's pace
  /// and has next to none left; a starved one has most of it. `None` once it has run the budget, or without a start.
  fn unspent_at(&self, reading: CpuReading) -> Option<u64> {
    self
      .budget_ns
      .checked_sub(reading.spent_ns.checked_sub(self.began?)?)
      .filter(|unspent| *unspent > 0)
  }

  /// The next window for a shard that ran in the one just ended, its progress marked: the budget it has not yet run.
  fn worked_to(&mut self, reading: CpuReading) -> Option<u64> {
    let unspent = self.unspent_at(reading)?;
    self.mark = Some(reading.progress);
    Some(unspent)
  }
}

/// What one attempt of an observation established: an end (an admitted task, or a final refusal), or
/// a capacity refusal to retry inside the budget — kept, so a deadline reached by a later attempt that
/// met no refusal of its own still names the refusal that held the observation up.
enum Attempt<T> {
  Ended(Result<Admitted<T>, ObserveError>),
  Retry(RtError),
}

/// Shape: the pace between retries of a capacity refusal — a tenth of a coordinator period, the tree's
/// collection-loop cadence ([`POLL_PER_PERIOD`]), so a refused observation asks again about as often as
/// the shard's own loops turn, never in a tight loop.
const PACE_NS: u64 = HEARTBEAT_NS / POLL_PER_PERIOD;

impl<T, Q> Observation<T, Q>
where
  T: Send + 'static,
  Q: FnOnce() -> Result<T, StateAccess> + Clone + Send + 'static,
{
  /// An observation of the shard `holder` names with `budget_ns` to answer; nothing is submitted
  /// until it is run.
  pub(crate) fn new(holder: SlotHolder, budget_ns: u64, question: Q) -> Self {
    Observation {
      holder,
      budget_ns,
      question,
      answer: PhantomData,
    }
  }

  /// Runs the observation to its end under its one absolute deadline: [`Self::admit`], then the
  /// admitted task's [`Admitted::answer`].
  pub fn wait(self) -> Result<T, ObserveError> {
    self.admit()?.answer()
  }

  /// Submits the question (retrying a full control channel) and reads its admission receipt
  /// (retrying a full arena) under the one absolute deadline; the calling thread parks between
  /// attempts, never spins. Returns the admitted task, whose answer is then pending.
  pub fn admit(self) -> Result<Admitted<T>, ObserveError> {
    let began = Instant::now();
    let mut deadline = began
      .checked_add(Duration::from_nanos(self.budget_ns))
      .unwrap_or(began);
    let mut time = ShardTime::new(self.holder, self.budget_ns);
    // The pace is a timed wait on a channel nobody sends on: the thread parks, it does not spin, so it
    // steals no CPU from the shard it waits on; the sender is held so the wait runs its full span.
    let (_pace_sender, pace) = channel::<()>();
    let mut attempts: u32 = 0;
    let mut held_up_by: Option<RtError> = None;
    loop {
      attempts = attempts.saturating_add(1);
      match self.attempt(attempts, began, &mut deadline, &mut time) {
        // A capacity refusal held at the deadline: retried for another window while the shard is working.
        Attempt::Ended(Err(ObserveError::Deadline { .. }))
          if held_up_by.is_some() && {
            let extended = time.extend();
            if let Some(next) = extended {
              deadline = next;
            }
            extended.is_some()
          } => {}
        Attempt::Ended(Err(ObserveError::Deadline {
          stage,
          budget_ns,
          attempts,
          waited_ns,
          last_refusal: None,
        })) => {
          // The last attempt met no refusal of its own (it was queued, or admitted, when the budget
          // ran out): the refusal that held the earlier attempts up is what the deadline names.
          return Err(ObserveError::Deadline {
            stage,
            budget_ns,
            attempts,
            waited_ns,
            last_refusal: held_up_by,
          });
        }
        Attempt::Ended(ended) => return ended,
        Attempt::Retry(refusal) => {
          held_up_by = Some(refusal);
          let _ = pace.recv_timeout(Duration::from_nanos(PACE_NS).min(remaining(deadline)));
        }
      }
    }
  }

  /// One submission and its journey to an admitted task or a refusal.
  fn attempt(
    &self,
    attempts: u32,
    began: Instant,
    deadline: &mut Instant,
    time: &mut ShardTime,
  ) -> Attempt<T> {
    let (reply_sender, reply) = channel::<Result<T, StateAccess>>();
    let question = self.question.clone();
    let submitted = slates_rt::runtime::submit_to_holder(self.holder, async move {
      let answer = question();
      if reply_sender.send(answer).is_err() {
        // The asker's wait ended first: the answer is late, discarded, and counted on the shard's
        // ledger while its state is there to count on.
        let _ = state::with_state_counted(|s| s.count(OBSERVE_LATE_REPLY, 1));
      }
    });
    let receipt = match submitted {
      Ok(receipt) => receipt,
      Err(refusal) => return self.submission_refused(refusal, attempts, began, *deadline),
    };
    // The receipt, waited for another window while the shard is working (`ShardTime`).
    let admission = loop {
      let waited = receipt.wait(remaining(*deadline));
      if waited.is_some() {
        break waited;
      }
      match time.extend() {
        Some(next) => *deadline = next,
        None => break None,
      }
    };
    match self.admitted(admission, attempts, began, *deadline) {
      Ok(task) => Attempt::Ended(Ok(Admitted {
        task,
        reply,
        began,
        deadline: *deadline,
        budget_ns: self.budget_ns,
        attempts,
        time: *time,
      })),
      Err(attempt) => attempt,
    }
  }

  /// A submission refused: a full control channel is retried inside the budget and named by the
  /// deadline past it; a gone shard and any other refusal end the observation at once.
  fn submission_refused(
    &self,
    refusal: RtError,
    attempts: u32,
    began: Instant,
    deadline: Instant,
  ) -> Attempt<T> {
    Attempt::Ended(Err(match refusal {
      RtError::ControlFull { .. } if Instant::now() < deadline => return Attempt::Retry(refusal),
      RtError::ControlFull { .. } => {
        self.deadline(ObserveStage::Submission, attempts, began, Some(refusal))
      }
      RtError::ShardGone { shard } => ObserveError::ShardGone {
        shard,
        stage: ObserveStage::Submission,
      },
      refusal => ObserveError::Submission {
        refusal,
        attempts,
        waited_ns: waited_ns(began),
      },
    }))
  }

  /// The receipt's answer: the admitted task; a full arena retried inside the budget and named by the
  /// deadline past it; any other refusal, a termination, or a request still undrained at the deadline
  /// ends the observation.
  fn admitted(
    &self,
    admission: Option<Admission>,
    attempts: u32,
    began: Instant,
    deadline: Instant,
  ) -> Result<TaskId, Attempt<T>> {
    Err(Attempt::Ended(Err(match admission {
      Some(Admission::Admitted(task)) => return Ok(task),
      Some(Admission::Refused(refusal @ RtError::TooManyTasks { .. }))
        if Instant::now() < deadline =>
      {
        return Err(Attempt::Retry(refusal));
      }
      Some(Admission::Refused(refusal @ RtError::TooManyTasks { .. })) => {
        self.deadline(ObserveStage::Admission, attempts, began, Some(refusal))
      }
      Some(Admission::Refused(refusal)) => ObserveError::Admission {
        refusal,
        attempts,
        waited_ns: waited_ns(began),
      },
      Some(Admission::Terminated) => ObserveError::Terminated {
        stage: ObserveStage::Admission,
        attempts,
      },
      None => self.deadline(ObserveStage::Admission, attempts, began, None),
    })))
  }

  /// The deadline error for `stage`.
  fn deadline(
    &self,
    stage: ObserveStage,
    attempts: u32,
    began: Instant,
    last_refusal: Option<RtError>,
  ) -> ObserveError {
    ObserveError::Deadline {
      stage,
      budget_ns: self.budget_ns,
      attempts,
      waited_ns: waited_ns(began),
      last_refusal,
    }
  }
}

impl<T> Admitted<T> {
  /// The admitted task.
  pub fn task(&self) -> TaskId {
    self.task
  }

  /// Waits for the answer under what is left of the observation's budget; the calling thread parks.
  /// A budget that elapses first cancels the task, so a starved shard does not run the question for
  /// nobody once it gets the CPU (a cancel refused under that same load is why a late reply is counted
  /// as well); a task dropped before it answered — cancelled by a shutdown — reads as terminated.
  pub fn answer(mut self) -> Result<T, ObserveError> {
    // The answer, waited for another window while the shard is working (`ShardTime`).
    let answered = loop {
      match self.reply.recv_timeout(remaining(self.deadline)) {
        Err(RecvTimeoutError::Timeout) => match self.time.extend() {
          Some(next) => self.deadline = next,
          None => break Err(RecvTimeoutError::Timeout),
        },
        other => break other,
      }
    };
    match answered {
      Ok(Ok(answer)) => Ok(answer),
      Ok(Err(access)) => Err(ObserveError::State(access)),
      Err(RecvTimeoutError::Timeout) => {
        let _ = slates_rt::runtime::cancel_task(self.task);
        Err(ObserveError::Deadline {
          stage: ObserveStage::Execution,
          budget_ns: self.budget_ns,
          attempts: self.attempts,
          waited_ns: waited_ns(self.began),
          last_refusal: None,
        })
      }
      Err(RecvTimeoutError::Disconnected) => Err(ObserveError::Terminated {
        stage: ObserveStage::Execution,
        attempts: self.attempts,
      }),
    }
  }
}

/// What is left of the budget, zero once the deadline passed.
fn remaining(deadline: Instant) -> Duration {
  deadline.saturating_duration_since(Instant::now())
}

/// Nanoseconds since `began`, saturating.
fn waited_ns(began: Instant) -> u64 {
  u64::try_from(began.elapsed().as_nanos()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
  use super::*;

  /// The reading a poll takes of each refusal: only what can clear is asked again.
  #[test]
  fn only_refusals_that_can_clear_are_not_terminal() {
    let gone = ObserveError::ShardGone {
      shard: 1,
      stage: ObserveStage::Submission,
    };
    let starved = ObserveError::Deadline {
      stage: ObserveStage::Submission,
      budget_ns: 1,
      attempts: 3,
      waited_ns: 1,
      last_refusal: Some(RtError::ControlFull { shard: 1 }),
    };
    assert!(gone.is_terminal());
    assert!(ObserveError::NoRuntime.is_terminal());
    assert!(ObserveError::State(StateAccess::Absent).is_terminal());
    assert!(!starved.is_terminal());
    assert!(!ObserveError::State(StateAccess::Borrowed).is_terminal());
    assert_eq!(starved.stage(), Some(ObserveStage::Submission));
    assert_eq!(ObserveError::NoTarget.stage(), None);
  }

  /// Shape: the budget the rule's replays run with, the liveness window an observation of a starved shard is given in
  /// `crates/server/tests/liveness.rs`.
  const BUDGET_NS: u64 = crate::daemon::LIVENESS_BUDGET_NS;
  /// Shape: the CPU time the shard had run when the wait began: two of Windows' 15.625 ms ticks, as CI's runner read.
  const BEGAN_NS: u64 = 31_250_000;
  /// Shape: the shard's progress count when the wait began.
  const MARK: u64 = 1_000;
  /// Shape: the CPU one run of the shard is charged: one Windows tick.
  const RUN_NS: u64 = 15_625_000;

  /// The rule as a wait begins.
  fn begun() -> OwnTime {
    OwnTime {
      budget_ns: BUDGET_NS,
      began: Some(BEGAN_NS),
      mark: Some(MARK),
    }
  }

  /// A clock reading: `spent_ns` of CPU run in all, progress count `progress`.
  fn at(spent_ns: u64, progress: u64) -> CpuReading {
    CpuReading { spent_ns, progress }
  }

  /// A clock that answers `readings` in turn, one a read, and nothing once they are spent.
  fn scripted(readings: Vec<CpuReading>) -> impl FnMut() -> Option<CpuReading> {
    let mut readings = readings.into_iter();
    move || readings.next()
  }

  /// The scheduler's answer of `state`, with no wait time (macOS and Windows keep none).
  fn seen(state: RunState) -> Option<RunReading> {
    Some(RunReading {
      state,
      waited_ns: None,
    })
  }

  /// §4.14, A-65. Do: end a window whose first clock read shows the shard ran a tick of CPU in it. Expect: the next
  /// window is the budget it has not yet run, and the scheduler is never asked: a shard that progresses costs no look.
  #[test]
  fn a_shard_that_ran_in_the_window_is_given_the_budget_it_has_not_run() {
    let asked = std::cell::Cell::new(0u32);
    let mut own = begun();
    let window = own.next_window_ns(scripted(vec![at(BEGAN_NS + RUN_NS, MARK + 1)]), || {
      asked.set(asked.get() + 1);
      seen(RunState::Blocked)
    });
    assert_eq!(window, Some(BUDGET_NS - RUN_NS));
    assert_eq!(asked.get(), 0);
  }

  /// §4.14, A-228. Do: end a window in which the shard's clock stood still at both reads, once with the scheduler
  /// finding it waiting for a CPU, once blocked, once with nothing read. Expect: the starved shard is given its whole
  /// unspent budget again; the blocked one and the unread one end the wait.
  #[test]
  fn a_starved_shard_is_given_the_window_again_and_a_blocked_one_ends_the_wait() {
    let still = || scripted(vec![at(BEGAN_NS, MARK), at(BEGAN_NS, MARK)]);
    assert_eq!(
      begun().next_window_ns(still(), || seen(RunState::Runnable)),
      Some(BUDGET_NS)
    );
    assert_eq!(
      begun().next_window_ns(still(), || seen(RunState::Blocked)),
      None
    );
    assert_eq!(begun().next_window_ns(still(), || None), None);
  }

  /// §4.14, A-228; CI's Windows runner, diag run 42671057142. Do: end a window in which the shard's clock stood still at
  /// the first read, then the shard ran (released, it answered and parked) before the scheduler looked, so the scheduler
  /// finds it blocked and the clock read after shows the run; then end the next window with its clock standing still at
  /// the new mark and the scheduler finding it blocked. Expect: the first window gives the shard its unspent budget again,
  /// since it ran; the second ends the wait, since it then sat a window blocked. Before, the rule read the clock only
  /// before the scheduler and gave up on the shard at the first window: an observation already answered was given up.
  #[test]
  fn a_shard_that_runs_and_blocks_between_the_reads_is_given_the_window() {
    let mut own = begun();
    let ran = at(BEGAN_NS + RUN_NS, MARK + 1);
    let window = own.next_window_ns(scripted(vec![at(BEGAN_NS, MARK), ran]), || {
      seen(RunState::Blocked)
    });
    assert_eq!(window, Some(BUDGET_NS - RUN_NS));
    let window = own.next_window_ns(scripted(vec![ran, ran]), || seen(RunState::Blocked));
    assert_eq!(window, None);
  }

  /// §4.14, A-65. Do: end a window in which a busy shard ran past its budget, once seen at the first clock read, once
  /// only at the read after the scheduler's answer. Expect: both end the wait: a shard that has run the budget is not
  /// given more, whichever read shows it.
  #[test]
  fn a_shard_that_has_run_its_budget_ends_the_wait() {
    let spent = at(BEGAN_NS + BUDGET_NS, MARK + 1);
    assert_eq!(
      begun().next_window_ns(scripted(vec![spent]), || seen(RunState::Runnable)),
      None
    );
    assert_eq!(
      begun().next_window_ns(scripted(vec![at(BEGAN_NS, MARK), spent]), || {
        seen(RunState::Runnable)
      }),
      None
    );
  }
}
