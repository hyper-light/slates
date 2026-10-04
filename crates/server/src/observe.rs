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

use slates_rt::{Admission, RtError, SlotHolder, TaskId};

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
/// window just ended. A starved shard, runnable but given little CPU, keeps consuming some and is answered; a wedged
/// one, which consumes none, ends the observation at the end of the first whole window it spends without CPU (one
/// window if it was wedged when the observation began, two if it wedged partway through the first); a busy one that never answers ends it once it has run
/// the budget, which an uncontended shard does at the wall clock's pace. A reading at or below the mark (a thread id the
/// OS reused once the shard's thread had ended, which reads lower) ends it too. Under Miri, which has no thread
/// clock, the wall budget stands alone.
#[derive(Clone, Copy, Debug)]
struct ShardTime {
  holder: SlotHolder,
  budget_ns: u64,
  /// The CPU time the shard had run when the observation began.
  began: Option<u64>,
  /// The shard's progress at the last window's end (first: at the observation's start); it only rises.
  mark: Option<u64>,
}

impl ShardTime {
  fn new(holder: SlotHolder, budget_ns: u64) -> Self {
    let began = slates_rt::registry::shard_cpu(holder);
    ShardTime {
      holder,
      budget_ns,
      began: began.map(|reading| reading.spent_ns),
      mark: began.map(|reading| reading.progress),
    }
  }

  /// The next wall deadline when the shard worked through the window just ended within its budget; `None` ends the
  /// observation.
  fn extend(&mut self) -> Option<Instant> {
    let now = slates_rt::registry::shard_cpu(self.holder)?;
    let unspent = self
      .budget_ns
      .checked_sub(now.spent_ns.checked_sub(self.began?)?)
      .filter(|unspent| *unspent > 0)?;
    if self.mark.is_none_or(|mark| now.progress <= mark) {
      return None;
    }
    self.mark = Some(now.progress);
    // The budget the shard has not yet run, as wall time: an uncontended shard runs at the wall clock's pace and has
    // next to none left; a starved one has most of it.
    Instant::now().checked_add(Duration::from_nanos(unspent))
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
    let mut deadline = began + Duration::from_nanos(self.budget_ns);
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
        let _ = state::with_state(|s| *s.refusals.entry(OBSERVE_LATE_REPLY).or_insert(0) += 1);
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
}
