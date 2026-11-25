//! An import's revisions advanced in RAM and committed as one entry (C2 stage C, the green's side; A-814; main's order
//! of 2026-11-21 04:11 and 06:11; the split proposed in inbox/w-svn-from-w-rewrite-ops-stage-c-split.md).
//!
//! **Why.** A Subversion import landed each revision as its own changeset on the node's store, each one register
//! commit, so git-2000's 2,001 revisions took 2,001 landings: 24.5 s against 8.2 s before the store, about 8 ms a
//! revision, one `F_FULLFSYNC` of the store's owner per landing (inbox/main-from-w-svn-stage-c-analysis.md). One landing
//! an import needs the green, and the records (w-svn's side), advanced through the import's revisions in RAM without
//! their chain entries, then committed in one transaction, a refusal giving both back.
//!
//! **What.** An [`ImportBatch`] holds a scratch member of the green's lineage made at its head
//! (`Greens::preview_at_head`, O(1)):
//! - Each revision's work, built against the scratch member ([`ImportBatch::with_engine`]), is judged and committed
//!   into it ([`ImportBatch::stage`]). The green itself shows nothing of the import.
//! - Each work is numbered as the green's: based at the green's head and each revision after it. The scratch member,
//!   whose own versions count from 0, judges it with its base read in its own numbering, so each chain entry is the
//!   real one a holder recomputes (§4.16 "Apply on holders").
//! - [`ImportBatch::commit`] appends every version's chain entry and the register entry (the import's one changeset's
//!   reference, w-svn's) as one log record (`repository::push::append_versions`). It then applies each version to the
//!   green as its entry records it (`Green::apply_recorded`, what a recovery replays) and retires the scratch member.
//! - A refusal before the record ([`ImportBatch::abandon`], or the commit refused) leaves the green and its chain as
//!   they were: nothing to roll back in the engine.
//!
//! **A crash.** Before the record, nothing is durable and the green is as it was. After it, the record holds every
//! version, and a replay applies them as it applies any chain.
//!
//! **Bounds.** A batch holds its staged works until it commits, so its caller bounds it: a batch of the revisions one
//! changeset holds, within the store's changeset bound (w-svn's side).

use slates_db::catalog::{Principal, RepositoryId, VolumeId as DbVolumeId};
use slates_ipc::protocol::Refusal;
use slates_merge::chain_entry::Applied;
use slates_merge::engine::{CommitStep, Green, Increment, Outcome};

use crate::greens::Preview;
use crate::repository::push::{AppendRefused, EntryParts};
use crate::state::ShardState;

/// Counter: a revision staged into an import's batch. Format: a counter name in the daemon's status report.
pub(crate) const STAGED: &str = "repository.import_batch.staged";
/// Counter: an import's batch committed as one entry. Format: a counter name.
pub(crate) const COMMITTED: &str = "repository.import_batch.committed";
/// Counter: the times a staged verdict's commit, checking its reads (A-920 piece 3a), met named texts the scratch member
/// held by name alone and was lent them from the content plane. Format: a counter name.
pub(crate) const STAGE_COMMIT_LENT: &str = "repository.import_batch.commit_lent";

/// One revision's work staged in a batch: the work as the green numbers it, what its verdict resolved it to, the head
/// identity at its version where its record goes to holders, and the retention its verdict reserved.
pub(crate) struct Staged {
  /// The work, based at the green's version before it.
  pub(crate) increment: Increment,
  /// What its verdict resolved it to.
  pub(crate) applied: Vec<Applied>,
  /// The head identity at its version, where the record goes to holders.
  pub(crate) identity: Option<[u8; 32]>,
  /// The history and change index its verdict reserved.
  pub(crate) reserved: usize,
}

/// An import's revisions advanced in RAM (module docs).
pub(crate) struct ImportBatch {
  /// The import's green.
  green: DbVolumeId,
  /// The green's head when the batch began: the first work's base.
  base: u64,
  /// The scratch member the works are judged and committed into.
  preview: Option<Preview>,
  /// The works staged, in order.
  staged: Vec<Staged>,
  /// The green's retention the batch may reserve: its cap less what it holds once its history is folded as far as its
  /// readers let it, taken when the batch began (`append_versions` secures every staged version's reservation at once,
  /// so a batch past it is refused whole at its landing).
  room: u64,
  /// What the staged versions reserved of it.
  reserved: u64,
  /// The bytes the batch's chain entries may take of the partition's green chains (`caps.green_chain_bytes`): their
  /// room when the batch began and this green's own chain, which the landing's admission checkpoints where it must
  /// (`push::append_versions`), taken when the batch began (A-895).
  chain_room: usize,
  /// What the staged versions' chain entries take of it.
  entries: usize,
  /// The work whose reservation is being taken a slice at a time ([`ImportBatch::stage_step`], A-920 piece 3b): judged
  /// once it is taken.
  reserving: Option<ReservingWork>,
  /// The work being judged into the scratch member a slice at a time ([`ImportBatch::stage_step`], A-920 piece 3c):
  /// kept, its commit begun, or refused once judged.
  judging: Option<JudgingWork>,
  /// The work whose accepted verdict is being committed into the scratch member a slice at a time
  /// ([`ImportBatch::stage_step`]): kept once its commit ends.
  pending: Option<Pending>,
}

/// A work being judged ([`ImportBatch::judge_reserved`]), in the scratch member's numbering until its judgment.
struct JudgingWork {
  /// The work.
  work: Increment,
  /// Its judge so far (`Green::begin_judging`).
  judging: slates_merge::engine::Judging,
  /// The batch's head it is based at, in the green's numbering.
  head: u64,
  /// The scratch member's offset from the green's numbering.
  offset: u64,
  /// What the batch's chains may still take, where the batch holds any work.
  chain_left: Option<usize>,
  /// What its reservation reserved.
  reservation: usize,
  /// Whether its record goes to holders.
  holders: bool,
  /// Whether it is judged again, lent the named texts its first judge read unlent.
  judged_again: bool,
}

/// A work whose reservation is being taken ([`ImportBatch::stage_begin`]).
struct ReservingWork {
  /// The work, in the green's numbering.
  work: Increment,
  /// Its reservation's walk so far (`Green::begin_reserving`).
  reserving: slates_merge::engine::Reserving,
  /// The batch's head it is based at, in the green's numbering.
  head: u64,
}

/// A work judged and accepted into the scratch member, its verdict being committed a slice at a time (A-920 piece 1).
struct Pending {
  /// The work, in the green's numbering.
  work: Increment,
  /// Its verdict's commit under way (`Green::begin_committing`).
  committing: slates_merge::engine::Committing,
  /// What its verdict needs once committed: the scratch member's offset from the green's numbering, what it reserved,
  /// and whether it changed nothing.
  verdict: PendingVerdict,
  /// The batch's head as it was judged, in the green's numbering.
  head: u64,
  /// Whether its record goes to holders.
  holders: bool,
  /// The named texts its commit's check of its reads asked for and was lent (A-920 piece 3a): one asked again is
  /// refused, never lent twice.
  asked: std::collections::BTreeSet<slates_merge::named::Identity>,
}

/// What a step of a pending verdict's commit left ([`ImportBatch::stage_step`]).
enum Stepped {
  /// More of it is left.
  More,
  /// Its check of its reads met these named texts unlent.
  Lend(std::collections::BTreeSet<slates_merge::named::Identity>),
  /// Committed: its verdict.
  Committed(StagedVerdict),
}

/// What [`finish_staged`] needs of a pending verdict ([`Pending`]).
struct PendingVerdict {
  offset: u64,
  reserved: usize,
  identical: bool,
}

/// Why a revision's work could not be staged.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum StageRefused {
  /// The work is not based at the batch's head.
  NotAtHead {
    /// The batch's head.
    head: u64,
  },
  /// The verdict did not accept it: the outcome.
  NotAccepted(Outcome),
  /// The scratch member is gone or busy.
  Unavailable,
  /// The work's reservation would take the batch past the green's retention room ([`ImportBatch::room`]): with works
  /// staged, the batch lands first and the work begins the next (goal 93, main's red of 2026-11-23: one batch secured
  /// 422,803,669 bytes against a cap of 373,475,417); with none (`reserved` 0), the work alone is past the room and is
  /// refused, typed, naming both.
  PastRoom {
    /// What the batch reserved.
    reserved: u64,
    /// The work's reservation.
    work: u64,
    /// The room.
    room: u64,
  },
  /// The work's chain entry would take the batch past its chains' room (`ImportBatch::chain_room`, A-895): the batch
  /// lands first, and the work begins the next. Never refused to a batch with nothing staged, whose landing's append
  /// refuses an entry past the room alone, typed (`TableAtCapacity { table: "green_chains" }`).
  PastChainRoom {
    /// What the staged versions' entries take.
    entries: usize,
    /// The work's entry.
    entry: usize,
    /// The room.
    room: usize,
  },
}

impl ImportBatch {
  /// A batch over `green`, a member of its repository's lineage, at its head; refused `ContentUnavailable` where no
  /// scratch member can be made (a green standing alone, its lineage busy).
  pub(crate) fn begin(state: &mut ShardState, green: DbVolumeId) -> Result<ImportBatch, Refusal> {
    let base = state
      .greens
      .get(&green)
      .map(|engine| engine.head())
      .ok_or(Refusal::NotFound)?;
    // The green's history folded as far as its readers let it, so what the batch may reserve is what its landing will
    // find (`verbs::secure_retention` settles first too).
    let _ = crate::merge_service::settle_green_retention(state, green);
    let cap = u64::try_from(crate::merge_service::rejected_cache_budget(state)).unwrap_or(u64::MAX);
    let charged = state.green_retention.get(&green).copied().unwrap_or(0);
    let partition = state.db.partition();
    let own_chain = partition
      .green_chain(green)
      .iter()
      .map(Vec::len)
      .fold(0usize, usize::saturating_add);
    let chain_room = partition.green_chain_room().saturating_add(own_chain);
    let preview = state
      .greens
      .preview_at_head(&green)
      .map_err(|_| Refusal::ContentUnavailable)?;
    Ok(ImportBatch {
      green,
      base,
      preview: Some(preview),
      staged: Vec::new(),
      reserving: None,
      judging: None,
      pending: None,
      room: cap.saturating_sub(charged),
      reserved: 0,
      chain_room,
      entries: 0,
    })
  }

  /// The green's retention the batch may reserve (its cap less what the green holds after a fold, when it began).
  pub(crate) fn room(&self) -> u64 {
    self.room
  }

  /// The version the next work is based at, in the green's numbering: its head and every revision staged.
  pub(crate) fn head(&self) -> u64 {
    self
      .base
      .saturating_add(u64::try_from(self.staged.len()).unwrap_or(u64::MAX))
  }

  /// The revisions staged.
  pub(crate) fn staged(&self) -> usize {
    self.staged.len()
  }

  /// `read` over the green as the staged revisions leave it (the scratch member): what the next revision's work is built
  /// against. None where the scratch member is gone or busy.
  pub(crate) fn with_engine<R>(
    &self,
    state: &mut ShardState,
    read: impl FnOnce(&Green) -> R,
  ) -> Option<R> {
    let preview = self.preview.as_ref()?;
    state
      .greens
      .with_preview(preview, |engine| read(engine))
      .ok()
  }

  /// [`ImportBatch::with_engine`] over `greens` alone, so the caller reads the rest of the shard's state beside it (a
  /// Subversion revision's increment is built from its records and the scratch member together, C2 stage C).
  pub(crate) fn with_engine_in<R>(
    &self,
    greens: &mut crate::greens::Greens,
    read: impl FnOnce(&Green) -> R,
  ) -> Option<R> {
    let preview = self.preview.as_ref()?;
    greens.with_preview(preview, |engine| read(engine)).ok()
  }

  /// Judges `work`, based at the batch's head in the green's numbering, into the scratch member, and keeps it: the
  /// version it is in the green's numbering. Refused, nothing kept, where it is not based at the head or not accepted.
  /// [`ImportBatch::stage_begin`] and [`ImportBatch::stage_step`] carried to their end.
  pub(crate) fn stage(
    &mut self,
    state: &mut ShardState,
    work: Increment,
  ) -> Result<u64, StageRefused> {
    self.stage_begin(state, work)?;
    loop {
      if let Some(staged) = self.stage_step(state, &mut || false) {
        return staged;
      }
    }
  }

  /// Begins staging `work` ([`ImportBatch::stage`]): its reservation begun, taken a slice at a time by
  /// [`ImportBatch::stage_step`], then the work judged into the scratch member, and kept where nothing of it is to
  /// commit, else its verdict's commit taken on. Refused where it is not based at the batch's head.
  pub(crate) fn stage_begin(
    &mut self,
    state: &mut ShardState,
    work: Increment,
  ) -> Result<(), StageRefused> {
    let head = self.head();
    if work.base != head {
      return Err(StageRefused::NotAtHead { head });
    }
    let preview = self.preview.as_ref().ok_or(StageRefused::Unavailable)?;
    let reserving = state
      .greens
      .with_preview(preview, |engine| engine.begin_reserving(&work))
      .map_err(|_| StageRefused::Unavailable)?;
    self.reserving = Some(ReservingWork {
      work,
      reserving,
      head,
    });
    Ok(())
  }

  /// One step of a work's reservation ([`ImportBatch::stage_begin`]), until `spent`: none while it goes on; once it
  /// ends, the work judged ([`ImportBatch::judge_reserved`]).
  fn reserve_step(
    &mut self,
    state: &mut ShardState,
    mut at: ReservingWork,
    spent: &mut dyn FnMut() -> bool,
  ) -> Option<Result<u64, StageRefused>> {
    let Some(preview) = self.preview.as_ref() else {
      return Some(Err(StageRefused::Unavailable));
    };
    let stepped = state.greens.with_preview(preview, |engine| {
      let walked = engine.reserving_step(&at.work, &mut at.reserving, spent)?;
      Some(reservation_of(engine, &mut at.work, (at.head, walked)))
    });
    match stepped {
      Err(_) => Some(Err(StageRefused::Unavailable)),
      Ok(None) => {
        self.reserving = Some(at);
        None
      }
      Ok(Some(reservation)) => self
        .judge_reserved(state, at.work, (at.head, reservation))
        .transpose(),
    }
  }

  /// `work`, its reservation taken, judged into the scratch member at the batch's head `head`: within the green's
  /// retention room first (the batch lands first where the work would take it past, and a work past the room alone is
  /// refused); then kept where nothing of it is to commit (its version), else its verdict's commit begun (none).
  fn judge_reserved(
    &mut self,
    state: &mut ShardState,
    work: Increment,
    (head, reservation): (u64, usize),
  ) -> Result<Option<u64>, StageRefused> {
    let work_reserves = u64::try_from(reservation).unwrap_or(u64::MAX);
    if self.reserved.saturating_add(work_reserves) > self.room {
      return Err(StageRefused::PastRoom {
        reserved: self.reserved,
        work: work_reserves,
        room: self.room,
      });
    }
    let holders = crate::merge_service::record_goes_to_holders(state, self.green);
    // A batch of none takes any work's entry, which its landing's append refuses typed where it is past the room alone.
    let chain_left =
      (!self.staged.is_empty()).then(|| self.chain_room.saturating_sub(self.entries));
    self.begin_judging_work(
      state,
      (work, head, reservation),
      (holders, chain_left, false),
    )?;
    Ok(None)
  }

  /// Begins judging `work` into the scratch member a slice at a time (A-920 piece 3c: the judge whole was 47 ms at 9,000
  /// files, debug, its verdict's compute 40): moved into the member's numbering, its judge begun, then taken on by
  /// [`ImportBatch::stage_step`].
  fn begin_judging_work(
    &mut self,
    state: &mut ShardState,
    (mut work, head, reservation): (Increment, u64, usize),
    (holders, chain_left, judged_again): (bool, Option<usize>, bool),
  ) -> Result<(), StageRefused> {
    let preview = self.preview.as_ref().ok_or(StageRefused::Unavailable)?;
    let (judging, offset) = state
      .greens
      .with_preview(preview, |engine| {
        // The scratch member counts its versions from 0: the work's base, read there, and each copy's source version
        // (a Subversion copy names the head it copies from, w-svn's C2 stage C) moved into its numbering.
        let offset = head.saturating_sub(engine.head());
        work.base = engine.head();
        in_numbering(&mut work, |version| version.checked_sub(offset));
        (engine.begin_judging(&work), offset)
      })
      .map_err(|_| StageRefused::Unavailable)?;
    self.judging = Some(JudgingWork {
      work,
      judging,
      head,
      offset,
      chain_left,
      reservation,
      holders,
      judged_again,
    });
    Ok(())
  }

  /// One step of a work's judge ([`ImportBatch::begin_judging_work`]), until `spent`: none while it goes on; once
  /// judged, the work kept, its commit begun, or refused ([`ImportBatch::judged`]).
  fn judge_step(
    &mut self,
    state: &mut ShardState,
    mut at: JudgingWork,
    spent: &mut dyn FnMut() -> bool,
  ) -> Option<Result<u64, StageRefused>> {
    let Some(preview) = self.preview.as_ref() else {
      return Some(Err(StageRefused::Unavailable));
    };
    let stepped = state.greens.with_preview_mut(preview, |engine| {
      let judgment = engine.judging_step(&mut at.work, &mut at.judging, spent)?;
      let judged = judge_end(
        engine,
        (&mut at.work, judgment),
        (at.offset, at.holders),
        (at.chain_left, at.reservation),
      );
      // What was lent for the verdict stays lent for its commit, which may make files over those texts.
      if !matches!(judged, Judging::Committing(_)) {
        engine.let_named_bytes_go();
      }
      Some(judged)
    });
    match stepped {
      Err(_) => Some(Err(StageRefused::Unavailable)),
      Ok(None) => {
        self.judging = Some(at);
        None
      }
      Ok(Some(judged)) => self.judged(state, at, judged).transpose(),
    }
  }

  /// `at`, judged `judged`: kept where nothing of it is to commit (its version), its verdict's commit begun (none),
  /// judged again once lent the named texts its verdict read unlent ([`ImportBatch::judge_lent`], none), or refused.
  fn judged(
    &mut self,
    state: &mut ShardState,
    at: JudgingWork,
    judged: Judging,
  ) -> Result<Option<u64>, StageRefused> {
    match judged {
      Judging::Judged(staged) => self
        .keep_staged(state, at.work, (Ok(staged), at.head))
        .map(Some),
      Judging::Committing(begun) => {
        let (committing, verdict) = *begun;
        let JudgingWork {
          mut work,
          head,
          holders,
          ..
        } = at;
        work.base = head;
        self.pending = Some(Pending {
          work,
          committing,
          verdict,
          head,
          holders,
          asked: std::collections::BTreeSet::new(),
        });
        Ok(None)
      }
      Judging::PastChain { entry } => Err(StageRefused::PastChainRoom {
        entries: self.entries,
        entry,
        room: self.chain_room,
      }),
      Judging::Unlent(unlent) => self.judge_lent(state, at, &unlent).map(|()| None),
    }
  }

  /// `at`, whose verdict read the named texts `unlent` the scratch member holds by name alone with none lent (A-760
  /// (b) 4d), judged again once they are lent from the content plane; refused where it was judged again already
  /// (counted) or a text is not read.
  fn judge_lent(
    &mut self,
    state: &mut ShardState,
    at: JudgingWork,
    unlent: &std::collections::BTreeSet<slates_merge::named::Identity>,
  ) -> Result<(), StageRefused> {
    if at.judged_again {
      state.count(crate::vcs_store::name_holds::NAME_LEND_MISSED, 1);
      return Err(StageRefused::Unavailable);
    }
    let lent = crate::vcs_store::name_holds::read_from_plane(state, unlent, None)
      .map_err(|_| StageRefused::Unavailable)?;
    state.count(crate::vcs_store::name_holds::NAME_LENT_VERDICTS, 1);
    let preview = self.preview.as_ref().ok_or(StageRefused::Unavailable)?;
    state
      .greens
      .with_preview_mut(preview, |engine| {
        for (identity, bytes) in lent {
          engine.lend(identity, bytes);
        }
      })
      .map_err(|_| StageRefused::Unavailable)?;
    self.begin_judging_work(
      state,
      (at.work, at.head, at.reservation),
      (at.holders, at.chain_left, true),
    )
  }

  /// Takes the commit [`ImportBatch::stage_begin`] began on a unit at a time, at least one, until `spent`: the version
  /// it kept once the commit ends, as [`ImportBatch::stage`] answers; none while more remains. The texts lent for its
  /// verdict are given back once it ends.
  pub(crate) fn stage_step(
    &mut self,
    state: &mut ShardState,
    spent: &mut dyn FnMut() -> bool,
  ) -> Option<Result<u64, StageRefused>> {
    if let Some(reserving) = self.reserving.take() {
      return self.reserve_step(state, reserving, spent);
    }
    if let Some(judging) = self.judging.take() {
      return self.judge_step(state, judging, spent);
    }
    // A step with nothing begun is refused, never "more": its caller steps until an end.
    let Some(mut pending) = self.pending.take() else {
      return Some(Err(StageRefused::Unavailable));
    };
    let Some(preview) = self.preview.as_ref() else {
      return Some(Err(StageRefused::Unavailable));
    };
    let stepped = state.greens.with_preview_mut(preview, |engine| {
      match engine.committing_step(&mut pending.committing, spent) {
        CommitStep::More => Stepped::More,
        CommitStep::Lend(unlent) => Stepped::Lend(unlent),
        CommitStep::Committed(version, applied) => {
          let staged = finish_staged(
            engine,
            &pending.work,
            (&pending.verdict, pending.holders),
            (version, applied),
          );
          engine.let_named_bytes_go();
          Stepped::Committed(staged)
        }
      }
    });
    match stepped {
      Err(_) => Some(Err(StageRefused::Unavailable)),
      Ok(Stepped::More) => {
        self.pending = Some(pending);
        None
      }
      Ok(Stepped::Lend(unlent)) => self.lend_pending(state, pending, unlent),
      Ok(Stepped::Committed(staged)) => {
        let Pending { work, head, .. } = pending;
        Some(self.keep_staged(state, work, (Ok(staged), head)))
      }
    }
  }

  /// Lends the scratch member the named texts `unlent` that `pending`'s commit met unlent, read from the content plane
  /// (A-920 piece 3a: its reads checked by its commit, a slice at a time), and keeps it pending: none while it goes
  /// on; refused where a text was asked for again once lent (counted, as a judge's second miss is) or is not read.
  fn lend_pending(
    &mut self,
    state: &mut ShardState,
    mut pending: Pending,
    unlent: std::collections::BTreeSet<slates_merge::named::Identity>,
  ) -> Option<Result<u64, StageRefused>> {
    if !unlent.is_disjoint(&pending.asked) {
      state.count(crate::vcs_store::name_holds::NAME_LEND_MISSED, 1);
      return Some(Err(StageRefused::Unavailable));
    }
    let Some(preview) = self.preview.as_ref() else {
      return Some(Err(StageRefused::Unavailable));
    };
    let Ok(lent) = crate::vcs_store::name_holds::read_from_plane(state, &unlent, None) else {
      return Some(Err(StageRefused::Unavailable));
    };
    let given = state.greens.with_preview_mut(preview, |engine| {
      for (identity, bytes) in lent {
        engine.lend(identity, bytes);
      }
    });
    if given.is_err() {
      return Some(Err(StageRefused::Unavailable));
    }
    state.count(STAGE_COMMIT_LENT, 1);
    pending.asked.extend(unlent);
    self.pending = Some(pending);
    None
  }

  /// Keeps `work`, its verdict `judged` at the batch's head `head`: the version it is in the green's numbering; refused
  /// where the verdict did not accept it.
  fn keep_staged(
    &mut self,
    state: &mut ShardState,
    mut work: Increment,
    (judged, head): (Result<StagedVerdict, StageRefused>, u64),
  ) -> Result<u64, StageRefused> {
    work.base = head;
    let (outcome, applied, identity, reserved, entry) = judged?;
    let (Outcome::Accepted { .. } | Outcome::Identical { .. }, Some(applied)) = (&outcome, applied)
    else {
      return Err(StageRefused::NotAccepted(outcome));
    };
    self.reserved = self
      .reserved
      .saturating_add(u64::try_from(reserved).unwrap_or(u64::MAX));
    self.entries = self.entries.saturating_add(entry);
    self.staged.push(Staged {
      increment: work,
      applied,
      identity,
      reserved,
    });
    state.count(STAGED, 1);
    Ok(head.saturating_add(1))
  }

  /// Commits the staged revisions with `parts` (the import's changeset reference, w-svn's) as one entry of
  /// `repository`'s register by `principal`, its owner (module docs): recorded, then applied to the green a slice at a
  /// time by what it answers (`RecordedBatch::apply_slice`; w-svn for main's ruling of 2026-11-21 12:43, the apply of a
  /// whole import in one step having held the shard past its watchdog). The scratch member is retired either way;
  /// refused, the green and its chain are as they were.
  pub(crate) fn commit(
    mut self,
    state: &mut ShardState,
    (principal, repository): (&Principal, RepositoryId),
    parts: EntryParts,
  ) -> Result<crate::repository::push::RecordedBatch, AppendRefused> {
    let staged = std::mem::take(&mut self.staged);
    let green = self.green;
    self.retire(state);
    let made = crate::repository::push::append_versions(
      state,
      (principal, repository),
      green,
      staged,
      parts,
    )?;
    state.count(COMMITTED, 1);
    Ok(made)
  }

  /// Gives the batch up: nothing of it applied or recorded, the scratch member retired.
  pub(crate) fn abandon(mut self, state: &mut ShardState) {
    self.retire(state);
  }

  /// Retires the scratch member, once.
  fn retire(&mut self, state: &mut ShardState) {
    if let Some(preview) = self.preview.take() {
      state.greens.retire_preview(preview);
    }
  }
}

/// Moves each `Copy` operation of `work` to the version `moved` answers for its own: from the green's numbering into a
/// scratch member's and back ([`ImportBatch::stage`]). A copy `moved` answers none for (a version before the scratch
/// member's first) is left as it is, and its verdict refuses it as a copy past what the member holds.
fn in_numbering(work: &mut Increment, moved: impl Fn(u64) -> Option<u64>) {
  for op in work
    .doc
    .ops
    .iter_mut()
    .filter(|op| op.kind == slates_merge::ops_doc::OpKind::Copy)
  {
    if let Some(version) = moved(op.at) {
      op.at = version;
    }
  }
}

impl Drop for ImportBatch {
  /// A batch dropped without its commit or abandon (a task ended early) gives its scratch member back where the shard's
  /// state is not borrowed then; counted otherwise.
  fn drop(&mut self) {
    if let Some(preview) = self.preview.take() {
      let _ = crate::state::with_state_counted(|state| state.greens.retire_preview(preview));
    }
  }
}

/// What a staged work's verdict in the scratch member gives: its outcome, what its entry records, the member's head
/// identity where holders take its record, the history and change bytes reserved for it, and its chain entry's bytes.
type StagedVerdict = (
  Outcome,
  Option<Vec<Applied>>,
  Option<[u8; 32]>,
  usize,
  usize,
);

/// What judging a staged work came to ([`judge_end`]).
enum Judging {
  /// Judged into the scratch member.
  Judged(StagedVerdict),
  /// Accepted, its chain entry (`entry` bytes) past what the batch's chains may still take: nothing of it applied.
  PastChain {
    /// The entry's bytes.
    entry: usize,
  },
  /// Refused naming every named text the verdict read that the member holds by name alone with none lent.
  Unlent(std::collections::BTreeSet<slates_merge::named::Identity>),
  /// Accepted, its commit begun, to be taken on a slice at a time ([`ImportBatch::stage_step`]).
  Committing(Box<(slates_merge::engine::Committing, PendingVerdict)>),
}

/// The history and change bytes `work`, based at the batch's head `head` (in the green's numbering), would reserve in
/// the scratch member `engine`, its walks' `walked` taken already (`Green::reserving_step`) and its copies' and
/// replaces' shares added, read as its judge ([`ImportBatch::begin_judging_work`]) reads it: moved into the member's numbering and back, `work` as it
/// was.
fn reservation_of(engine: &Green, work: &mut Increment, (head, walked): (u64, usize)) -> usize {
  let offset = head.saturating_sub(engine.head());
  let base = work.base;
  work.base = engine.head();
  in_numbering(work, |version| version.checked_sub(offset));
  let reserved = engine.reservation_shares(work, walked);
  in_numbering(work, |version| version.checked_add(offset));
  work.base = base;
  reserved
}

/// `work`'s judgment into the scratch member `engine`, judged in the member's numbering (offset `offset` from the
/// green's), taken: past the batch's chains' room, its commit begun where accepted, refused naming every named text the
/// verdict read that the member holds by name alone with none lent ([`Green::unlent_reads`], A-760 (b) 4d), or judged;
/// `work` back in the green's numbering. `reserved` is what the batch's room check computed of this work at this head
/// (`reservation_of`), the scratch member unchanged since but for the texts lent it (A-920 piece 0). An accepted
/// verdict's chain entry is sized before anything of it applies (A-895); its commit's reads are checked by its commit
/// (A-920 piece 3a).
fn judge_end(
  engine: &mut Green,
  (work, judgment): (&mut Increment, slates_merge::engine::Judgment),
  (offset, holders): (u64, bool),
  (chain_left, reserved): (Option<usize>, usize),
) -> Judging {
  if let (Some(left), slates_merge::engine::Judgment::Accepted(held)) = (chain_left, &judgment) {
    let entry = slates_merge::chain_entry::encoded_len(work, held.applied(), holders);
    if entry > left {
      in_numbering(work, |version| version.checked_add(offset));
      return Judging::PastChain { entry };
    }
  }
  // Accepted: committed into the scratch member a slice at a time from here (A-920 piece 1); the work back in the green's
  // numbering now, since the commit holds what it needs of it.
  if let slates_merge::engine::Judgment::Accepted(held) = judgment {
    in_numbering(work, |version| version.checked_add(offset));
    let verdict = PendingVerdict {
      offset,
      reserved,
      identical: held.applied().is_empty(),
    };
    return Judging::Committing(Box::new((engine.begin_committing(held), verdict)));
  }
  let (outcome, mut applied) = engine.commit_judgment(judgment);
  let unlent = match outcome {
    Outcome::ContentUnread { identity }
      if !work.named.runs.iter().any(|run| run.identity == identity) =>
    {
      Some(engine.unlent_reads(work))
    }
    _ => None,
  };
  let identity = holders.then(|| engine.head_identity());
  // Back into the green's numbering: the work's copies, and those its verdict records.
  in_numbering(work, |version| version.checked_add(offset));
  if let Some(unlent) = unlent {
    return Judging::Unlent(unlent);
  }
  if let Some(applied) = applied.as_mut() {
    for step in applied.iter_mut() {
      if let Applied::CopyTree { version, .. } = step {
        *version = version.saturating_add(offset);
      }
    }
  }
  let entry = applied.as_deref().map_or(0, |applied| {
    slates_merge::chain_entry::encoded_len(work, applied, holders)
  });
  Judging::Judged((outcome, applied, identity, reserved, entry))
}

/// What a pending verdict ([`Pending`]) staged once its commit into the scratch member `engine` ended as `version`,
/// `applied`: as [`judge_end`] answers an accepted work, its verdict's copies moved back into the green's numbering
/// and its chain entry sized.
fn finish_staged(
  engine: &mut Green,
  work: &Increment,
  (verdict, holders): (&PendingVerdict, bool),
  (version, mut applied): (u64, Vec<Applied>),
) -> StagedVerdict {
  let outcome = if verdict.identical {
    Outcome::Identical { version }
  } else {
    Outcome::Accepted { version }
  };
  let identity = holders.then(|| engine.head_identity());
  for step in &mut applied {
    if let Applied::CopyTree { version, .. } = step {
      *version = version.saturating_add(verdict.offset);
    }
  }
  let entry = slates_merge::chain_entry::encoded_len(work, &applied, holders);
  (outcome, Some(applied), identity, verdict.reserved, entry)
}
