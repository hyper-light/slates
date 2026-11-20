//! A push's checks made a turn at a time (A-719; §4.17.2 "A push", items 4 and 5; §4.3, no step
//! longer than its quantum): every pushed object checked as git checks a push ([`super::fsck`]),
//! then the connectivity walk ([`Walk`]), each a pass over the pack's objects in pack order that a
//! turn advances for its quantum and at least one object. Found by use: stock git's push of git/git
//! (607 MiB) ran both whole in the step that took the pack's last bytes, and its shard ran past the
//! watchdog's window. [`super::check`] runs the same passes whole, in one turn with no end.

use super::connect::{Connected, Stopped, Walk, unbounded};
use super::fsck;
use super::{
  Checked, HiddenRefs, PackRefusal, PackRefused, Pushed, Refusal, Repository, Request,
  UNPACKER_ERROR, charged_vec, early_refusal, refused,
};
use crate::Admission;
use crate::object_name::ObjectId;
use crate::store::hook_view::Quantum;
use crate::unpack::Unpacked;

/// Where a push's checks stand.
#[derive(Debug)]
enum Stage {
  /// The objects checked as git checks a push, up to this one.
  Fsck {
    /// The next object.
    at: usize,
  },
  /// The connectivity walk under way, over the objects indexed by name.
  Walk {
    /// Each object's name and position, sorted by name.
    by_id: Vec<(ObjectId, usize)>,
    /// The walk.
    walk: Walk,
  },
  /// Every check made: each command's connectivity.
  Connected {
    /// Each object's name and position, sorted by name.
    by_id: Vec<(ObjectId, usize)>,
    /// Each command's connectivity, in the order sent.
    connected: Vec<Connected>,
  },
  /// The pack was refused: every command `unpacker error`.
  Refused,
}

/// A read push being checked a turn at a time (the module docs).
#[derive(Debug)]
pub struct Checking {
  /// The request.
  request: Request,
  /// The pack's objects, or why it was refused; `None` when no pack was due.
  unpacked: Option<Result<Unpacked, PackRefused>>,
  /// Where the checks stand.
  stage: Stage,
}

impl Checking {
  /// The checks of `request` and its pack, `unpacked` (none when no pack was due), not yet begun.
  pub fn new(request: Request, unpacked: Option<Result<Unpacked, PackRefused>>) -> Checking {
    let stage = match &unpacked {
      Some(Ok(_)) => Stage::Fsck { at: 0 },
      Some(Err(_)) => Stage::Refused,
      // No pack: a walk of nothing, which judges each new tip by what the repository holds.
      None => Stage::Fsck { at: 0 },
    };
    Checking {
      request,
      unpacked,
      stage,
    }
  }

  /// Whether every check is made.
  pub fn is_done(&self) -> bool {
    matches!(self.stage, Stage::Connected { .. } | Stage::Refused)
  }

  /// One turn of the checks against `repository`, for `quantum` and at least one unit of work, the
  /// tables they make charged to `admission` first: whether every check is made.
  pub fn turn<R: Repository + ?Sized>(
    &mut self,
    repository: &R,
    admission: &mut Admission,
    quantum: Quantum<'_>,
  ) -> Result<bool, Refusal> {
    let Quantum { clock, ns } = quantum;
    let begun = clock.now_ns();
    while !self.is_done() {
      let spent = clock.now_ns().saturating_sub(begun);
      let left = Quantum {
        clock: &mut *clock,
        ns: ns.saturating_sub(spent),
      };
      self.unit(repository, admission, left)?;
      if clock.now_ns().saturating_sub(begun) >= ns {
        break;
      }
    }
    Ok(self.is_done())
  }

  /// One unit of the checks: one object checked as git checks a push, the objects indexed, or a
  /// turn of the walk for what is `left` of the quantum.
  fn unit<R: Repository + ?Sized>(
    &mut self,
    repository: &R,
    admission: &mut Admission,
    left: Quantum<'_>,
  ) -> Result<(), Refusal> {
    let Checking {
      request,
      unpacked,
      stage,
    } = self;
    let objects = match unpacked {
      Some(Ok(unpacked)) => unpacked.objects.as_slice(),
      _ => &[],
    };
    match stage {
      Stage::Fsck { at } => match objects.get(*at) {
        Some(object) => match fsck::refusal(object, request.format) {
          Some(finding) => {
            let counted = u32::try_from(objects.len()).ok();
            *unpacked = Some(Err(PackRefused {
              objects: counted,
              why: PackRefusal::Fsck {
                id: object.id,
                finding,
              },
            }));
            *stage = Stage::Refused;
          }
          None => *at = at.saturating_add(1),
        },
        None => {
          let pushed = Pushed::new(objects, admission)?;
          let walk = Walk::new(request, &pushed, admission)?;
          *stage = Stage::Walk {
            by_id: pushed.by_id,
            walk,
          };
        }
      },
      Stage::Walk { by_id, walk } => {
        let pushed = Pushed {
          objects,
          by_id: core::mem::take(by_id),
        };
        let done = walk.turn((request, &pushed, repository), admission, left);
        let connected = match done {
          Ok(true) => Some(walk.connected(request, &pushed, repository)),
          Ok(false) => None,
          Err(refusal) => Some(Err(refusal)),
        };
        *by_id = pushed.by_id;
        if let Some(connected) = connected {
          *stage = Stage::Connected {
            by_id: core::mem::take(by_id),
            connected: connected?,
          };
        }
      }
      Stage::Connected { .. } | Stage::Refused => {}
    }
    Ok(())
  }

  /// The checked push, every check made first in one turn with no end ([`super::check`]'s), each
  /// command's early refusal against `hidden` and the outcomes' table charged to `admission`.
  pub fn checked<R, H>(
    mut self,
    repository: &R,
    hidden: &H,
    admission: &mut Admission,
  ) -> Result<Checked, Refusal>
  where
    R: Repository + ?Sized,
    H: HiddenRefs + ?Sized,
  {
    let mut stopped = Stopped;
    while !self.turn(repository, admission, unbounded(&mut stopped))? {}
    let count = self.request.commands.len();
    let mut early = charged_vec(count, admission)?;
    let by_id = match self.stage {
      Stage::Connected { by_id, connected } => {
        early.extend(
          self
            .request
            .commands
            .iter()
            .zip(&connected)
            .map(|(command, connected)| early_refusal(command, *connected, hidden)),
        );
        by_id
      }
      Stage::Refused | Stage::Fsck { .. } | Stage::Walk { .. } => {
        early.extend((0..count).map(|_| Some(refused(UNPACKER_ERROR, None))));
        Vec::new()
      }
    };
    Ok(Checked {
      request: self.request,
      unpacked: self.unpacked,
      early,
      by_id,
    })
  }
}
