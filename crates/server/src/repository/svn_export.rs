//! A Subversion repository's range of revisions exported as a dump into a new volume (A-652 part 3, its second half;
//! §4.17.4 piece 6; §4.15; R10; A-628, A-441, A-459).
//!
//! **The verb.** `ExportDump { repository, first, last, deltas, name }` is served on the partition the repository's name
//! hashes to, as every repository verb is, and for the repository's owner alone (`NotFound`, `Forbidden`; the parent
//! module's "Rights"). A repository of another tool is refused `Unsupported`. The range is checked as
//! `svn_repos_dump_fs4` checks it, refused `BadRequest` in its words (`dump.c` 2072-2080, `SVN_ERR_REPOS_BAD_ARGS`):
//! "Start revision F is greater than end revision L", "End revision L is invalid (youngest revision is Y)". The new
//! volume's name is checked as an archive's is (`AlreadyExists`; an alias at the name's own partition where another
//! holds it, A-236).
//!
//! **The dump** is what `svnadmin dump` writes for the same range (A-628's exporter, A-652's format 3): format 2, or
//! format 3 with `deltas`; a range starting after revision 0 written incrementally, as a migration in parts loads it. It
//! is written into a new plain volume `name` as `DUMP_FILE` in its root, readable and writable by its owner alone, and the
//! caller lands the volume where it wants the file, under a grant (§4.15, R10: the disk is written only by a granted
//! landing), or reads it through a mount.
//!
//! **Sliced, never held whole** (as A-459's archive). A task of the owner shard walks the range twice, a revision a unit
//! within the shard's slice (`crate::image::sliced`): the first walk measures each revision's records and drops them, so
//! the volume is admitted exactly (the dump's length rounded up to whole chunks, at least one, since a write is charged
//! its chunk window, §4.5); the second walks the range again and writes each revision's records into the file, a copy
//! slice at a time (`DaemonConfig::copy_slice_bytes`), so the export holds at most one revision's records at once. Both
//! walks hash what they made (BLAKE3): a second walk that made other bytes than the first measured (the revisions changed
//! under it, as an import setting revision 0's properties would) is refused `Unpublished`, counted ([`WALKS_DIFFERED`]),
//! and nothing is published; ask again. The volume's record, its publication and the request's completion are one atom
//! (`crate::image::complete`); a refusal discards the volume with its credits.

use slates_db::catalog::{
  BaseRecord, PolicyRecord, Principal, RepositoryId, Role, SnapshotId as DbSnapshotId, VcsTool,
  VolumeId as DbVolumeId, VolumeRecord, VolumeState,
};
use slates_ipc::protocol::{DUMP_FILE, NamePolicy, Refusal, ReplyBody, SizeClass};
use slates_svn::dump::Format;
use slates_svn::migration::{Exporter, Exporting, MigrationRefusal};
use slates_vfs::clock::Clock;
use slates_vfs::ids::InodeNo;
use slates_wire::request::RequestId;

use crate::error::refusal_of_vfs;
use crate::image::{NameAlias, publish_new_volume, sliced, with};
use crate::state::ShardState;
use crate::verbs::{Admitted, fresh_volume_id, refused, wire_id};

#[cfg(test)]
mod tests;

/// Format: the dump file's permissions: its owner's read and write alone, since it holds the range's whole history, as
/// an archive file's are (`crate::image`'s `ARCHIVE_MODE`).
const DUMP_MODE: u32 = 0o600;

/// Counter: an export whose second walk made other bytes than its first measured: the revisions changed under it,
/// refused and nothing published. Zero is the healthy reading. Format: a counter name in the status report.
pub(crate) const WALKS_DIFFERED: &str = "repository.svn_export.walks_differed";

/// Counter: a piece of the dump written into its file; with the exports, how many pieces they were written in.
/// Format: a counter name in the status report.
pub(crate) const PIECES_WRITTEN: &str = "repository.svn_export.pieces_written";

/// `ExportDump` (the module doc): the reply now when the export cannot start; otherwise the export runs in a task of
/// this shard and the reply waits for it (`crate::image::defer`), so a retry joins it.
pub(crate) fn export_dump(
  state: &mut ShardState,
  principal: &Principal,
  (name, range, deltas): (&str, (u64, u64), bool),
  volume_name: String,
) -> ReplyBody {
  if !state.consensus_ready {
    return refused(Refusal::ConsensusNotInitialized);
  }
  let record = match super::owned_repository(state, principal, name, "export_dump") {
    Ok(record) => record,
    Err(reply) => return *reply,
  };
  if record.tool != VcsTool::Subversion {
    return refused(Refusal::Unsupported {
      feature: "a dump of a repository of another tool than Subversion".to_owned(),
    });
  }
  let format = if deltas { Format::Three } else { Format::Two };
  if let Err(refusal) = exporter(state, record.id, range, format) {
    return refused(refusal);
  }
  if let Some(existing) = crate::verbs::name_held(state, &volume_name) {
    return refused(Refusal::AlreadyExists { existing });
  }
  let Some(request) = state.current_request else {
    return refused(Refusal::Unsupported {
      feature: "a dump export outside a recorded request".to_owned(),
    });
  };
  let (alias, volume) = match crate::image::name_alias(state, &volume_name) {
    Ok(minted) => minted,
    Err(refusal) => return refused(refusal),
  };
  let job = ExportJob::new(
    (request, principal.clone()),
    (record.id, range, format),
    (volume_name, volume),
  );
  crate::image::defer(state, run_export(job, alias))
}

/// The export of `range` of repository `id`'s records in `format`, refused for records not held here and for a range
/// they do not hold, in `svn_repos_dump_fs4`'s words.
fn exporter(
  state: &ShardState,
  id: RepositoryId,
  (first, last): (u64, u64),
  format: Format,
) -> Result<Exporter, Refusal> {
  let records = state
    .vcs_stores
    .subversion_records(id)
    .ok_or(Refusal::ContentUnavailable)?;
  // Each whole text a piece within the shard's copy slice at the measured digest rate (the writer checks a text's
  // checksums as it writes it), so no unit of the export's walks runs past the slice for a large text.
  let piece = usize::try_from(state.config.svn_text_piece_bytes())
    .ok()
    .and_then(std::num::NonZeroUsize::new)
    .unwrap_or(std::num::NonZeroUsize::MIN);
  Exporter::new(records, first, last, format)
    .map(|exporter| exporter.with_text_piece(piece))
    .map_err(|refusal| of_export(&refusal))
}

/// The wire's refusal for the exporter's: a range the repository does not hold in `svn_repos_dump_fs4`'s words (module
/// doc), and records that do not hold together `ContentUnavailable`.
fn of_export(refusal: &MigrationRefusal) -> Refusal {
  match *refusal {
    MigrationRefusal::BadRange { first, last, .. } if first > last => Refusal::BadRequest {
      reason: format!("Start revision {first} is greater than end revision {last}"),
    },
    MigrationRefusal::BadRange { last, youngest, .. } => Refusal::BadRequest {
      reason: format!("End revision {last} is invalid (youngest revision is {youngest})"),
    },
    _ => Refusal::ContentUnavailable,
  }
}

/// The refusal of a step whose job lost a part it made, which no correct use meets.
fn unreachable() -> Refusal {
  Refusal::Unsupported {
    feature: "a dump export step missing what its earlier step made".to_owned(),
  }
}

/// A dump export between its slices (the module doc): what it holds, given back by [`ExportJob::release`] on a refusal
/// or by its drop.
pub(crate) struct ExportJob {
  /// The request it answers.
  request: (u64, RequestId),
  /// Who asked, the repository's owner and the new volume's.
  principal: Principal,
  /// The repository.
  repository: RepositoryId,
  /// The range's first and last revisions.
  range: (u64, u64),
  /// The dump's format.
  format: Format,
  /// The new volume's name.
  name: String,
  /// The walk under way: the first, measuring, then the second, writing.
  exporter: Option<Exporter>,
  /// What the walk under way made, hashed as it goes.
  hasher: blake3::Hasher,
  /// The bytes the walk under way made.
  length: u64,
  /// The first walk's length and hash, once it ended.
  measured: Option<(u64, [u8; 32])>,
  /// The records of the revision the second walk made last.
  stage: Vec<u8>,
  /// How much of the stage the file holds.
  staged_written: usize,
  /// The bytes the file holds.
  written: u64,
  /// The bytes one write takes at most: the copy slice.
  piece: usize,
  /// Whether the second walk ended.
  walked: bool,
  /// The volume, admitted and not yet recorded.
  admitted: Option<Admitted>,
  /// The dump file in it.
  file: InodeNo,
  /// The volume's id: its name's alias's, or minted when it is admitted.
  volume: Option<DbVolumeId>,
}

impl Drop for ExportJob {
  fn drop(&mut self) {
    if self.admitted.is_some() && crate::state::with_state(|s| self.release(s)).is_none() {
      // A task that ended early with the state unreachable (a shutdown): nothing can be given back, and the process's
      // end returns it.
      self.admitted = None;
    }
  }
}

impl ExportJob {
  /// An export of `range` of `repository` in `format` into a new volume `name` (of id `volume` where its name's alias
  /// minted one), answering `request` of `principal`; nothing walked yet.
  pub(crate) fn new(
    (request, principal): ((u64, RequestId), Principal),
    (repository, range, format): (RepositoryId, (u64, u64), Format),
    (name, volume): (String, Option<DbVolumeId>),
  ) -> ExportJob {
    ExportJob {
      request,
      principal,
      repository,
      range,
      format,
      name,
      exporter: None,
      hasher: blake3::Hasher::new(),
      length: 0,
      measured: None,
      stage: Vec::new(),
      staged_written: 0,
      written: 0,
      piece: 1,
      walked: false,
      admitted: None,
      file: InodeNo(0),
      volume,
    }
  }

  /// One slice of the first walk, each revision's records measured, hashed and dropped; `true` once the range is
  /// measured.
  pub(crate) fn measure_step(&mut self, state: &mut ShardState) -> Result<bool, Refusal> {
    if self.exporter.is_none() && self.measured.is_none() {
      self.exporter = Some(exporter(state, self.repository, self.range, self.format)?);
    }
    let repository = self.repository;
    let mut scratch = Vec::new();
    let ExportJob {
      exporter: walk,
      hasher,
      length,
      ..
    } = self;
    let walk = walk.as_mut().ok_or_else(unreachable)?;
    let done = sliced(state, |s| {
      let records = s
        .vcs_stores
        .subversion_records(repository)
        .ok_or(Refusal::ContentUnavailable)?;
      match walk
        .export(records, &mut scratch)
        .map_err(|refusal| of_export(&refusal))?
      {
        Exporting::Wrote(_) => {
          hasher.update(&scratch);
          *length = length.saturating_add(u64::try_from(scratch.len()).unwrap_or(u64::MAX));
          scratch.clear();
          Ok(None)
        }
        Exporting::Done => Ok(Some(())),
      }
    })?;
    if done.is_none() {
      return Ok(false);
    }
    self.measured = Some((self.length, *self.hasher.finalize().as_bytes()));
    self.exporter = None;
    Ok(true)
  }

  /// Admits the volume sized to the first walk (its length rounded up to whole chunks, at least one), makes the dump
  /// file empty and owned as the root is, and starts the second walk.
  pub(crate) fn prepare(&mut self, state: &mut ShardState) -> Result<(), Refusal> {
    let (length, _) = self.measured.ok_or_else(unreachable)?;
    let chunk = u64::try_from(state.store.content.chunk_bytes().max(1)).unwrap_or(u64::MAX);
    let size = SizeClass::Bounded {
      limit: length.div_ceil(chunk).max(1).saturating_mul(chunk),
    };
    let admitted =
      crate::verbs::admit(state, &self.principal, size, NamePolicy::Exact, false, None)
        .map_err(|reply| crate::verbs::reply_refusal(*reply))?;
    // Held from here, so a refusal below discards it.
    let volume = &mut self.admitted.insert(admitted).volume;
    let store = &mut state.store;
    let vfs = |error: slates_vfs::VfsError| refusal_of_vfs(&error);
    let root = volume.root_inode(store).map_err(vfs)?;
    let file = volume
      .create_file_no(store, root, DUMP_FILE, DUMP_MODE)
      .map_err(vfs)?;
    let owner = volume.stat(store, root).map_err(vfs)?;
    volume
      .chown(store, file, owner.uid, owner.gid)
      .map_err(vfs)?;
    self.file = file;
    self.piece = usize::try_from(state.config.copy_slice_bytes())
      .unwrap_or(usize::MAX)
      .max(1);
    self.exporter = Some(exporter(state, self.repository, self.range, self.format)?);
    self.hasher = blake3::Hasher::new();
    self.length = 0;
    Ok(())
  }

  /// One slice of the second walk, a unit at a time: the stage written into the file a piece at a time, else the next
  /// revision's records staged; `true` once the range is written and its bytes are the first walk's.
  pub(crate) fn write_step(&mut self, state: &mut ShardState) -> Result<bool, Refusal> {
    let (repository, file, piece) = (self.repository, self.file, self.piece);
    let ExportJob {
      exporter: walk,
      hasher,
      length,
      stage,
      staged_written,
      written,
      walked,
      admitted,
      ..
    } = self;
    let (Some(walk), Some(admitted)) = (walk.as_mut(), admitted.as_mut()) else {
      return Err(unreachable());
    };
    let done = sliced(state, |s| {
      if *staged_written < stage.len() {
        let end = stage.len().min(staged_written.saturating_add(piece));
        let bytes = stage.get(*staged_written..end).ok_or_else(unreachable)?;
        let wrote = admitted
          .volume
          .write(&mut s.store, file, *written, bytes)
          .map_err(|error| refusal_of_vfs(&error))?;
        if wrote == 0 {
          // A write that lands nothing would loop for good: the volume's own refusal, which a short write stands for.
          return Err(refusal_of_vfs(&slates_vfs::VfsError::NoSpace));
        }
        *staged_written = staged_written.saturating_add(wrote);
        *written = written.saturating_add(u64::try_from(wrote).unwrap_or(u64::MAX));
        s.count(PIECES_WRITTEN, 1);
        return Ok(None);
      }
      stage.clear();
      *staged_written = 0;
      let records = s
        .vcs_stores
        .subversion_records(repository)
        .ok_or(Refusal::ContentUnavailable)?;
      match walk
        .export(records, stage)
        .map_err(|refusal| of_export(&refusal))?
      {
        Exporting::Wrote(_) => {
          hasher.update(stage);
          *length = length.saturating_add(u64::try_from(stage.len()).unwrap_or(u64::MAX));
          Ok(None)
        }
        Exporting::Done => {
          *walked = true;
          Ok(Some(()))
        }
      }
    })?;
    if done.is_none() {
      return Ok(false);
    }
    self.same_as_measured(state)?;
    Ok(true)
  }

  /// Refuses a second walk that made other bytes than the first measured, counted (module doc).
  fn same_as_measured(&self, state: &mut ShardState) -> Result<(), Refusal> {
    let walked = (self.length, *self.hasher.finalize().as_bytes());
    if self.measured == Some(walked) && self.written == self.length {
      return Ok(());
    }
    state.count(WALKS_DIFFERED, 1);
    Err(Refusal::Unpublished {
      reason: "the repository's revisions changed while they were exported: ask again".to_owned(),
    })
  }

  /// Records the volume, a plain volume of its own, moves it into the registry and publishes it; the reply.
  pub(crate) fn publish(&mut self, state: &mut ShardState) -> Result<ReplyBody, Refusal> {
    // Minted here, in the atom that records it, unless its name's alias named it: an id minted at `prepare` was the
    // next unrecorded one for every export prepared before the first published, so a second export side by side took
    // the first's id and was refused `AlreadyExists` at its record (measured: the whole history's export of the test
    // below, beside a part's).
    let id = match self.volume {
      Some(id) => id,
      None => fresh_volume_id(state).ok_or_else(crate::verbs::ids_exhausted)?,
    };
    let admitted = self.admitted.take().ok_or_else(unreachable)?;
    let limit = admitted.volume.capacity_bytes();
    let record = VolumeRecord {
      id,
      name: self.name.clone(),
      owner_shard: state.partition,
      policy: PolicyRecord {
        size: crate::verbs::db_size(SizeClass::Bounded { limit }),
        names: slates_db::catalog::NamePolicy::Exact,
        require_locked: false,
        role: Role::Plain,
        live_shipped: false,
        live_holders: None,
        write: None,
        write_version: 0,
        signers: Vec::new(),
      },
      base: BaseRecord::Scratch,
      head: DbSnapshotId::default(),
      epoch: 0,
      referenced_bytes: 0,
      unique_bytes: 0,
      state: VolumeState::Live,
      lease: None,
      owner: self.principal.clone(),
      access: Vec::new(),
      created_ns: state.clock.monotonic_ns(),
      catalog_version: 0,
      taken_over: None,
    };
    let Admitted {
      volume,
      host,
      credits,
    } = admitted;
    let reply = crate::verbs::publish_created_volume(
      state,
      id,
      volume,
      host,
      (
        credits.reservation,
        credits.version,
        credits.metadata,
        credits.lock,
      ),
      record,
    );
    if let ReplyBody::Refused { refusal } = reply {
      return Err(refusal);
    }
    publish_new_volume(state, id)?;
    Ok(ReplyBody::DumpExported {
      id: wire_id(id),
      dump_bytes: self.length,
    })
  }

  /// Gives back what the export holds: the volume, discarded with its credits.
  pub(crate) fn release(&mut self, state: &mut ShardState) {
    if let Some(admitted) = self.admitted.take() {
      crate::verbs::discard_admitted(state, admitted);
    }
  }
}

/// The export's task (module doc): the name's alias reserved where another partition holds the name, the two walks,
/// then the volume recorded and published with the request's completion as one atom, or given back on a refusal; the
/// alias settled and the reply delivered.
async fn run_export(mut job: ExportJob, alias: Option<NameAlias>) {
  let reserved = crate::image::reserve(alias.as_ref()).await;
  let outcome = match &reserved {
    Ok(()) => export_steps(&mut job).await,
    Err(refusal) => Err(refusal.clone()),
  };
  let request = job.request;
  let completed = crate::state::with_state(|s| {
    crate::image::complete(s, request, |s| {
      match outcome.and_then(|()| job.publish(s)) {
        Ok(reply) => reply,
        Err(refusal) => {
          job.release(s);
          refused(refusal)
        }
      }
    })
  });
  let Some((reply, route)) = completed else {
    // The shard's state was unreachable (its shutdown): no record committed, so the client's retry runs the verb again,
    // and the job's drop gives back what it made.
    return;
  };
  let made = matches!(reply, ReplyBody::DumpExported { .. });
  crate::image::settle(alias, reserved, made).await;
  // The completion is recorded: a reply the state cannot deliver now is counted lost here, and the client's retry
  // reads the recorded one.
  crate::state::with_state_counted(|s| crate::image::deliver(s, reply, route));
}

/// The export's slices: the first walk measuring the range, the volume admitted exactly, then the second walk writing
/// it, each slice a step of the shard and a yield between them.
async fn export_steps(job: &mut ExportJob) -> Result<(), Refusal> {
  while !with(|s| job.measure_step(s))? {
    slates_rt::futures::yield_now().await;
  }
  with(|s| job.prepare(s))?;
  while !with(|s| job.write_step(s))? {
    slates_rt::futures::yield_now().await;
  }
  Ok(())
}
