//! A push's checks made a turn at a time (A-719; §4.17.2 "A push", items 4 and 5; §4.3, no step
//! longer than its quantum): every pushed object checked as git checks a push ([`super::fsck`]),
//! then the connectivity walk ([`Walk`]), each a pass over the pack's objects in pack order that a
//! turn advances for its quantum and at least one object. Found by use: stock git's push of git/git
//! (607 MiB) ran both whole in the step that took the pack's last bytes, and its shard ran past the
//! watchdog's window. [`super::check`] runs the same passes whole, in one turn with no end.

use std::borrow::Cow;

use super::connect::{Connected, Stopped, Walk, WalkStep, unbounded};
use super::fsck::{self, SpecialBlob};
use super::{
  Checked, HiddenRefs, PackRefusal, PackRefused, Pushed, Refusal, Repository, Request,
  UNPACKER_ERROR, charged_vec, early_refusal, refused,
};
use crate::Admission;
use crate::object_name::Kind;
use crate::object_name::ObjectId;
use crate::store::hook_view::Quantum;
use crate::unpack::{Contents, Kept};

/// Where a push's checks stand.
#[derive(Debug)]
enum Stage {
  /// The objects checked as git checks a push, up to this one.
  Fsck {
    /// The next object.
    at: usize,
  },
  /// Special dotfile blobs (`.gitmodules`, `.gitattributes`) referenced by trees, checked one a unit.
  SpecialBlobs {
    /// Each object's name and position, sorted by name.
    by_id: Vec<(ObjectId, usize)>,
    /// The next entry in `special`.
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
  /// The pack's objects' places, or why it was refused; `None` when no pack was due.
  unpacked: Option<Result<Kept, PackRefused>>,
  /// Special dotfile entries collected while checking trees, validated in `Stage::SpecialBlobs`.
  special: Vec<SpecialBlob>,
  /// Where the checks stand.
  stage: Stage,
  /// Where the check last stopped for a let-go window `(stage_tag, at, offset)` so a failed
  /// bring-back terminates instead of looping.
  stopped: Option<(u8, usize, u64)>,
}

impl Checking {
  /// The request whose push is checked.
  pub(crate) fn request(&self) -> &Request {
    &self.request
  }

  /// The checks of `request` and its pack, `unpacked` (none when no pack was due), not yet begun.
  pub fn new(request: Request, unpacked: Option<Result<Kept, PackRefused>>) -> Checking {
    let stage = match &unpacked {
      Some(Ok(_)) => Stage::Fsck { at: 0 },
      Some(Err(_)) => Stage::Refused,
      // No pack: a walk of nothing, which judges each new tip by what the repository holds.
      None => Stage::Fsck { at: 0 },
    };
    Checking {
      request,
      unpacked,
      special: Vec::new(),
      stage,
      stopped: None,
    }
  }

  /// The checks of `request` and its pack, `unpacked`, whose every object was checked as git checks
  /// a push as it was kept (the session's, A-719 part 3): the connectivity walk alone to make.
  pub fn fscked(request: Request, unpacked: Option<Result<Kept, PackRefused>>) -> Checking {
    Checking::fscked_with_special(request, unpacked, Vec::new())
  }

  /// Like [`Checking::fscked`], carrying any `.gitmodules` and `.gitattributes` entries collected
  /// from trees as they were kept so they are checked once the whole pack is unpacked.
  pub(crate) fn fscked_with_special(
    request: Request,
    unpacked: Option<Result<Kept, PackRefused>>,
    special: Vec<SpecialBlob>,
  ) -> Checking {
    let mut checking = Checking::new(request, unpacked);
    checking.special = special;
    if let (Stage::Fsck { at }, Some(Ok(kept))) = (&mut checking.stage, &checking.unpacked) {
      *at = kept.objects.len();
    }
    checking
  }

  /// Whether every check is made.
  pub fn is_done(&self) -> bool {
    matches!(self.stage, Stage::Connected { .. } | Stage::Refused)
  }

  /// One turn of the checks against `repository`, the objects' bytes read from `contents`, for
  /// `quantum` and at least one unit of work, the tables they make charged to `admission` first:
  /// whether every check is made.
  pub fn turn<R: Repository + ?Sized, C: Contents>(
    &mut self,
    (repository, contents): (&R, &C),
    admission: &mut Admission,
    quantum: Quantum<'_>,
  ) -> Result<bool, Refusal> {
    let Quantum { clock, ns } = quantum;
    let begun = clock.now_ns();
    let mut spent = 0_u64;
    while !self.is_done() {
      let left = Quantum {
        clock: &mut *clock,
        ns: ns.saturating_sub(spent),
      };
      if self.unit((repository, contents), admission, left)? {
        break;
      }
      spent = clock.now_ns().saturating_sub(begun);
      if spent >= ns {
        break;
      }
    }
    Ok(self.is_done())
  }

  /// One unit of the checks: one object checked as git checks a push, one special dotfile blob
  /// checked, the objects indexed, or a turn of the walk for what is `left` of the quantum.
  /// Returns `Ok(true)` when paused for a let-go window so the driver brings it back before retrying.
  fn unit<R: Repository + ?Sized, C: Contents>(
    &mut self,
    (repository, contents): (&R, &C),
    admission: &mut Admission,
    left: Quantum<'_>,
  ) -> Result<bool, Refusal> {
    let Checking {
      request,
      unpacked,
      special,
      stage,
      stopped,
    } = self;
    let objects = match unpacked {
      Some(Ok(unpacked)) => unpacked.objects.as_slice(),
      _ => &[],
    };
    let contents: &dyn Contents = contents;
    match stage {
      Stage::Fsck { at } => match objects.get(*at) {
        Some(object) => {
          match fsck_one(object, *at, (repository, contents, request.format), special) {
            Some(finding) => {
              if let Some(away) = contents.away()
                && *stopped != Some((0, *at, away))
              {
                *stopped = Some((0, *at, away));
                return Ok(true);
              }
              *stopped = None;
              let (count, id) = (objects.len(), object.id);
              refuse_pack(unpacked, stage, count, id, finding);
            }
            None => {
              *stopped = None;
              *at = at.saturating_add(1);
            }
          }
        }
        None => {
          *stopped = None;
          let pushed = Pushed::new(objects, Some(contents), admission)?;
          if special.is_empty() {
            let walk = Walk::new(request, &pushed, admission)?;
            *stage = Stage::Walk {
              by_id: pushed.by_id,
              walk,
            };
          } else {
            special.sort_unstable();
            special.dedup();
            *stage = Stage::SpecialBlobs {
              by_id: pushed.by_id,
              at: 0,
            };
          }
        }
      },
      Stage::SpecialBlobs { by_id, at } => match special.get(*at).copied() {
        Some(entry) => {
          let finding = check_special_entry(entry, objects, by_id, contents, repository);
          if let Some(away) = contents.away().or_else(|| repository.away())
            && *stopped != Some((1, *at, away))
          {
            *stopped = Some((1, *at, away));
            return Ok(true);
          }
          *stopped = None;
          if let Some(finding) = finding {
            let count = objects.len();
            refuse_pack(unpacked, stage, count, entry.id, finding);
          } else {
            *at = at.saturating_add(1);
          }
        }
        None => {
          *stopped = None;
          let pushed = Pushed {
            objects,
            by_id: core::mem::take(by_id),
            contents: Some(contents),
          };
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
          contents: Some(contents),
        };
        let done = walk.turn((request, &pushed, repository), admission, left);
        let (paused, connected) = match done {
          Ok(WalkStep::Done) => (false, Some(walk.connected(request, &pushed, repository))),
          Ok(WalkStep::Away) => (true, None),
          Ok(WalkStep::More) => (false, None),
          Err(refusal) => (false, Some(Err(refusal))),
        };
        *by_id = pushed.by_id;
        if let Some(connected) = connected {
          *stage = Stage::Connected {
            by_id: core::mem::take(by_id),
            connected: connected?,
          };
        }
        if paused {
          return Ok(true);
        }
      }
      Stage::Connected { .. } | Stage::Refused => {}
    }
    Ok(false)
  }

  /// The checked push, every check made first in one turn with no end ([`super::check`]'s), each
  /// command's early refusal against `hidden` and the outcomes' table charged to `admission`.
  pub fn checked<R, C, H>(
    mut self,
    (repository, contents): (&R, &C),
    hidden: &H,
    admission: &mut Admission,
  ) -> Result<Checked, Refusal>
  where
    R: Repository + ?Sized,
    C: Contents,
    H: HiddenRefs + ?Sized,
  {
    let mut stopped = Stopped;
    while !self.turn((repository, contents), admission, unbounded(&mut stopped))? {}
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
      Stage::Refused | Stage::Fsck { .. } | Stage::SpecialBlobs { .. } | Stage::Walk { .. } => {
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

/// Marks the pack refused for an fsck finding on `id`.
fn refuse_pack(
  unpacked: &mut Option<Result<Kept, PackRefused>>,
  stage: &mut Stage,
  count: usize,
  id: ObjectId,
  finding: crate::objects::FsckId,
) {
  let counted = u32::try_from(count).ok();
  *unpacked = Some(Err(PackRefused {
    objects: counted,
    why: PackRefusal::Fsck { id, finding },
  }));
  *stage = Stage::Refused;
}

/// Checks one `.gitmodules` or `.gitattributes` entry against the pushed pack first, then `repository`.
fn check_special_entry<R: Repository + ?Sized>(
  entry: SpecialBlob,
  objects: &[crate::unpack::Placed],
  by_id: &[(ObjectId, usize)],
  contents: &dyn Contents,
  repository: &R,
) -> Option<crate::objects::FsckId> {
  let lookup = |id: &ObjectId| {
    if let Ok(pos) = by_id.binary_search_by(|(known, _)| known.cmp(id))
      && let Some(&(_, at)) = by_id.get(pos)
      && let Some(placed) = objects.get(at)
      && let Some(bytes) = contents.content(at, id)
    {
      return Some((placed.kind, bytes));
    }
    repository
      .object(id)
      .map(|(kind, bytes)| (kind, Cow::Borrowed(bytes)))
  };
  fsck::check_special(entry, lookup, |id| repository.holds(id))
}

/// What a push's check finds against the object `placed` at position `at`, its bytes read from
/// `contents`: a blob passes unread until referenced as a special dotfile by a tree; an object
/// whose collision-checked identity (SHA-1DC / SHA-256) already lives in `repository` passed fsck
/// on its original admission; one whose bytes are not lent is refused as git refuses an object it
/// cannot read.
fn fsck_one<R: Repository + ?Sized>(
  placed: &crate::unpack::Placed,
  at: usize,
  (repository, contents, format): (&R, &dyn Contents, crate::object_name::ObjectFormat),
  special: &mut Vec<SpecialBlob>,
) -> Option<crate::objects::FsckId> {
  if placed.kind == Kind::Blob || repository.holds(&placed.id) {
    return None;
  }
  match contents.content(at, &placed.id) {
    Some(content) => fsck::check_object(placed.kind, &content, format, special),
    None => Some(crate::objects::FsckId::BadObjectSha1),
  }
}
