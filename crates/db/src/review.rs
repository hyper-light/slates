//! The review records of a partition's repositories (§4.17.1 "Works over works", shared piece 6; D-28; A-596).
//!
//! A review request is a work held unsubmitted (§4.17.1), updated as its author pushes; its versions are the versions of
//! the volume the records name (`request`). Its review record is its comments and approvals, held on its repository's
//! partition beside the repository (§4.8): logged as operations, applied unconditionally once the guard has passed
//! them, carried whole by a partition snapshot and rebuilt from it ([`Reviews::restore`]). The rules the guard keeps
//! are the design's, each refused by its own kind ([`ReviewRefusal`]):
//! - a comment is anchored to a path, the whole lines a byte range covers, and a version of the request. It moves only
//!   forward, to a later version, while no change has met its lines; once outdated it is never moved or outdated again,
//!   so it is never moved onto other text. The engine decides each move by position mapping
//!   (`slates_merge::engine::Green::reanchor`), and the server records what it decided;
//! - an approval binds to the version its approver saw. The approver's later approval replaces it; one at an earlier
//!   version, or at one before the change that made the standing one stale, is refused, so an approval never binds
//!   back. A later change makes it stale where the repository's policy says that change meets what an approval covers:
//!   [`Reviews::staled_by`] takes the policy as a predicate (its matcher is A-597's), and a stale approval is not made
//!   stale again. The standing approvals ([`Reviews::standing`]) are the evidence a policy counts (§4.17.1 "approvals;
//!   and the evidence the policy requires");
//! - a request's records leave with its review ([`Op::ReviewClosed`], which the server records when the request is
//!   merged, closed or its volume destroyed) and with their repository.
//!
//! Bounds: the records' bytes are charged to the partition's merge byte budget (`PartitionCaps::green_chain_bytes`,
//! which the daemon derives from memory), the plane a review belongs to, so a partition holds no more review records
//! than it has room for, refused with the merge plane's typed `Capacity` at it ([`Reviews::charge`]). A comment's
//! number is its request's last plus one, refused at the largest ([`Reviews::next_comment`]).

use std::collections::BTreeMap;

use crate::catalog::{
  Principal, RepositoryId, ReviewApprovalRecord, ReviewCommentRecord, VolumeId,
};
use crate::error::{DbError, ReviewRefusal};
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

/// The bytes a comment charges: its record, its path and its text.
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

/// A typed review refusal.
fn refused(rule: ReviewRefusal) -> Result<(), DbError> {
  Err(DbError::Review(rule))
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

  /// The number `request`'s next comment takes: its last one's plus one, one for its first; `None` past the largest.
  pub fn next_comment(&self, repository: RepositoryId, request: VolumeId) -> Option<u64> {
    self
      .comments(repository, request)
      .last()
      .map_or(Some(1), |last| last.id.checked_add(1))
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

  /// `request`'s standing approvals in `repository`, by approver: those no change has made stale, the evidence a
  /// policy counts.
  pub fn standing(
    &self,
    repository: RepositoryId,
    request: VolumeId,
  ) -> impl Iterator<Item = &ReviewApprovalRecord> + '_ {
    self
      .approvals(repository, request)
      .filter(|approval| approval.stale_at.is_none())
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
  /// (`Partition::check`), and what the budget moves by when it is. A record's bytes never change once it is made: a
  /// move, an outdating and a staling keep its size.
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

  /// The guard for a review operation (`Partition::check`): refuses what the log must never record. `holds` says
  /// whether this partition holds a repository.
  pub fn check(&self, op: &Op, holds: impl Fn(RepositoryId) -> bool) -> Result<(), DbError> {
    match op {
      Op::ReviewCommented { record } => self.check_commented(record, holds),
      Op::ReviewCommentMoved {
        repository,
        request,
        id,
        start,
        len,
        version,
      } => {
        self.check_comment_moves(*repository, *request, *id, *version)?;
        if !spans(*start, *len) {
          return refused(ReviewRefusal::Empty);
        }
        Ok(())
      }
      Op::ReviewCommentOutdated {
        repository,
        request,
        id,
        version,
      } => self.check_comment_moves(*repository, *request, *id, *version),
      Op::ReviewApproved { record } => self.check_approved(record, holds),
      Op::ReviewApprovalStaled {
        repository,
        request,
        approver,
        version,
      } => {
        let approval = self
          .approval(*repository, *request, approver)
          .ok_or(DbError::NotFound)?;
        if approval.stale_at.is_some() {
          return refused(ReviewRefusal::Stale);
        }
        if *version <= approval.version {
          return refused(ReviewRefusal::NotAfter);
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

  /// A comment moved or outdated at `version` exists, is not outdated, and stands on an earlier version.
  fn check_comment_moves(
    &self,
    repository: RepositoryId,
    request: VolumeId,
    id: u64,
    version: u64,
  ) -> Result<(), DbError> {
    let comment = self
      .comment(repository, request, id)
      .ok_or(DbError::NotFound)?;
    if comment.outdated_at.is_some() {
      return refused(ReviewRefusal::Outdated);
    }
    if version <= comment.version {
      return refused(ReviewRefusal::NotAfter);
    }
    Ok(())
  }

  /// A new comment is on a repository here, on a named path, not outdated, on lines that span bytes, and under a number
  /// its request does not hold.
  fn check_commented(
    &self,
    record: &ReviewCommentRecord,
    holds: impl Fn(RepositoryId) -> bool,
  ) -> Result<(), DbError> {
    if !holds(record.repository) {
      return Err(DbError::NotFound);
    }
    if record.path.is_empty() || record.path.contains('\0') {
      return Err(DbError::InvalidName);
    }
    if record.outdated_at.is_some() {
      return refused(ReviewRefusal::Ended);
    }
    if !spans(record.start, record.len) {
      return refused(ReviewRefusal::Empty);
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

  /// A new approval is on a repository here, not stale, and binds to a version no earlier than its approver's standing
  /// one, and no earlier than the change that made the approver's approval stale.
  fn check_approved(
    &self,
    record: &ReviewApprovalRecord,
    holds: impl Fn(RepositoryId) -> bool,
  ) -> Result<(), DbError> {
    if !holds(record.repository) {
      return Err(DbError::NotFound);
    }
    if record.stale_at.is_some() {
      return refused(ReviewRefusal::Ended);
    }
    let behind = self
      .approval(record.repository, record.request, &record.approver)
      .is_some_and(|held| {
        record.version < held.version || held.stale_at.is_some_and(|at| record.version < at)
      });
    if behind {
      return refused(ReviewRefusal::Behind);
    }
    Ok(())
  }

  /// Applies a guarded review operation; unconditional, so replay is deterministic. A record the operation names that is
  /// missing (a corrupted history) is a typed refusal. Other operations are not the reviews' and change nothing.
  pub fn apply(&mut self, op: &Op) -> Result<(), DbError> {
    match op {
      Op::ReviewCommented { record } => {
        self.bytes = self.bytes.saturating_add(comment_bytes(record));
        self.comments.insert(
          (record.repository, record.request, record.id),
          record.clone(),
        );
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
        version,
      } => {
        let comment = self
          .comments
          .get_mut(&(*repository, *request, *id))
          .ok_or(DbError::NotFound)?;
        comment.outdated_at = Some(*version);
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
        version,
      } => {
        let approval = self
          .approvals
          .get_mut(&(*repository, *request, approver.key()))
          .ok_or(DbError::NotFound)?;
        approval.stale_at = Some(*version);
      }
      Op::ReviewClosed {
        repository,
        request,
      } => {
        self.remove_where(|owner, of| owner == *repository && of == *request);
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
    self.bytes = self.bytes.saturating_sub(released);
    released
  }

  /// A destroyed repository's review records leave with it: the bytes they held, which the partition's merge budget
  /// releases.
  pub fn repository_removed(&mut self, repository: RepositoryId) -> usize {
    self.remove_where(|owner, _| owner == repository)
  }

  /// The operations that make stale the approvals of `request` the change committed at `version` makes stale: each
  /// standing one bound to an earlier version that `stales` says the change meets. `stales` is the repository's policy
  /// over what the change touched since the approval's version (its matcher is A-597's), asked of each approval.
  pub fn staled_by(
    &self,
    repository: RepositoryId,
    request: VolumeId,
    version: u64,
    stales: impl Fn(&ReviewApprovalRecord) -> bool,
  ) -> Vec<Op> {
    self
      .standing(repository, request)
      .filter(|approval| approval.version < version && stales(approval))
      .map(|approval| Op::ReviewApprovalStaled {
        repository,
        request,
        approver: approval.approver.clone(),
        version,
      })
      .collect()
  }

  /// The review records a snapshot carries, rebuilt with their byte count. A record of a repository `holds` says the
  /// partition does not hold, or a second record under one key, is a corrupt snapshot, refused rather than half
  /// restored.
  pub fn restore(
    comments: &[ReviewCommentRecord],
    approvals: &[ReviewApprovalRecord],
    holds: impl Fn(RepositoryId) -> bool,
  ) -> Result<Reviews, DbError> {
    let corrupt = |reason| DbError::Corrupt { seq: 0, reason };
    let mut reviews = Reviews::new();
    for record in comments {
      let key = (record.repository, record.request, record.id);
      if !holds(record.repository) || reviews.comments.insert(key, record.clone()).is_some() {
        return Err(corrupt(
          "a snapshot's review comment names no repository, or two share a number",
        ));
      }
      reviews.bytes = reviews.bytes.saturating_add(comment_bytes(record));
    }
    for record in approvals {
      let key = (record.repository, record.request, record.approver.key());
      if !holds(record.repository) || reviews.approvals.insert(key, record.clone()).is_some() {
        return Err(corrupt(
          "a snapshot's review approval names no repository, or one approver approves twice",
        ));
      }
      reviews.bytes = reviews.bytes.saturating_add(approval_bytes(record));
    }
    Ok(reviews)
  }
}

#[cfg(test)]
// proptest's strategy types carry `Arc` (D-8's harness exception).
#[allow(clippy::disallowed_types)]
mod tests {
  use std::collections::{BTreeMap, BTreeSet};

  use proptest::prelude::*;

  use super::*;
  use crate::catalog::{ObjectFormat, RepositoryRecord, VcsTool};
  use crate::partition::{Partition, PartitionCaps};

  /// Shape: the partition the tests' repositories live on.
  const HOME: u16 = 3;
  /// Shape: the records the merge byte budget has room for, about: small, so histories meet the bound.
  const BUDGET_RECORDS: usize = 6;

  /// An id on `HOME` with `serial` in its low byte, as the server mints them.
  fn id_bytes(serial: u8) -> [u8; 16] {
    let mut bytes = [0u8; 16];
    bytes[8..10].copy_from_slice(&HOME.to_be_bytes());
    bytes[15] = serial;
    bytes
  }

  fn repository_id(serial: u8) -> RepositoryId {
    RepositoryId {
      bytes: id_bytes(serial),
    }
  }

  /// A request's volume id: requests' serials start past the repositories'.
  fn request_id(serial: u8) -> VolumeId {
    VolumeId {
      bytes: id_bytes(100 + serial),
    }
  }

  fn person(serial: u8) -> Principal {
    Principal::Certificate { hash: [serial; 32] }
  }

  fn caps() -> PartitionCaps {
    PartitionCaps {
      volumes: 1 << 4,
      snapshots: 1 << 4,
      attachments: 1 << 4,
      segment_slots: 4,
      timers: 1 << 4,
      tick_ns: 1_000,
      green_chain_bytes: BUDGET_RECORDS * size_of::<ReviewCommentRecord>(),
      audit_records: 1 << 4,
    }
  }

  fn created(serial: u8) -> Op {
    Op::RepositoryCreated {
      record: RepositoryRecord {
        id: repository_id(serial),
        name: format!("repository-{serial}"),
        tool: VcsTool::Git,
        format: ObjectFormat::Sha1,
        default_branch: None,
        owner: person(0),
      },
    }
  }

  fn comment(
    repo: u8,
    request: u8,
    id: u64,
    version: u64,
    start: u64,
    len: u64,
  ) -> ReviewCommentRecord {
    ReviewCommentRecord {
      repository: repository_id(repo),
      request: request_id(request),
      id,
      author: person(1),
      path: "src/main.rs".to_owned(),
      start,
      len,
      version,
      outdated_at: None,
      body: "why this line?".to_owned(),
    }
  }

  fn approval(repo: u8, request: u8, approver: u8, version: u64) -> ReviewApprovalRecord {
    ReviewApprovalRecord {
      repository: repository_id(repo),
      request: request_id(request),
      approver: person(approver),
      version,
      stale_at: None,
    }
  }

  /// Runs `op` as the live path does: the guard, then the apply.
  fn run(partition: &mut Partition, op: &Op) -> Result<(), DbError> {
    partition.check(op, 0)?;
    partition.apply(op)
  }

  /// A partition holding repository 0.
  fn partition() -> Partition {
    let mut partition = Partition::new(caps(), 0);
    run(&mut partition, &created(0)).unwrap();
    partition
  }

  /// What a partition answers about its review records, compared whole across a snapshot.
  fn records(partition: &Partition) -> (Vec<ReviewCommentRecord>, Vec<ReviewApprovalRecord>) {
    let reviews = partition.reviews();
    (
      reviews.comment_records().cloned().collect(),
      reviews.approval_records().cloned().collect(),
    )
  }

  /// A-596 (§4.17.1 shared piece 6). Do: approve a request by three people at versions 2, 3 and 5; ask which a change
  /// at version 4 makes stale under a policy naming the first and third; apply that; then approve again behind and
  /// ahead, stale twice, and ask for every approval at version 6 and 7. Expect: only the first is made stale (the third
  /// saw version 5, the second the policy does not name), the other two stand; the first approving again at 3 is
  /// refused `Behind` (before the change that made it stale) and at 4 replaces it and stands; the second approving at
  /// 1 is refused `Behind`; staling the replaced approval at its own version is refused `NotAfter`; a change at 6 makes
  /// all three stale and one at 7 none (a stale approval is not made stale again, refused `Stale` when asked).
  #[test]
  fn an_approval_binds_to_the_version_it_saw_and_a_change_the_policy_names_makes_it_stale() {
    let mut partition = partition();
    let (repo, request) = (repository_id(0), request_id(0));
    for (approver, version) in [(1, 2), (2, 3), (3, 5)] {
      run(
        &mut partition,
        &Op::ReviewApproved {
          record: approval(0, 0, approver, version),
        },
      )
      .unwrap();
    }
    let named = [person(1), person(3)];
    let stale = partition
      .reviews()
      .staled_by(repo, request, 4, |held| named.contains(&held.approver));
    assert_eq!(
      stale,
      vec![Op::ReviewApprovalStaled {
        repository: repo,
        request,
        approver: person(1),
        version: 4
      }]
    );
    for op in &stale {
      run(&mut partition, op).unwrap();
    }
    let standing: Vec<Principal> = partition
      .reviews()
      .standing(repo, request)
      .map(|held| held.approver.clone())
      .collect();
    assert_eq!(standing, vec![person(2), person(3)]);
    let again = |approver, version| Op::ReviewApproved {
      record: approval(0, 0, approver, version),
    };
    assert_eq!(
      run(&mut partition, &again(1, 3)),
      Err(DbError::Review(ReviewRefusal::Behind))
    );
    run(&mut partition, &again(1, 4)).unwrap();
    assert_eq!(partition.reviews().standing(repo, request).count(), 3);
    assert_eq!(
      run(&mut partition, &again(2, 1)),
      Err(DbError::Review(ReviewRefusal::Behind))
    );
    let staled = |approver, version| Op::ReviewApprovalStaled {
      repository: repo,
      request,
      approver: person(approver),
      version,
    };
    assert_eq!(
      run(&mut partition, &staled(1, 4)),
      Err(DbError::Review(ReviewRefusal::NotAfter))
    );
    let every = partition.reviews().staled_by(repo, request, 6, |_| true);
    assert_eq!(every.len(), 3);
    for op in &every {
      run(&mut partition, op).unwrap();
    }
    assert_eq!(partition.reviews().standing(repo, request).count(), 0);
    assert!(
      partition
        .reviews()
        .staled_by(repo, request, 7, |_| true)
        .is_empty()
    );
    assert_eq!(
      run(&mut partition, &staled(2, 7)),
      Err(DbError::Review(ReviewRefusal::Stale))
    );
    let rebuilt = Partition::from_snapshot(&partition.to_snapshot(0), caps(), 0).unwrap();
    assert_eq!(records(&rebuilt), records(&partition));
  }

  /// A-596 (§4.17.1 "Works over works"). Do: write comment 1 at version 2, then move it to version 1, to version 3, to
  /// version 4 with no bytes, outdate it at 3 and at 4, then move and outdate it again; write new comments already
  /// outdated, on an empty path and on a path with a NUL, with no bytes, under a number taken, and on a repository the
  /// partition does not hold. Expect: the move back refused `NotAfter`, the move forward taken, the empty one `Empty`,
  /// outdating at its own version `NotAfter` and at 4 taken (it stays where it stood at 3), every later change refused
  /// `Outdated`; the new comments refused `Ended`, `InvalidName` twice, `Empty`, `AlreadyExists` and `NotFound`; and
  /// the next number one past the last.
  #[test]
  fn a_comment_moves_forward_until_a_change_outdates_it_and_never_after() {
    let mut partition = partition();
    let (repo, request) = (repository_id(0), request_id(0));
    assert_eq!(partition.reviews().next_comment(repo, request), Some(1));
    run(
      &mut partition,
      &Op::ReviewCommented {
        record: comment(0, 0, 1, 2, 5, 4),
      },
    )
    .unwrap();
    let moved = |version, start, len| Op::ReviewCommentMoved {
      repository: repo,
      request,
      id: 1,
      start,
      len,
      version,
    };
    let outdated = |version| Op::ReviewCommentOutdated {
      repository: repo,
      request,
      id: 1,
      version,
    };
    let review = |rule| Err(DbError::Review(rule));
    assert_eq!(
      run(&mut partition, &moved(1, 9, 4)),
      review(ReviewRefusal::NotAfter)
    );
    run(&mut partition, &moved(3, 9, 4)).unwrap();
    assert_eq!(
      run(&mut partition, &moved(4, 9, 0)),
      review(ReviewRefusal::Empty)
    );
    assert_eq!(
      run(&mut partition, &outdated(3)),
      review(ReviewRefusal::NotAfter)
    );
    run(&mut partition, &outdated(4)).unwrap();
    let held = partition.reviews().comment(repo, request, 1).unwrap();
    assert_eq!(
      (held.start, held.len, held.version, held.outdated_at),
      (9, 4, 3, Some(4))
    );
    assert_eq!(
      run(&mut partition, &moved(5, 13, 4)),
      review(ReviewRefusal::Outdated)
    );
    assert_eq!(
      run(&mut partition, &outdated(5)),
      review(ReviewRefusal::Outdated)
    );
    let mut ended = comment(0, 0, 2, 2, 5, 4);
    ended.outdated_at = Some(3);
    let mut unnamed = comment(0, 0, 2, 2, 5, 4);
    unnamed.path = String::new();
    let mut nul = comment(0, 0, 2, 2, 5, 4);
    nul.path = "src/\0".to_owned();
    for (record, answer) in [
      (ended, review(ReviewRefusal::Ended)),
      (unnamed, Err(DbError::InvalidName)),
      (nul, Err(DbError::InvalidName)),
      (comment(0, 0, 2, 2, 5, 0), review(ReviewRefusal::Empty)),
      (
        comment(0, 0, 2, 2, u64::MAX, 2),
        review(ReviewRefusal::Empty),
      ),
      (
        comment(0, 0, 1, 2, 5, 4),
        Err(DbError::AlreadyExists {
          existing: request.bytes,
        }),
      ),
      (comment(1, 0, 2, 2, 5, 4), Err(DbError::NotFound)),
    ] {
      assert_eq!(
        run(
          &mut partition,
          &Op::ReviewCommented {
            record: record.clone()
          }
        ),
        answer,
        "{record:?}"
      );
    }
    assert_eq!(partition.reviews().next_comment(repo, request), Some(2));
    let rebuilt = Partition::from_snapshot(&partition.to_snapshot(0), caps(), 0).unwrap();
    assert_eq!(records(&rebuilt), records(&partition));
  }

  /// A-596 (bounded growth). Do: write comments on one request until the merge byte budget refuses one, close the
  /// request, write again, then fill the budget from another request and destroy the repository; rebuild from a snapshot
  /// at each point. Expect: the comment past the budget refused `Capacity` (the merge plane's table) with nothing
  /// recorded; after the close the request holds nothing and a comment fits again, as many as before; after the
  /// destroy nothing of the repository is held and a new repository's comments fit; a rebuilt partition holds the same
  /// records and refuses at the same comment.
  #[test]
  fn review_records_fit_the_merge_budget_and_leave_with_their_close_and_their_repository() {
    let mut partition = partition();
    let fill = |partition: &mut Partition, request: u8| -> u64 {
      let mut written = 0;
      loop {
        let id = partition
          .reviews()
          .next_comment(repository_id(0), request_id(request))
          .unwrap();
        let record = comment(0, request, id, 1, 0, 4);
        match run(partition, &Op::ReviewCommented { record }) {
          Ok(()) => written += 1,
          Err(refusal) => {
            assert_eq!(
              refusal,
              DbError::Capacity {
                table: "green_chains"
              }
            );
            return written;
          }
        }
      }
    };
    let first = fill(&mut partition, 0);
    assert!(first > 0 && first < BUDGET_RECORDS as u64, "{first}");
    let rebuilt = Partition::from_snapshot(&partition.to_snapshot(0), caps(), 0).unwrap();
    assert_eq!(records(&rebuilt), records(&partition));
    let mut rebuilt = rebuilt;
    assert_eq!(
      fill(&mut rebuilt, 0),
      0,
      "a rebuilt partition charges what it holds"
    );
    let closed = Op::ReviewClosed {
      repository: repository_id(0),
      request: request_id(0),
    };
    run(&mut partition, &closed).unwrap();
    assert_eq!(
      partition
        .reviews()
        .comments(repository_id(0), request_id(0))
        .count(),
      0
    );
    assert_eq!(run(&mut partition, &closed), Err(DbError::NotFound));
    assert_eq!(fill(&mut partition, 0), first);
    run(
      &mut partition,
      &Op::RepositoryDestroyed {
        repository: repository_id(0),
      },
    )
    .unwrap();
    assert_eq!(partition.reviews().comment_records().count(), 0);
    run(&mut partition, &created(0)).unwrap();
    assert_eq!(fill(&mut partition, 1), first);
    let rebuilt = Partition::from_snapshot(&partition.to_snapshot(0), caps(), 0).unwrap();
    assert_eq!(records(&rebuilt), records(&partition));
  }

  /// One generated step of a history over two repositories' requests.
  #[derive(Clone, Debug)]
  enum Step {
    Create(u8),
    Destroy(u8),
    Comment {
      at: (u8, u8, u8),
      version: u8,
      path: u8,
      span: (u8, u8),
      ended: bool,
      body: u8,
    },
    Move {
      at: (u8, u8, u8),
      version: u8,
      span: (u8, u8),
    },
    Outdate {
      at: (u8, u8, u8),
      version: u8,
    },
    Approve {
      at: (u8, u8, u8),
      version: u8,
      ended: bool,
    },
    Stale {
      at: (u8, u8, u8),
      version: u8,
    },
    StaleBy {
      at: (u8, u8),
      version: u8,
      named: u8,
    },
    Close(u8, u8),
  }

  /// A repository, a request and a third index (a comment's number, or an approver).
  fn at() -> impl Strategy<Value = (u8, u8, u8)> {
    (0..2u8, 0..2u8, 1..4u8)
  }

  fn step() -> impl Strategy<Value = Step> {
    prop_oneof![
      1 => (0..3u8).prop_map(Step::Create),
      1 => (0..3u8).prop_map(Step::Destroy),
      4 => (at(), 0..6u8, 0..4u8, (0..4u8, 0..3u8), prop::bool::weighted(0.1), 0..48u8)
        .prop_map(|(at, version, path, span, ended, body)| Step::Comment { at, version, path, span, ended, body }),
      3 => (at(), 0..8u8, (0..4u8, 0..3u8)).prop_map(|(at, version, span)| Step::Move { at, version, span }),
      2 => (at(), 0..8u8).prop_map(|(at, version)| Step::Outdate { at, version }),
      3 => (at(), 0..8u8, prop::bool::weighted(0.1)).prop_map(|(at, version, ended)| Step::Approve { at, version, ended }),
      1 => (at(), 0..8u8).prop_map(|(at, version)| Step::Stale { at, version }),
      2 => ((0..2u8, 0..2u8), 0..8u8, any::<u8>()).prop_map(|(at, version, named)| Step::StaleBy { at, version, named }),
      1 => (0..2u8, 0..2u8).prop_map(|(repo, request)| Step::Close(repo, request)),
    ]
  }

  /// A path by index: two names, an empty one and one holding a NUL byte.
  fn path_of(index: u8) -> String {
    match index {
      0 => "src/main.rs".to_owned(),
      1 => "README".to_owned(),
      2 => String::new(),
      _ => "src/\0".to_owned(),
    }
  }

  /// A span by its generated pair: the last start value is the largest byte position, whose end overflows.
  fn span_of((start, len): (u8, u8)) -> (u64, u64) {
    let start = if start == 3 {
      u64::MAX
    } else {
      u64::from(start)
    };
    (start, u64::from(len))
  }

  /// What the partition answered: accepted, or the refusal.
  type Answer = Result<(), DbError>;

  /// The design's rules over plain maps: the repositories held, every comment and approval by repository, request and
  /// number or approver, and the merge byte budget's use.
  #[derive(Default)]
  struct Model {
    repositories: BTreeSet<u8>,
    comments: BTreeMap<(u8, u8, u64), ReviewCommentRecord>,
    approvals: BTreeMap<(u8, u8, u8), ReviewApprovalRecord>,
    bytes: usize,
  }

  impl Model {
    /// Whether the budget has room for `added` once `released` leaves; the merge plane's refusal otherwise.
    fn fits(&self, released: usize, added: usize) -> Answer {
      if self.bytes - released + added > caps().green_chain_bytes {
        return Err(DbError::Capacity {
          table: "green_chains",
        });
      }
      Ok(())
    }

    /// Removes every record of `repository` (and of `request` in it, when given), giving their bytes back.
    fn remove(&mut self, repository: u8, request: Option<u8>) {
      let matches =
        |repo: u8, req: u8| repo == repository && request.is_none_or(|only| only == req);
      let mut released = 0;
      self.comments.retain(|(repo, req, _), record| {
        let gone = matches(*repo, *req);
        if gone {
          released += comment_bytes(record);
        }
        !gone
      });
      self.approvals.retain(|(repo, req, _), record| {
        let gone = matches(*repo, *req);
        if gone {
          released += approval_bytes(record);
        }
        !gone
      });
      self.bytes -= released;
    }

    fn step(&mut self, step: &Step) -> Answer {
      let review = |rule| Err(DbError::Review(rule));
      match *step {
        Step::Create(repo) => {
          if !self.repositories.insert(repo) {
            return Err(DbError::AlreadyExists {
              existing: repository_id(repo).bytes,
            });
          }
          Ok(())
        }
        Step::Destroy(repo) => {
          if !self.repositories.remove(&repo) {
            return Err(DbError::NotFound);
          }
          self.remove(repo, None);
          Ok(())
        }
        Step::Comment {
          at: (repo, request, id),
          path,
          span,
          ended,
          ..
        } => {
          let record = comment_of(step);
          if !self.repositories.contains(&repo) {
            return Err(DbError::NotFound);
          }
          if path >= 2 {
            return Err(DbError::InvalidName);
          }
          if ended {
            return review(ReviewRefusal::Ended);
          }
          let (start, len) = span_of(span);
          if len == 0 || start.checked_add(len).is_none() {
            return review(ReviewRefusal::Empty);
          }
          let key = (repo, request, u64::from(id));
          if self.comments.contains_key(&key) {
            return Err(DbError::AlreadyExists {
              existing: request_id(request).bytes,
            });
          }
          self.fits(0, comment_bytes(&record))?;
          self.bytes += comment_bytes(&record);
          self.comments.insert(key, record);
          Ok(())
        }
        Step::Move {
          at: (repo, request, id),
          version,
          span,
        } => {
          let (start, len) = span_of(span);
          let held = self
            .comments
            .get_mut(&(repo, request, u64::from(id)))
            .ok_or(DbError::NotFound)?;
          if held.outdated_at.is_some() {
            return review(ReviewRefusal::Outdated);
          }
          if u64::from(version) <= held.version {
            return review(ReviewRefusal::NotAfter);
          }
          if len == 0 || start.checked_add(len).is_none() {
            return review(ReviewRefusal::Empty);
          }
          (held.start, held.len, held.version) = (start, len, u64::from(version));
          Ok(())
        }
        Step::Outdate {
          at: (repo, request, id),
          version,
        } => {
          let held = self
            .comments
            .get_mut(&(repo, request, u64::from(id)))
            .ok_or(DbError::NotFound)?;
          if held.outdated_at.is_some() {
            return review(ReviewRefusal::Outdated);
          }
          if u64::from(version) <= held.version {
            return review(ReviewRefusal::NotAfter);
          }
          held.outdated_at = Some(u64::from(version));
          Ok(())
        }
        Step::Approve {
          at: (repo, request, approver),
          version,
          ended,
        } => {
          if !self.repositories.contains(&repo) {
            return Err(DbError::NotFound);
          }
          if ended {
            return review(ReviewRefusal::Ended);
          }
          let version = u64::from(version);
          let key = (repo, request, approver);
          let held = self.approvals.get(&key);
          if held.is_some_and(|held| {
            version < held.version || held.stale_at.is_some_and(|at| version < at)
          }) {
            return review(ReviewRefusal::Behind);
          }
          let record = approval(repo, request, approver, version);
          let released = held.map_or(0, approval_bytes);
          self.fits(released, approval_bytes(&record))?;
          self.bytes = self.bytes - released + approval_bytes(&record);
          self.approvals.insert(key, record);
          Ok(())
        }
        Step::Stale {
          at: (repo, request, approver),
          version,
        } => {
          let held = self
            .approvals
            .get_mut(&(repo, request, approver))
            .ok_or(DbError::NotFound)?;
          if held.stale_at.is_some() {
            return review(ReviewRefusal::Stale);
          }
          if u64::from(version) <= held.version {
            return review(ReviewRefusal::NotAfter);
          }
          held.stale_at = Some(u64::from(version));
          Ok(())
        }
        Step::StaleBy { .. } => Ok(()),
        Step::Close(repo, request) => {
          let any = self
            .comments
            .keys()
            .any(|(r, q, _)| (*r, *q) == (repo, request))
            || self
              .approvals
              .keys()
              .any(|(r, q, _)| (*r, *q) == (repo, request));
          if !any {
            return Err(DbError::NotFound);
          }
          self.remove(repo, Some(request));
          Ok(())
        }
      }
    }

    /// Makes stale the approvals of `request` in `repo` a change at `version` makes stale under a policy naming the
    /// approvers whose bit `named` sets (the standing ones bound to an earlier version), returning the operations that
    /// record it, by approver.
    fn stale_by(&mut self, (repo, request): (u8, u8), version: u64, named: u8) -> Vec<Op> {
      let mut ops = Vec::new();
      for ((r, q, approver), held) in &mut self.approvals {
        let covered = (*r, *q) == (repo, request)
          && held.stale_at.is_none()
          && held.version < version
          && named & (1 << *approver) != 0;
        if covered {
          held.stale_at = Some(version);
          ops.push(Op::ReviewApprovalStaled {
            repository: repository_id(repo),
            request: request_id(request),
            approver: person(*approver),
            version,
          });
        }
      }
      ops
    }

    fn records(&self) -> (Vec<ReviewCommentRecord>, Vec<ReviewApprovalRecord>) {
      (
        self.comments.values().cloned().collect(),
        self.approvals.values().cloned().collect(),
      )
    }
  }

  /// The comment a generated step writes.
  fn comment_of(step: &Step) -> ReviewCommentRecord {
    let Step::Comment {
      at: (repo, request, id),
      version,
      path,
      span,
      ended,
      body,
    } = *step
    else {
      panic!("only a comment step writes a comment");
    };
    let (start, len) = span_of(span);
    ReviewCommentRecord {
      repository: repository_id(repo),
      request: request_id(request),
      id: u64::from(id),
      author: person(9),
      path: path_of(path),
      start,
      len,
      version: u64::from(version),
      outdated_at: ended.then_some(u64::from(version) + 1),
      body: "b".repeat(usize::from(body)),
    }
  }

  /// The operation a generated step runs (a policy's staling runs the operations the partition answers).
  fn op_of(step: &Step) -> Option<Op> {
    Some(match *step {
      Step::Create(repo) => created(repo),
      Step::Destroy(repo) => Op::RepositoryDestroyed {
        repository: repository_id(repo),
      },
      Step::Comment { .. } => Op::ReviewCommented {
        record: comment_of(step),
      },
      Step::Move {
        at: (repo, request, id),
        version,
        span,
      } => {
        let (start, len) = span_of(span);
        Op::ReviewCommentMoved {
          repository: repository_id(repo),
          request: request_id(request),
          id: u64::from(id),
          start,
          len,
          version: u64::from(version),
        }
      }
      Step::Outdate {
        at: (repo, request, id),
        version,
      } => Op::ReviewCommentOutdated {
        repository: repository_id(repo),
        request: request_id(request),
        id: u64::from(id),
        version: u64::from(version),
      },
      Step::Approve {
        at: (repo, request, approver),
        version,
        ended,
      } => {
        let mut record = approval(repo, request, approver, u64::from(version));
        record.stale_at = ended.then_some(u64::from(version) + 1);
        Op::ReviewApproved { record }
      }
      Step::Stale {
        at: (repo, request, approver),
        version,
      } => Op::ReviewApprovalStaled {
        repository: repository_id(repo),
        request: request_id(request),
        approver: person(approver),
        version: u64::from(version),
      },
      Step::StaleBy { .. } => return None,
      Step::Close(repo, request) => Op::ReviewClosed {
        repository: repository_id(repo),
        request: request_id(request),
      },
    })
  }

  proptest! {
    #![proptest_config(slates_test_seeds::unseeded(ProptestConfig { cases: slates_test_seeds::cases(400), ..ProptestConfig::default() }))]

    /// A-596 (§4.17.1 shared piece 6). Do: run a generated history of repository creates and destroys, comments
    /// written, moved and outdated, approvals made and made stale (alone, and as a policy over every approval of a
    /// request names them), and reviews closed, over two repositories' requests on a partition whose merge byte budget
    /// holds a few records; after every step rebuild the partition from its snapshot and go on from the rebuilt one.
    /// Expect: every answer (accepted, or which refusal) and the whole record state equal the design's rules kept over
    /// plain maps, the policy's staling names exactly the standing approvals bound to an earlier version that it
    /// covers, and the rebuilt partition holds the same records and refuses at the same budget.
    #[test]
    fn the_review_records_equal_the_rules_over_every_history(
      steps in proptest::collection::vec(step(), 0..120),
    ) {
      let mut partition = Partition::new(caps(), 0);
      let mut model = Model::default();
      for step in &steps {
        if let Step::StaleBy { at: (repo, request), version, named } = *step {
          let version = u64::from(version);
          let ops = partition.reviews().staled_by(repository_id(repo), request_id(request), version, |held| {
            (1u16 << held.approver.key()[1]) & u16::from(named) != 0
          });
          prop_assert_eq!(&ops, &model.stale_by((repo, request), version, named));
          for op in &ops {
            prop_assert_eq!(run(&mut partition, op), Ok(()));
          }
        } else if let Some(op) = op_of(step) {
          let real = run(&mut partition, &op);
          prop_assert_eq!(real, model.step(step), "step {:?}", step);
        }
        prop_assert_eq!(records(&partition), model.records());
        let rebuilt = Partition::from_snapshot(&partition.to_snapshot(0), caps(), 0).unwrap();
        prop_assert_eq!(records(&rebuilt), records(&partition));
        partition = rebuilt;
      }
    }
  }
}
