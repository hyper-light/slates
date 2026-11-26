//! A verdict computed a slice at a time (A-920 piece 3c; A-658's [`Green::compute`]): the increment's ops resolved an op
//! a unit ([`Green::resolve_some`]), then its units decided a unit at a time (`deciding`), each step's reads of
//! unlent named blocks noted (`crate::unlent`) and kept, so a verdict that read one is refused naming the least, as the
//! whole computation refuses it. [`Green::compute`] is its steps carried to their end; a judge sliced so is
//! [`Green::begin_judging`]: the rejected-result cache's answer, the computation, then its one write, the verdict held
//! (`Green::hold_unlent`), in its last step.
//!
//! **Contract.** Between its begin and its end neither the green nor the increment changes: an import batch's scratch
//! member and its pending work under the landing turn. A verdict computed at the head it began at is held there.

use std::collections::BTreeSet;

use super::deciding::Deciding;
use super::{ComputeRefused, Computed, Green, Increment, Judgment, Reached, Resolved, position};
use crate::named::Identity;

/// A verdict being computed a slice at a time ([`Green::begin_computing`], [`Green::computing_step`]).
#[derive(Debug)]
pub struct Computing {
  /// Where it stands.
  phase: ComputePhase,
  /// The head it is computed against.
  at: u64,
  /// The fold floor of the green computing it.
  floor: u64,
  /// The unlent named blocks its steps read so far.
  unlent: BTreeSet<Identity>,
}

/// Where a computation stands between its steps.
enum ComputePhase {
  /// Answered before any op was resolved; taken by the step that answers it.
  Ended(Option<Result<Computed, ComputeRefused>>),
  /// Resolving the ops from the next one.
  Resolving(usize, Box<Resolved>),
  /// Deciding the units.
  Deciding(Box<(Resolved, Deciding, position::Units)>),
  /// What the units reach, a unit a unit ([`Reached::of`]'s walk; A-920 piece 3f).
  Reaching(Box<Reaching>),
}

/// The units decided, what they reach being found from the next ([`ComputePhase::Reaching`]).
struct Reaching {
  /// The resolution and the decision's sets, let go a unit a unit before any unit is reached (A-920 piece 3h).
  releasing: Option<Box<(Resolved, Deciding)>>,
  units: Vec<position::Unit>,
  at: usize,
  windows: Vec<crate::ConflictWindow>,
  applied: Vec<crate::chain_entry::Applied>,
}

impl core::fmt::Debug for ComputePhase {
  /// Where it stands: its phase, and how far a walk of the ops has come.
  fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
    match self {
      ComputePhase::Ended(answer) => f.debug_tuple("Ended").field(answer).finish(),
      ComputePhase::Resolving(at, _) => f.debug_tuple("Resolving").field(at).finish(),
      ComputePhase::Deciding(_) => f.write_str("Deciding"),
      ComputePhase::Reaching(reaching) => f.debug_tuple("Reaching").field(&reaching.at).finish(),
    }
  }
}

/// A judge being taken a slice at a time ([`Green::begin_judging`], [`Green::judging_step`]).
#[derive(Debug)]
pub struct Judging {
  /// The cache's answer, or the computation it holds once computed.
  phase: JudgingPhase,
}

/// Where a judge stands between its steps.
enum JudgingPhase {
  /// The rejected-result cache answered it.
  Answered(Option<Box<Judgment>>),
  /// Its verdict being computed.
  Computing(Box<Computing>),
  /// Its verdict being held, a unit at a time (A-920 piece 3g), with the unlent named blocks its hold has read.
  Holding(Box<(super::Holding, BTreeSet<Identity>)>),
}

impl core::fmt::Debug for JudgingPhase {
  /// Where it stands.
  fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
    match self {
      JudgingPhase::Answered(answer) => f.debug_tuple("Answered").field(answer).finish(),
      JudgingPhase::Computing(computing) => f.debug_tuple("Computing").field(computing).finish(),
      JudgingPhase::Holding(_) => f.write_str("Holding"),
    }
  }
}

impl Green {
  /// Begins computing `inc`'s verdict a slice at a time ([`Green::computing_step`]): answered at once where the green
  /// accepted the increment before, or does not hold every version it is judged over, or its runs are unread.
  pub fn begin_computing(&self, inc: &Increment) -> Computing {
    let (at, floor) = (self.head(), self.floor());
    let ended = |answer| ComputePhase::Ended(Some(answer));
    let phase = if let Some(version) = self.accepted.get(&inc.id) {
      ended(Ok(Computed {
        id: inc.id,
        base: inc.base,
        at,
        floor,
        reached: Reached::Before(*version),
      }))
    } else if inc.base < self.deltas_from {
      ended(Err(ComputeRefused::BaseUnavailable {
        base: inc.base,
        floor: self.deltas_from,
      }))
    } else if inc.base > at {
      ended(Err(ComputeRefused::BaseAhead {
        base: inc.base,
        head: at,
      }))
    } else if let Some(run) = inc.unread() {
      ended(Err(ComputeRefused::ContentUnread {
        identity: run.identity,
      }))
    } else {
      ComputePhase::Resolving(0, Box::default())
    };
    Computing {
      phase,
      at,
      floor,
      unlent: BTreeSet::new(),
    }
  }

  /// Takes `computing` on, a unit at least, until `spent`: once it ends, the verdict [`Green::compute`] computes,
  /// refused [`ComputeRefused::ContentUnread`] naming the least unlent named block any of its steps read.
  pub fn computing_step(
    &self,
    inc: &Increment,
    computing: &mut Computing,
    spent: &mut dyn FnMut() -> bool,
  ) -> Option<Result<Computed, ComputeRefused>> {
    let computed = self.computing_some(inc, computing, spent)?;
    Some(match computing.unlent.first() {
      Some(identity) => Err(ComputeRefused::ContentUnread {
        identity: *identity,
      }),
      None => computed,
    })
  }

  /// [`Green::computing_step`] with its reads of unlent named blocks noted and kept, not refused: what
  /// [`Green::compute_read`] carries to its end.
  pub(super) fn computing_some(
    &self,
    inc: &Increment,
    computing: &mut Computing,
    spent: &mut dyn FnMut() -> bool,
  ) -> Option<Result<Computed, ComputeRefused>> {
    let (stepped, unlent) = crate::unlent::scope(|| self.computing_phase(inc, computing, spent));
    computing.unlent.extend(unlent);
    stepped
  }

  /// One step of `computing`'s phase.
  fn computing_phase(
    &self,
    inc: &Increment,
    computing: &mut Computing,
    spent: &mut dyn FnMut() -> bool,
  ) -> Option<Result<Computed, ComputeRefused>> {
    match &mut computing.phase {
      ComputePhase::Ended(answer) => answer.take(),
      ComputePhase::Resolving(at, resolved) => {
        if !Green::resolve_some(inc, (resolved, at), spent) {
          return None;
        }
        let mut resolved = core::mem::take(resolved);
        resolved.finish_resolving();
        if let Some((path, case)) = self.copy_refusal(&resolved) {
          return Some(Err(ComputeRefused::CopyRefused { path, case }));
        }
        computing.phase = ComputePhase::Deciding(Box::new((
          *resolved,
          Deciding::default(),
          position::Units::first(),
        )));
        None
      }
      ComputePhase::Deciding(deciding) => {
        let (resolved, cursor, units) = &mut **deciding;
        if !self.decide_some(inc, resolved, (units, cursor), spent) {
          return None;
        }
        let ComputePhase::Deciding(decided) =
          core::mem::replace(&mut computing.phase, ComputePhase::Ended(None))
        else {
          return None;
        };
        let (resolved, cursor, units) = *decided;
        computing.phase = ComputePhase::Reaching(Box::new(Reaching {
          releasing: Some(Box::new((resolved, cursor))),
          units: units.into_units().0,
          at: 0,
          windows: Vec::new(),
          applied: Vec::new(),
        }));
        None
      }
      ComputePhase::Reaching(reaching) => {
        let released = match reaching.releasing.as_deref_mut() {
          None => true,
          Some((resolved, cursor)) => loop {
            if !resolved.release_one() && !cursor.release_one() {
              break true;
            }
            if spent() {
              break false;
            }
          },
        };
        if !released {
          return None;
        }
        reaching.releasing = None;
        while let Some(unit) = reaching.units.get(reaching.at) {
          Reached::reach_unit(unit, (&mut reaching.windows, &mut reaching.applied));
          reaching.at = reaching.at.saturating_add(1);
          if reaching.at < reaching.units.len() && spent() {
            return None;
          }
        }
        let Reaching {
          units,
          windows,
          applied,
          ..
        } = core::mem::replace(
          &mut **reaching,
          Reaching {
            releasing: None,
            units: Vec::new(),
            at: 0,
            windows: Vec::new(),
            applied: Vec::new(),
          },
        );
        Some(Ok(Computed {
          id: inc.id,
          base: inc.base,
          at: computing.at,
          floor: computing.floor,
          reached: Reached::Verdict {
            units,
            applied,
            windows,
          },
        }))
      }
    }
  }

  /// [`Green::compute`] with its reads of unlent named blocks noted, not refused: what [`Green::unlent_reads`] walks.
  pub(super) fn compute_read(&self, inc: &Increment) -> Result<Computed, ComputeRefused> {
    let mut computing = self.begin_computing(inc);
    loop {
      if let Some(computed) = self.computing_some(inc, &mut computing, &mut || false) {
        return computed;
      }
    }
  }

  /// Begins judging `inc` as [`Green::judge_named_reads_later`] judges it, a slice at a time
  /// ([`Green::judging_step`]): a conflict the rejected-result cache holds answered at once, unless the same increment
  /// was accepted since; else its verdict computed a slice at a time ([`Green::begin_computing`]).
  pub fn begin_judging(&self, inc: &Increment) -> Judging {
    let cached = (!self.accepted.contains_key(&inc.id))
      .then(|| self.rejected.get(&inc.id))
      .flatten();
    let phase = match cached {
      Some(windows) => JudgingPhase::Answered(Some(Box::new(Judgment::Conflict {
        windows: windows.clone(),
      }))),
      None => JudgingPhase::Computing(Box::new(self.begin_computing(inc))),
    };
    Judging { phase }
  }

  /// Takes `judging` on, a unit at least, until `spent`: once it ends, [`Green::judge_named_reads_later`]'s judgment,
  /// the work's named runs' bytes taken into an accepted verdict; held, or a conflict cached, in this last step alone.
  pub fn judging_step(
    &mut self,
    inc: &mut Increment,
    judging: &mut Judging,
    spent: &mut dyn FnMut() -> bool,
  ) -> Option<Judgment> {
    let mut judgment = match &mut judging.phase {
      JudgingPhase::Answered(answer) => return answer.take().map(|answer| *answer),
      JudgingPhase::Computing(computing) => match self.computing_step(inc, computing, spent)? {
        Err(refused) => self.refused_judgment(refused),
        Ok(computed) => {
          let (begun, unlent) = crate::unlent::scope(|| self.hold_begin(inc, computed, false));
          match begun {
            Err(answer) => self.unlent_rule(inc, answer, &unlent),
            Ok(holding) => {
              judging.phase = JudgingPhase::Holding(Box::new((holding, unlent)));
              return None;
            }
          }
        }
      },
      JudgingPhase::Holding(held) => {
        let (holding, unlent) = &mut **held;
        let (stepped, more) = crate::unlent::scope(|| self.holding_step(inc, holding, spent));
        unlent.extend(more);
        let judgment = stepped?;
        let unlent = core::mem::take(unlent);
        self.unlent_rule(inc, judgment, &unlent)
      }
    };
    if let Judgment::Accepted(held) = &mut judgment {
      held.read = core::mem::take(&mut inc.named.read.0);
    }
    Some(judgment)
  }
}
