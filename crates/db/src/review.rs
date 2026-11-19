//! The review records of a partition's repositories (§4.17.1 "Works over works", shared piece 6; D-28; A-596).
//!
//! A review request is a work held unsubmitted; its review record is its comments and approvals, held on its
//! repository's partition beside the repository (§4.8): logged as operations, applied unconditionally once the guard has
//! passed them, carried whole by a partition snapshot and rebuilt from it ([`Reviews::restore`]). The rules the guard
//! keeps are the design's:
//! - a comment is anchored to a path, the whole lines a byte range covers, and a version of the request's work. It moves
//!   only forward, to a later version, and only while no change has met its lines; once outdated it is never moved or
//!   outdated again, so it is never moved onto other text (`slates_merge::review::reanchor` decides each move from the
//!   engine's position mapping, and its caller records it);
//! - an approval binds to the version it saw. Its approver's later approval replaces it; one of an earlier version is
//!   refused. A later change marks it stale where the repository's policy says the change meets what it covers
//!   ([`Reviews::staled_by`] takes the policy as a predicate: its matcher is A-597's), and a stale approval is not staled
//!   again;
//! - a request's records leave with it ([`Op::ReviewClosed`]) and with their repository.
//!
//! Bounds: the records' bytes are charged to the partition's merge byte budget (`PartitionCaps::green_chain_bytes`,
//! which the daemon derives from memory), the plane a review belongs to, so a partition holds no more review records than
//! it has room for, refused with the merge plane's typed `Capacity` at it ([`Reviews::charge`]).

use std::collections::BTreeMap;

use crate::catalog::{Principal, RepositoryId, ReviewApprovalRecord, ReviewCommentRecord, VolumeId};
use crate::error::DbError;
use crate::op::Op;

/// A comment's key: its repository, its request, its number.
type CommentKey = (RepositoryId, VolumeId, u64);
/// An approval's key: its repository, its request, its approver's index key ([`Principal::key`]).
type ApprovalKey = (RepositoryId, VolumeId, Vec<u8>);

/// The review records of one partition's repositories.
#[derive(Debug, Default)]
pub struct Reviews {
  /// The comments, by repository, request and number, so one request's are one range.
  comments: BTreeMap<CommentKey, ReviewCommentRecord>,
  /// The approvals, by repository, request and approver, so one request's are one range.
  approvals: BTreeMap<ApprovalKey, ReviewApprovalRecord>,
  /// The bytes the records hold, charged to the partition's merge byte budget.
  bytes: usize,
}

/// The bytes a comment charges: its record and the text it holds.
fn comment_bytes(record: &ReviewCommentRecord) -> usize {
  size_of::<ReviewCommentRecord>()
    .saturating_add(record.path.len())
    .saturating_add(record.body.len())
}

/// The bytes an approval charges: its record and its approver's key.
fn approval_bytes(record: &ReviewApprovalRecord) -> usize {
  size_of::<ReviewApprovalRecord>().saturating_add(record.approver.key().len())
}

/// Whether a comment's lines span bytes: not empty, and their end is a byte position.
fn spans(start: u64, len: u64) -> bool {
  len > 0 && start.checked_add(len).is_some()
}

impl Reviews {
  /// No records.
  pub fn new() -> Reviews {
    Reviews::default()
  }

  /// The bytes the records hold.
  pub fn bytes(&self) -> usize {
    self.bytes
  }

  /// Comment `id` of `request` in `repository`.
  pub fn comment(
    &self,
    repository: RepositoryId,
    request: VolumeId,
    id: u64,
  ) -> Option<&ReviewCommentRecord> {
    self.comments.get(&(repository, request, id))
  }

  /// `request`'s comments in `repository`, by number.
  pub fn comments(
    &self,
    repository: RepositoryId,
    request: VolumeId,
  ) -> impl Iterator<Item = &ReviewCommentRecord> + '_ {
    self
      .comments
      .range((repository, request, 0)..=(repository, request, u64::MAX))
      .map(|(_, record)| record)
  }

  /// `approver`'s approval of `request` in `repository`.
  pub fn approval(
    &self,
    repository: RepositoryId,
    request: VolumeId,
    approver: &Principal,
  ) -> Option<&ReviewApprovalRecord> {
    self.approvals.get(&(repository, request, approver.key()))
  }

  /// `request`'s approvals in `repository`, by approver.
  pub fn approvals(
    &self,
    repository: RepositoryId,
    request: VolumeId,
  ) -> impl Iterator<Item = &ReviewApprovalRecord> + '_ {
    self
      .approvals
      .range((repository, request, Vec::new())..)
      .take_while(move |((owner, of, _), _)| *owner == repository && *of == request)
      .map(|(_, record)| record)
  }

  /// Every comment, in key order, as a snapshot carries them.
  pub fn comment_records(&self) -> impl Iterator<Item = &ReviewCommentRecord> + '_ {
    self.comments.values()
  }

  /// Every approval, in key order, as a snapshot carries them.
  pub fn approval_records(&self) -> impl Iterator<Item = &ReviewApprovalRecord> + '_ {
    self.approvals.values()
  }

  /// The bytes `op` releases and adds: what the partition's merge budget must have room for before it is applied
  /// (`Partition::check`), and what the budget moves by when it is.
  pub fn charge(&self, op: &Op) -> (usize, usize) {
    match op {
      Op::ReviewCommented { record } => (0, comment_bytes(record)),
      Op::ReviewApproved { record } => (
        self
          .approval(record.repository, record.request, &record.approver)
          .map_or(0, approval_bytes),
        approval_bytes(record),
      ),
      Op::ReviewClosed {
        repository,
        request,
      } => {
        let comments: usize = self
          .comments(*repository, *request)
          .map(comment_bytes)
          .sum();
        let approvals: usize = self
          .approvals(*repository, *request)
          .map(approval_bytes)
          .sum();
        (comments.saturating_add(approvals), 0)
      }
      _ => (0, 0),
    }
  }

  /// The guard for a review operation (`Partition::check`): refuses what the log must never record. `repository` says
  /// whether this partition holds a repository.
  pub fn check(&self, op: &Op, repository: impl Fn(RepositoryId) -> bool) -> Result<(), DbError> {
    match op {
      Op::ReviewCommented { record } => self.check_commented(record, repository),
      Op::ReviewCommentMoved {
        repository,
        request,
        id,
        start,
        len,
        version,
      } => {
        let comment = self
          .comment(*repository, *request, *id)
          .ok_or(DbError::NotFound)?;
        if comment.outdated || *version <= comment.version || !spans(*start, *len) {
          return Err(DbError::ReviewOutOfOrder);
        }
        Ok(())
      }
      Op::ReviewCommentOutdated {
        repository,
        request,
        id,
        version,
      } => {
        let comment = self
          .comment(*repository, *request, *id)
          .ok_or(DbError::NotFound)?;
        if comment.outdated || *version <= comment.version {
          return Err(DbError::ReviewOutOfOrder);
        }
        Ok(())
      }
      Op::ReviewApproved { record } => {
        if !repository(record.repository) {
          return Err(DbError::NotFound);
        }
        let earlier = self
          .approval(record.repository, record.request, &record.approver)
          .is_some_and(|held| held.version > record.version);
        if record.stale || earlier {
          return Err(DbError::ReviewOutOfOrder);
        }
        Ok(())
      }
      Op::ReviewApprovalStaled {
        repository,
        request,
        approver,
        version,
      } => {
        let approval = self
          .approval(*repository, *request, approver)
          .ok_or(DbError::NotFound)?;
        if approval.stale || *version <= approval.version {
          return Err(DbError::ReviewOutOfOrder);
        }
        Ok(())
      }
      Op::ReviewClosed {
        repository,
        request,
      } => {
        let held = self.comments(*repository, *request).next().is_some()
          || self.approvals(*repository, *request).next().is_some();
        if !held {
          return Err(DbError::NotFound);
        }
        Ok(())
      }
      _ => Ok(()),
    }
  }

  /// A new comment is on a repository here, under a number its request does not hold, on a named path and whole lines
  /// that span bytes, and not outdated yet.
  fn check_commented(
    &self,
    record: &ReviewCommentRecord,
    repository: impl Fn(RepositoryId) -> bool,
  ) -> Result<(), DbError> {
    if !repository(record.repository) {
      return Err(DbError::NotFound);
    }
    if record.path.is_empty() || record.path.contains('\0') {
      return Err(DbError::InvalidName);
    }
    if record.outdated || !spans(record.start, record.len) {
      return Err(DbError::ReviewOutOfOrder);
    }
    if self
      .comments
      .contains_key(&(record.repository, record.request, record.id))
    {
      return Err(DbError::AlreadyExists {
        existing: record.request.bytes,
      });
    }
    Ok(())
  }

  /// Applies a guarded review operation; unconditional, so replay is deterministic. A record the operation names that is
  /// missing (a corrupted history) is a typed refusal. Other operations are not the reviews' and change nothing.
  pub fn apply(&mut self, op: &Op) -> Result<(), DbError> {
    match op {
      Op::ReviewCommented { record } => {
        self.bytes = self.bytes.saturating_add(comment_bytes(record));
        self
          .comments
          .insert((record.repository, record.request, record.id), record.clone());
      }
      Op::ReviewCommentMoved {
        repository,
        request,
        id,
        start,
        len,
        version,
      } => {
        let comment = self
          .comments
          .get_mut(&(*repository, *request, *id))
          .ok_or(DbError::NotFound)?;
        comment.start = *start;
        comment.len = *len;
        comment.version = *version;
      }
      Op::ReviewCommentOutdated {
        repository,
        request,
        id,
        ..
      } => {
        let comment = self
          .comments
          .get_mut(&(*repository, *request, *id))
          .ok_or(DbError::NotFound)?;
        comment.outdated = true;
      }
      Op::ReviewApproved { record } => {
        let key = (record.repository, record.request, record.approver.key());
        if let Some(replaced) = self.approvals.insert(key, record.clone()) {
          self.bytes = self.bytes.saturating_sub(approval_bytes(&replaced));
        }
        self.bytes = self.bytes.saturating_add(approval_bytes(record));
      }
      Op::ReviewApprovalStaled {
        repository,
        request,
        approver,
        ..
      } => {
        let approval = self
          .approvals
          .get_mut(&(*repository, *request, approver.key()))
          .ok_or(DbError::NotFound)?;
        approval.stale = true;
      }
      Op::ReviewClosed {
        repository,
        request,
      } => {
        let released = self.remove_where(|owner, of| owner == *repository && of == *request);
        self.bytes = self.bytes.saturating_sub(released);
      }
      _ => {}
    }
    Ok(())
  }

  /// Removes every record whose repository and request `matches` says, returning the bytes they held.
  fn remove_where(&mut self, matches: impl Fn(RepositoryId, VolumeId) -> bool) -> usize {
    let mut released = 0usize;
    self.comments.retain(|(repository, request, _), record| {
      let gone = matches(*repository, *request);
      if gone {
        released = released.saturating_add(comment_bytes(record));
      }
      !gone
    });
    self.approvals.retain(|(repository, request, _), record| {
      let gone = matches(*repository, *request);
      if gone {
        released = released.saturating_add(approval_bytes(record));
      }
      !gone
    });
    released
  }

  /// A destroyed repository's review records leave with it: the bytes they held, which the partition's merge budget
  /// releases.
  pub fn repository_removed(&mut self, repository: RepositoryId) -> usize {
    let released = self.remove_where(|owner, _| owner == repository);
    self.bytes = self.bytes.saturating_sub(released);
    released
  }

  /// The operations that mark stale the approvals of `request` a change at `version` makes stale: each one not stale
  /// yet, bound to an earlier version, that `stales` says the change meets. `stales` is the repository's policy over
  /// what the change touched (its matcher is A-597's), asked of each approval.
  pub fn staled_by(
    &self,
    repository: RepositoryId,
    request: VolumeId,
    version: u64,
    stales: impl Fn(&ReviewApprovalRecord) -> bool,
  ) -> Vec<Op> {
    self
      .approvals(repository, request)
      .filter(|approval| !approval.stale && approval.version < version && stales(approval))
      .map(|approval| Op::ReviewApprovalStaled {
        repository,
        request,
        approver: approval.approver.clone(),
        version,
      })
      .collect()
  }

  /// The review records a snapshot carries, rebuilt with their byte count. A second record under one key is a corrupt
  /// snapshot, refused rather than half restored.
  pub fn restore(
    comments: &[ReviewCommentRecord],
    approvals: &[ReviewApprovalRecord],
  ) -> Result<Reviews, DbError> {
    let mut reviews = Reviews::new();
    for record in comments {
      let key = (record.repository, record.request, record.id);
      if reviews.comments.insert(key, record.clone()).is_some() {
        return Err(DbError::Corrupt {
          seq: 0,
          reason: "a snapshot carries two review comments under one number",
        });
      }
      reviews.bytes = reviews.bytes.saturating_add(comment_bytes(record));
    }
    for record in approvals {
      let key = (record.repository, record.request, record.approver.key());
      if reviews.approvals.insert(key, record.clone()).is_some() {
        return Err(DbError::Corrupt {
          seq: 0,
          reason: "a snapshot carries two review approvals of one approver",
        });
      }
      reviews.bytes = reviews.bytes.saturating_add(approval_bytes(record));
    }
    Ok(reviews)
  }
}
