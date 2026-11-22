//! A push's increments held in the owner's content store a slice a turn before its landing's step (01a1ff2e's ruling of
//! 2026-11-21 17:26, option (a); main 01a20182's 21:35 order; §4.17.1 "A push is a work", lane (A)'s chain entry format
//! 6): the one step that appends a push's entry names each work's inputs, held already, and copies or hashes none of
//! them.
//!
//! **Why.** The landing's step held each work's encoding as its version's inputs archive whole
//! (`merge_service::hold_increment`): every chunk hashed with BLAKE3 and copied into its block in that one step. A
//! 64+32 MiB Mercurial push spun its shard past the watchdog's window there (measured 2026-11-21, MacBook arm64, debug,
//! `stock_hg_pushes_large_files_and_the_daemon_keeps_serving`: the verdict 7-220 ms, then `hold_increments` past
//! 3,480 ms of CPU, the daemon killed and every open stream cut).
//!
//! **How.** Each work is encoded once ([`AheadHold::new`]), then a unit a step, as a green's checkpoint is held
//! (`merge_service::CheckpointHold`, A-579): a chunk hashed into the manifest's extents; the stage opened on the green's
//! own object, placed for the version after its head, once every chunk is hashed; a chunk copied into the stage; the
//! stage completed. Its inputs ([`HeldAhead`]) go with the push to its landing, whose step names them
//! (`merge_service::increment_already_held`) while the content store holds them; a work not held ahead is held whole as
//! before, and a small one costs that step little.
//!
//! **Given back.** What is held ahead and named by no entry is given back after the landing, whatever it answered
//! ([`give_back`], `merge_service::release_refused_increment`): a refused or moved landing leaves nothing. Content is
//! named by its identity, so a crash between the hold and the entry leaves only chunks no entry names, which recovery
//! gives back (`merge_service::release_unnamed_increments`, placed past the head).
//!
//! **Owed** (named in GAPS): the encoding is still one step (a copy of the work, about 10 ms at 96 MiB in release);
//! git's large updates and P2's materialize take the same hold before their own landing steps.

use slates_cluster::content::{ContentRefusal, Placed};
use slates_db::catalog::VolumeId as DbVolumeId;
use slates_db::register::ObjectId;
use slates_ipc::protocol::Refusal;
use slates_merge::chain_entry::Inputs;

use crate::state::ShardState;

/// Counter: a unit of a push's increment held ahead of its landing (a chunk hashed or copied, the stage opened or
/// completed). Format: a counter name in the daemon's status report.
pub(crate) const AHEAD_UNITS: &str = "push.held_ahead.units";
/// Counter: a push's increment held whole ahead of its landing. Format: a counter name in the daemon's status report.
pub(crate) const AHEAD_HELD: &str = "push.held_ahead.held";
/// Counter: a landing's step that named an increment held ahead, copying and hashing none of it. Format: a counter name
/// in the daemon's status report.
pub(crate) const AHEAD_NAMED: &str = "push.held_ahead.named";

/// One work's increment held ahead of its push's landing: its green, the work's identity, and the inputs its version's
/// entry names; `fresh` when this hold stored it, so only what it stored is given back.
#[derive(Clone, Copy, Debug)]
pub(crate) struct HeldAhead {
  /// The green the work moves.
  pub(crate) green: DbVolumeId,
  /// The work's identity (`Increment::id`).
  pub(crate) work: [u8; 32],
  /// The identity and length of its encoding as an inputs archive.
  pub(crate) inputs: Inputs,
  /// Whether the content store held it only from this hold.
  pub(crate) fresh: bool,
}

/// What one unit of an [`AheadHold`] left.
pub(crate) enum Ahead {
  /// Units are left for a later step.
  More,
  /// Held whole.
  Held(HeldAhead),
}

/// One work's increment being held ahead (module docs): its encoding, the extents hashed so far, and, once its stage is
/// open, the chunks copied into it.
pub(crate) struct AheadHold {
  /// The green the work moves.
  green: DbVolumeId,
  /// The work's identity.
  work: [u8; 32],
  /// The version its entry will record: the one after the green's head when the hold began.
  version: u64,
  /// The work's encoding (`Increment::encode`).
  encoding: Vec<u8>,
  /// The manifest's extents hashed so far, one a chunk in order.
  extents: Vec<slates_archive::manifest::Extent>,
  /// The encoding's bytes hashed so far.
  hashed: usize,
  /// The manifest's identity, once every chunk is hashed and its stage is opened.
  staging: Option<[u8; 32]>,
  /// The chunks copied into the stage so far.
  staged: usize,
}

impl AheadHold {
  /// The hold of `work`'s increment for `green`'s next version, `version` (module docs); its encoding made here.
  pub(crate) fn new(
    green: DbVolumeId,
    version: u64,
    work: &slates_merge::engine::Increment,
  ) -> Self {
    Self {
      green,
      work: work.id,
      version,
      encoding: work.encode(),
      extents: Vec::new(),
      hashed: 0,
      staging: None,
      staged: 0,
    }
  }

  /// One unit (module docs); refused typed where the content store refuses the stage or a chunk (its room, a newer
  /// placement on the green's object), nothing of this hold kept.
  pub(crate) fn unit(&mut self, state: &mut ShardState) -> Result<Ahead, Refusal> {
    state.count(AHEAD_UNITS, 1);
    let object = ObjectId(self.green.bytes);
    let Some(manifest) = self.staging else {
      return self.hash_or_open(state, object);
    };
    let Some(extent) = self.extents.get(self.staged) else {
      let completed = state.held_content.complete_stage(
        &mut crate::content_holder::hold_space(&mut state.store),
        object,
      );
      return match completed {
        Ok(identity) => Ok(Ahead::Held(self.held(state, identity, true))),
        Err(refusal) => Err(refused(refusal)),
      };
    };
    let start = usize::try_from(extent.offset).unwrap_or(usize::MAX);
    let end = usize::try_from(extent.len).map_or(usize::MAX, |len| start.saturating_add(len));
    let piece = self
      .encoding
      .get(start..end)
      .ok_or(Refusal::ContentUnavailable)?;
    state
      .held_content
      .stage_lent_piece(
        &mut crate::content_holder::hold_space(&mut state.store),
        object,
        &manifest,
        extent.chunk,
        piece,
      )
      .map_err(refused)?;
    self.staged = self.staged.saturating_add(1);
    Ok(Ahead::More)
  }

  /// A chunk hashed into the manifest's extents; or, every chunk hashed, the stage opened on the green's object (held
  /// already: nothing to copy, and nothing of it this hold's to give back).
  fn hash_or_open(&mut self, state: &mut ShardState, object: ObjectId) -> Result<Ahead, Refusal> {
    let chunk = crate::merge_origin::inputs_chunk_bytes().max(1);
    let end = self.hashed.saturating_add(chunk).min(self.encoding.len());
    if let Some(piece) = self
      .encoding
      .get(self.hashed..end)
      .filter(|piece| !piece.is_empty())
    {
      let offset = u64::try_from(self.hashed).unwrap_or(u64::MAX);
      self
        .extents
        .push(crate::merge_origin::inputs_extent(offset, piece));
      self.hashed = end;
      return Ok(Ahead::More);
    }
    let archive = crate::merge_origin::inputs_manifest_archive_of(
      self.extents.clone(),
      u64::try_from(self.encoding.len()).unwrap_or(u64::MAX),
      0,
      crate::merge_service::archive_page(state),
    );
    let identity = archive.manifest_identity();
    let opened = state.held_content.stage_fetched(
      &mut crate::content_holder::hold_space(&mut state.store),
      object,
      Placed {
        sequence: self.version,
      },
      &archive,
    );
    match opened {
      Ok(None) => Ok(Ahead::Held(self.held(state, identity, false))),
      Ok(Some(_)) => {
        self.staging = Some(identity);
        Ok(Ahead::More)
      }
      Err(refusal) => Err(refused(refusal)),
    }
  }

  /// The hold done: its inputs, counted.
  fn held(&self, state: &mut ShardState, identity: [u8; 32], fresh: bool) -> HeldAhead {
    state.count(AHEAD_HELD, 1);
    HeldAhead {
      green: self.green,
      work: self.work,
      inputs: Inputs {
        identity,
        len: u64::try_from(self.encoding.len()).unwrap_or(u64::MAX),
      },
      fresh,
    }
  }
}

/// The content store's refusal of a hold, in the push's words: its room `BudgetExceeded`, any other (a newer placement
/// took the green's stage) `ContentUnavailable`.
fn refused(refusal: ContentRefusal) -> Refusal {
  // Named in the daemon's log, so a run that meets it says which (main 01a20182's order of 2026-11-22 03:50: a fresh
  // repository's first push refused on CI's runners, not reproduced on a Mac).
  slates_machine::log_line!("slates-server: a push's increment held ahead was refused by the content store: {refusal:?}");
  match refusal {
    ContentRefusal::NoCapacity { available, .. } => Refusal::BudgetExceeded { available },
    _ => Refusal::ContentUnavailable,
  }
}

/// The inputs held ahead for `work` moving `green`, if any (the landing's step, `repository::push`).
pub(crate) fn held_for(
  ahead: &[HeldAhead],
  green: DbVolumeId,
  work: &[u8; 32],
) -> Option<HeldAhead> {
  ahead
    .iter()
    .find(|held| held.green == green && &held.work == work)
    .copied()
}

/// Gives back each of `ahead` this hold stored and no entry names (module docs): after its landing, whatever it answered;
/// one an entry named stays.
pub(crate) fn give_back(state: &mut ShardState, ahead: &[HeldAhead]) {
  for held in ahead {
    crate::merge_service::release_refused_increment(
      state,
      held.green,
      crate::merge_service::HeldIncrement {
        inputs: held.inputs,
        fresh: held.fresh,
      },
    );
  }
}
