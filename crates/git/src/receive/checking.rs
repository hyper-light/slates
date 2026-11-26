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
  Checked, HiddenRefs, IdIndex, PackRefusal, PackRefused, Pushed, Refusal, Repository, Request,
  UNPACKER_ERROR, charged_vec, early_refusal, refused,
};
use crate::Admission;
use crate::object_name::Kind;
use crate::object_name::ObjectId;
use crate::store::hook_view::Quantum;
use crate::unpack::{Contents, Kept};

/// Format: branch ref prefix whose targets git requires to be commits (`refs.c` / `receive-pack.c`).
const HEADS_PREFIX: &[u8] = b"refs/heads/";

/// Stage tag for [`Checking::stopped`] so a failed bring-back terminates instead of looping.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StopStage {
  /// Object-level fsck pass.
  Fsck,
  /// Special dotfile blob pass (`.gitmodules`, `.gitattributes`).
  SpecialBlobs,
  /// Branch ref (`refs/heads/*`) commit-target validation pass.
  BranchTargets,
}

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
    /// Open-addressed O(1) index from [`ObjectId`] to pack position.
    by_id: IdIndex,
    /// The next entry in `special`.
    at: usize,
  },
  /// The connectivity walk under way, over the objects indexed by name.
  Walk {
    /// Open-addressed O(1) index from [`ObjectId`] to pack position.
    by_id: IdIndex,
    /// The walk.
    walk: Walk,
  },
  /// Verifies every non-delete `refs/heads/*` command resolves to a [`Kind::Commit`], one command
  /// a unit so repository lookups hitting a let-go pack window can pause and retry cleanly.
  BranchTargets {
    /// Open-addressed O(1) index from [`ObjectId`] to pack position.
    by_id: IdIndex,
    /// Each command's connectivity, in the order sent.
    connected: Vec<Connected>,
    /// Whether each command is a non-delete `refs/heads/*` pointing to a non-commit.
    non_commit: Vec<bool>,
    /// The next command index to inspect.
    at: usize,
  },
  /// Every check made: each command's connectivity and branch-target validation.
  Connected {
    /// Open-addressed O(1) index from [`ObjectId`] to pack position.
    by_id: IdIndex,
    /// Each command's connectivity, in the order sent.
    connected: Vec<Connected>,
    /// Whether each command is a non-delete `refs/heads/*` pointing to a non-commit.
    non_commit: Vec<bool>,
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
  /// Where the check last stopped for a let-go window `(stage, at, offset)` so a failed
  /// bring-back terminates instead of looping.
  stopped: Option<(StopStage, usize, u64)>,
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
  /// checked, the objects indexed, a turn of the walk for what is `left` of the quantum, or one
  /// branch target verified as a commit.
  /// Returns `Ok(true)` when paused for a let-go window so the driver brings it back before retrying.
  fn unit<R: Repository + ?Sized, C: Contents>(
    &mut self,
    (repository, contents): (&R, &C),
    admission: &mut Admission,
    left: Quantum<'_>,
  ) -> Result<bool, Refusal> {
    let contents: &dyn Contents = contents;
    match &self.stage {
      Stage::Fsck { .. } => self.unit_fsck(repository, contents, admission),
      Stage::SpecialBlobs { .. } => self.unit_special(repository, contents, admission),
      Stage::Walk { .. } => self.unit_walk(repository, contents, admission, left),
      Stage::BranchTargets { .. } => Ok(self.unit_branch_targets(repository)),
      Stage::Connected { .. } | Stage::Refused => Ok(false),
    }
  }

  /// Advances `Stage::BranchTargets` by one command, or transitions to `Stage::Connected`.
  fn unit_branch_targets<R: Repository + ?Sized>(&mut self, repository: &R) -> bool {
    let Checking {
      request,
      unpacked,
      stage,
      stopped,
      ..
    } = self;
    let objects = match unpacked {
      Some(Ok(unpacked)) => unpacked.objects.as_slice(),
      _ => &[],
    };
    let Stage::BranchTargets {
      by_id,
      connected,
      non_commit,
      at,
    } = stage
    else {
      return false;
    };
    let Some(command) = request.commands.get(*at) else {
      *stopped = None;
      *stage = Stage::Connected {
        by_id: core::mem::take(by_id),
        connected: core::mem::take(connected),
        non_commit: core::mem::take(non_commit),
      };
      return false;
    };
    let check = check_branch_target(
      command,
      connected.get(*at).copied(),
      objects,
      by_id,
      repository,
    );
    match check {
      TargetCheck::Ok => {}
      TargetCheck::Away(away) if *stopped != Some((StopStage::BranchTargets, *at, away)) => {
        *stopped = Some((StopStage::BranchTargets, *at, away));
        return true;
      }
      TargetCheck::NonCommit | TargetCheck::Away(_) => {
        if let Some(slot) = non_commit.get_mut(*at) {
          *slot = true;
        }
      }
    }
    *stopped = None;
    *at = at.saturating_add(1);
    false
  }

  /// Advances `Stage::Fsck` by one object, or transitions to `Stage::SpecialBlobs` / `Stage::Walk`.
  fn unit_fsck<R: Repository + ?Sized>(
    &mut self,
    repository: &R,
    contents: &dyn Contents,
    admission: &mut Admission,
  ) -> Result<bool, Refusal> {
    let objects = match &self.unpacked {
      Some(Ok(unpacked)) => unpacked.objects.as_slice(),
      _ => &[],
    };
    let Stage::Fsck { at } = &mut self.stage else {
      return Ok(false);
    };
    let Some(object) = objects.get(*at) else {
      return self.finish_fsck(contents, admission);
    };
    let finding = fsck_one(
      object,
      *at,
      (repository, contents, self.request.format),
      &mut self.special,
    );
    match finding {
      Some(finding) => {
        if let Some(away) = contents.away()
          && self.stopped != Some((StopStage::Fsck, *at, away))
        {
          self.stopped = Some((StopStage::Fsck, *at, away));
          return Ok(true);
        }
        self.stopped = None;
        let (count, id) = (objects.len(), object.id);
        refuse_pack(&mut self.unpacked, &mut self.stage, count, id, finding);
      }
      None => {
        self.stopped = None;
        *at = at.saturating_add(1);
      }
    }
    Ok(false)
  }

  /// Transitions out of `Stage::Fsck` once every pushed object has been checked.
  fn finish_fsck(
    &mut self,
    contents: &dyn Contents,
    admission: &mut Admission,
  ) -> Result<bool, Refusal> {
    let objects = match &self.unpacked {
      Some(Ok(unpacked)) => unpacked.objects.as_slice(),
      _ => &[],
    };
    self.stopped = None;
    let pushed = Pushed::new(objects, Some(contents), admission)?;
    self.stage = if self.special.is_empty() {
      let walk = Walk::new(&self.request, &pushed, admission)?;
      Stage::Walk {
        by_id: pushed.by_id,
        walk,
      }
    } else {
      self.special.sort_unstable();
      self.special.dedup();
      Stage::SpecialBlobs {
        by_id: pushed.by_id,
        at: 0,
      }
    };
    Ok(false)
  }

  /// Advances `Stage::SpecialBlobs` by one dotfile entry, or transitions to `Stage::Walk`.
  fn unit_special<R: Repository + ?Sized>(
    &mut self,
    repository: &R,
    contents: &dyn Contents,
    admission: &mut Admission,
  ) -> Result<bool, Refusal> {
    let objects = match &self.unpacked {
      Some(Ok(unpacked)) => unpacked.objects.as_slice(),
      _ => &[],
    };
    let Stage::SpecialBlobs { by_id, at } = &mut self.stage else {
      return Ok(false);
    };
    let Some(entry) = self.special.get(*at).copied() else {
      self.stopped = None;
      let pushed = Pushed {
        objects,
        by_id: core::mem::take(by_id),
        contents: Some(contents),
      };
      let walk = Walk::new(&self.request, &pushed, admission)?;
      self.stage = Stage::Walk {
        by_id: pushed.by_id,
        walk,
      };
      return Ok(false);
    };
    let finding = check_special_entry(entry, objects, by_id, contents, repository);
    if let Some(away) = contents.away().or_else(|| repository.away())
      && self.stopped != Some((StopStage::SpecialBlobs, *at, away))
    {
      self.stopped = Some((StopStage::SpecialBlobs, *at, away));
      return Ok(true);
    }
    self.stopped = None;
    if let Some(finding) = finding {
      let count = objects.len();
      refuse_pack(
        &mut self.unpacked,
        &mut self.stage,
        count,
        entry.id,
        finding,
      );
    } else {
      *at = at.saturating_add(1);
    }
    Ok(false)
  }

  /// Advances `Stage::Walk` by one turn, or transitions to `Stage::BranchTargets`.
  fn unit_walk<R: Repository + ?Sized>(
    &mut self,
    repository: &R,
    contents: &dyn Contents,
    admission: &mut Admission,
    left: Quantum<'_>,
  ) -> Result<bool, Refusal> {
    let objects = match &self.unpacked {
      Some(Ok(unpacked)) => unpacked.objects.as_slice(),
      _ => &[],
    };
    let Stage::Walk { by_id, walk } = &mut self.stage else {
      return Ok(false);
    };
    let pushed = Pushed {
      objects,
      by_id: core::mem::take(by_id),
      contents: Some(contents),
    };
    let done = walk.turn((&self.request, &pushed, repository), admission, left);
    let (paused, connected) = match done {
      Ok(WalkStep::Done) => (
        false,
        Some(walk.connected(&self.request, &pushed, repository)),
      ),
      Ok(WalkStep::Away) => (true, None),
      Ok(WalkStep::More) => (false, None),
      Err(refusal) => (false, Some(Err(refusal))),
    };
    *by_id = pushed.by_id;
    if let Some(connected) = connected {
      let connected = connected?;
      let count = self.request.commands.len();
      let mut non_commit = charged_vec(count, admission)?;
      non_commit.resize(count, false);
      self.stage = Stage::BranchTargets {
        by_id: core::mem::take(by_id),
        connected,
        non_commit,
        at: 0,
      };
    }
    Ok(paused)
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
      Stage::Connected {
        by_id,
        connected,
        non_commit,
      } => {
        early.extend(
          self
            .request
            .commands
            .iter()
            .zip(&connected)
            .zip(&non_commit)
            .map(|((command, connected), non_commit)| {
              early_refusal(command, *connected, *non_commit, hidden)
            }),
        );
        by_id
      }
      Stage::Refused
      | Stage::Fsck { .. }
      | Stage::SpecialBlobs { .. }
      | Stage::Walk { .. }
      | Stage::BranchTargets { .. } => {
        early.extend((0..count).map(|_| Some(refused(UNPACKER_ERROR, None))));
        IdIndex::default()
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

/// Outcome of validating one command's target kind for a `refs/heads/*` ref.
enum TargetCheck {
  /// Valid (not a non-delete `refs/heads/*` ref, or target is a commit).
  Ok,
  /// Target is not a commit (or unreadable after retry).
  NonCommit,
  /// Lookup met a let-go pack window in `repository`.
  Away(u64),
}

/// Validates that a connected, valid-named, non-delete `refs/heads/*` command points to a commit.
fn check_branch_target<R: Repository + ?Sized>(
  command: &super::Command,
  connected: Option<Connected>,
  objects: &[crate::unpack::Placed],
  by_id: &IdIndex,
  repository: &R,
) -> TargetCheck {
  let needs_commit = connected == Some(Connected::Complete)
    && !command.is_delete()
    && command.name.starts_with(HEADS_PREFIX)
    && super::request::refname_ok(&command.name, false);
  if !needs_commit {
    return TargetCheck::Ok;
  }
  match target_kind(&command.new, objects, by_id, repository) {
    Some(Kind::Commit) => TargetCheck::Ok,
    Some(_) => TargetCheck::NonCommit,
    None => repository
      .away()
      .map_or(TargetCheck::NonCommit, TargetCheck::Away),
  }
}

/// Resolves the [`Kind`] of `id` from the incoming pack first, then `repository`.
fn target_kind<R: Repository + ?Sized>(
  id: &ObjectId,
  objects: &[crate::unpack::Placed],
  by_id: &IdIndex,
  repository: &R,
) -> Option<Kind> {
  if let Some(pos) = by_id.position(objects, id) {
    return by_id.kind(pos).or_else(|| objects.get(pos).map(|p| p.kind));
  }
  repository.kind(id)
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
  by_id: &IdIndex,
  contents: &dyn Contents,
  repository: &R,
) -> Option<crate::objects::FsckId> {
  let lookup = |id: &ObjectId| {
    if let Some(at) = by_id.position(objects, id)
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
