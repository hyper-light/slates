//! A work mounted like any volume (§4.16 "Role", GAP-A9-14, A-774): a work's files served through the same bridges a
//! plain volume's are (NFS, FUSE, WinFsp), written by ordinary tools, with what they wrote declared into the work at
//! its submit, so a mounted work's journal is its changeset and the verdict is the one the declared path gets.
//!
//! **The slot.** A work has no store slot of its own (`crate::work_files`: a delta over its green's version, changed
//! by `edit` and `declare`). Its first mount makes one ([`ensure_slot`]): a dynamic plain volume, registered under the
//! work's own id so every bridge finds it as it finds any volume, holding the work's files (its green's at its base
//! version with the work's own changes over them), then snapshotted. That snapshot is the mount's base: what the mount
//! changed is its op log since it ([`slates_vfs::derive`]), the declaration path §4.16 names.
//!
//! **The declaration.** A submit of a mounted work first declares what the mount changed since its base
//! ([`declare_mount`]), through the work's own `edit` and `declare` verbs, so each change is recorded in the work's log
//! and charged as a declared one is; then the mount's base moves to a snapshot of its head, and the submit runs as
//! for any work. The deriver names the touched paths; each changed file is declared as the one splice that turns the
//! work's bytes into the mount's ([`changed_span`]: the common prefix and suffix kept), so an editor's save (the whole
//! file written to a temporary and renamed over the original) is declared as the span the editor changed, never as a
//! whole-file replacement. A finer minimiser (several spans a file, D-O17) is w-rewrite-ops'; it replaces
//! [`changed_span`] at its one call site.
//!
//! **What is owed (GAPS, A-774).** The seed copies the work's files into the slot, so a mounted work costs its
//! green's bytes, not its divergence: w-work-cow's copy-on-write base replaces [`seed`]. The slot is not recorded:
//! a restart rebuilds it from the work, and a mount's writes not yet submitted are lost with it. A mount is not
//! refreshed when its work's base moves past what it shows (an accepted submit moves the base to the green's head,
//! and the mount still shows only its own changes over its old base): a later submit through the same mount of a
//! file another work changed meanwhile is declared against the work's bytes and conflicts as such. Symlinks, modes
//! and special files of the green are not seeded; a special file made through the mount is refused at the submit.
//! A green on another shard than its work's mounts nothing yet (`crate::work_home`'s fetch is per verb).

use slates_db::catalog::{Role, VolumeId as DbVolumeId};
use slates_ipc::protocol::{NamePolicy, Refusal, ReplyBody, SizeClass, VolumeId, WorkOp};
use slates_vfs::dir::Child;
use slates_vfs::error::VfsError;
use slates_vfs::ids::SnapshotId;
use slates_vfs::volume::{Store, Volume};

use crate::error::refusal_of_vfs;
use crate::state::{ShardState, VolumeSlot};

/// Shape: a mount's quota: none of its own. A dynamic volume reserves nothing and is charged to the shard's budget
/// for each byte it grows by (`crate::verbs::admit`), as the work it mounts is charged for what it holds; that budget
/// is the mount's bound, as it is the work's, so a quota of its own would only refuse a write the work could make.
const MOUNT_QUOTA_NONE: u64 = u64::MAX;
/// Format: the permission bits a seeded directory takes (the green records none for a directory).
const SEEDED_DIR_MODE: u32 = 0o755;
/// Format: the permission bits a seeded file takes when the green records none.
const SEEDED_FILE_MODE: u32 = 0o644;

/// The slot of mounted work `id`, made on its first mount ([`ensure_slot`]'s doc in the module's): `true` when the
/// work has one now, `false` when `id` is no work of this shard's or its slot could not be made. A plain volume's
/// slot answers `true`.
pub(crate) fn ensure_slot(state: &mut ShardState, id: DbVolumeId) -> bool {
  if state.by_id.contains_key(&id) {
    return true;
  }
  let Some(record) = state.db.partition().volume(id).cloned() else {
    return false;
  };
  if !matches!(record.policy.role, Role::Work { .. }) || !state.works.contains_key(&id) {
    return false;
  }
  let Ok(admitted) = crate::verbs::admit(
    state,
    &record.owner,
    SizeClass::Dynamic {
      max: MOUNT_QUOTA_NONE,
    },
    NamePolicy::Exact,
    false,
    None,
  ) else {
    state.count(MOUNT_SLOT_REFUSED, 1);
    return false;
  };
  let crate::verbs::Admitted {
    mut volume,
    host,
    credits,
  } = admitted;
  let seeded = seed(state, id, &mut volume).and_then(|()| volume.snapshot(&mut state.store));
  let base = match seeded {
    Ok(base) => base,
    Err(_) => {
      crate::verbs::discard_admitted(
        state,
        crate::verbs::Admitted {
          volume,
          host,
          credits,
        },
      );
      state.count(MOUNT_SLOT_REFUSED, 1);
      return false;
    }
  };
  if install(state, (id, record.name), volume, host, credits).is_err() {
    state.count(MOUNT_SLOT_REFUSED, 1);
    return false;
  }
  state.merge.mounted.insert(id, base);
  true
}

/// Format: the status counter of a work's mount slot that could not be made (no budget, no room in the registry, a
/// seed the volume refused); the mount then finds no volume, as one of a volume it may not see.
pub(crate) const MOUNT_SLOT_REFUSED: &str = "merge.mount_slot_refused";

/// Writes work `id`'s files into `volume`: its green's at its base version with the work's own changes over them.
fn seed(state: &mut ShardState, id: DbVolumeId, volume: &mut Volume) -> Result<(), VfsError> {
  let (works, greens, home, store) = (
    &state.works,
    &state.greens,
    &state.work_home,
    &mut state.store,
  );
  let work = works.get(&id).ok_or(VfsError::StaleHandle)?;
  let lent = greens.get(&work.green);
  let base = crate::work_files::Base {
    green: lent.as_deref(),
    fetched: home.fetched.as_ref(),
    version: work.base_version,
  };
  for (path, bytes) in work.files.files(base) {
    seed_file(volume, store, &path, &bytes)?;
  }
  Ok(())
}

/// Creates `path` in `volume` with `bytes`, making its missing parent directories.
fn seed_file(
  volume: &mut Volume,
  store: &mut Store,
  path: &str,
  bytes: &[u8],
) -> Result<(), VfsError> {
  let mut parts = path.split('/').filter(|part| !part.is_empty()).peekable();
  let mut dir = directory(volume, store, "")?;
  let mut walked = String::new();
  while let Some(part) = parts.next() {
    if parts.peek().is_none() {
      let file = volume.create_file(store, dir, part, SEEDED_FILE_MODE)?;
      volume.write(store, file, 0, bytes)?;
      return Ok(());
    }
    if !walked.is_empty() {
      walked.push('/');
    }
    walked.push_str(part);
    dir = match directory(volume, store, &walked) {
      Ok(found) => found,
      Err(VfsError::NotFound) => volume.mkdir(store, dir, part, SEEDED_DIR_MODE)?,
      Err(other) => return Err(other),
    };
  }
  Err(VfsError::NotFound)
}

/// The directory at `path` in `volume`'s head.
fn directory(
  volume: &Volume,
  store: &Store,
  path: &str,
) -> Result<slates_mem::Handle<slates_vfs::dir::DirNode>, VfsError> {
  match volume.resolve(store, path)?.child {
    Child::Dir(dir) => Ok(dir),
    _ => Err(VfsError::NotDirectory),
  }
}

/// The one splice that turns `old` into `new`: `(at, delete_len, insert)` with `insert` the range of `new` put at
/// `at`, keeping the longest common prefix and then the longest common suffix of what is left; `None` when they are
/// equal. Linear in the shorter of the two, and allocates nothing.
pub(crate) fn changed_span(old: &[u8], new: &[u8]) -> Option<(u64, u64, std::ops::Range<usize>)> {
  let prefix = old
    .iter()
    .zip(new.iter())
    .take_while(|(a, b)| a == b)
    .count();
  if prefix == old.len() && prefix == new.len() {
    return None;
  }
  let old_rest = old.get(prefix..).unwrap_or_default();
  let new_rest = new.get(prefix..).unwrap_or_default();
  let suffix = old_rest
    .iter()
    .rev()
    .zip(new_rest.iter().rev())
    .take_while(|(a, b)| a == b)
    .count();
  let deleted = old_rest.len().saturating_sub(suffix);
  let inserted = new_rest.len().saturating_sub(suffix);
  Some((
    u64::try_from(prefix).ok()?,
    u64::try_from(deleted).ok()?,
    prefix..prefix.saturating_add(inserted),
  ))
}

/// Declares into `work` what its mount changed since the mount's base (the module doc), then moves the mount's base to
/// its head. Nothing to do for a work with no mount. A refusal of any declaration is the submit's: what was declared
/// before it stays declared (each is recorded as its verb records it), and the mount's base stays, so a retry declares
/// the rest against the work as it now is.
pub(crate) fn declare_mount(
  state: &mut ShardState,
  principal: &slates_db::catalog::Principal,
  work: VolumeId,
) -> Result<(), Refusal> {
  let id = crate::verbs::to_db_volume(work);
  let Some(&base) = state.merge.mounted.get(&id) else {
    return Ok(());
  };
  let Some(&handle) = state.by_id.get(&id) else {
    return Ok(());
  };
  let document = {
    let slot = state.volumes.get(handle).map_err(|_| Refusal::NotFound)?;
    slot
      .volume
      .derive(&state.store, base)
      .map_err(|e| refusal_of_vfs(&e))?
  };
  if let Some(special) = document.specials.first() {
    return Err(Refusal::Unsupported {
      feature: format!(
        "a special file made through a mounted work ({}): declare it",
        special.path
      ),
    });
  }
  for (from, to) in &document.dirs_renamed {
    declared(crate::verbs::declare_op(
      state,
      principal,
      work,
      WorkOp::Rename {
        from: from.to_string(),
        to: to.to_string(),
      },
    ))?;
  }
  // A file moved to a new path is renamed before its bytes are compared, so its splice is against its own base.
  for file in &document.files {
    if let Some(moved) = file.base.as_ref().filter(|base| base.path != file.path) {
      declared(crate::verbs::declare_op(
        state,
        principal,
        work,
        WorkOp::Rename {
          from: moved.path.to_string(),
          to: file.path.to_string(),
        },
      ))?;
    }
  }
  for removed in &document.removed {
    declared(crate::verbs::declare_op(
      state,
      principal,
      work,
      WorkOp::Unlink {
        path: removed.to_string(),
      },
    ))?;
  }
  // Deepest first, so a directory is empty when it goes.
  let mut dirs_removed: Vec<&str> = document.dirs_removed.iter().map(|path| &**path).collect();
  dirs_removed.sort_by_key(|path| std::cmp::Reverse(path.matches('/').count()));
  for path in dirs_removed {
    declared(crate::verbs::declare_op(
      state,
      principal,
      work,
      WorkOp::Rmdir {
        path: path.to_owned(),
      },
    ))?;
  }
  // Shallowest first, so a directory's parent is there when it is made.
  let mut dirs_created: Vec<&str> = document.dirs_created.iter().map(|path| &**path).collect();
  dirs_created.sort_by_key(|path| path.matches('/').count());
  for path in dirs_created {
    declared(crate::verbs::declare_op(
      state,
      principal,
      work,
      WorkOp::Mkdir {
        path: path.to_owned(),
      },
    ))?;
  }
  for symlink in &document.symlinks {
    declared(crate::verbs::declare_op(
      state,
      principal,
      work,
      WorkOp::Symlink {
        path: symlink.path.to_string(),
        target: symlink.target.to_string(),
      },
    ))?;
  }
  for file in &document.files {
    declare_file(state, principal, (work, id, handle), &file.path)?;
  }
  // Everything the mount changed is the work's now: the next submit declares only what changes after this.
  let slot = state
    .volumes
    .get_mut(handle)
    .map_err(|_| Refusal::NotFound)?;
  let head = slot
    .volume
    .snapshot(&mut state.store)
    .map_err(|e| refusal_of_vfs(&e))?;
  // The old base is no longer read; a refusal to destroy it keeps it (its versions stay charged to the volume).
  let _ = slot.volume.destroy_snapshot(&mut state.store, base);
  state.merge.mounted.insert(id, head);
  Ok(())
}

/// Declares the mount's bytes of `path` into the work as one splice against the work's own bytes ([`changed_span`]).
fn declare_file(
  state: &mut ShardState,
  principal: &slates_db::catalog::Principal,
  (work, id, handle): (VolumeId, DbVolumeId, slates_mem::Handle<VolumeSlot>),
  path: &str,
) -> Result<(), Refusal> {
  let path = crate::merge_service::canonical_path(path);
  let mounted = {
    let slot = state.volumes.get(handle).map_err(|_| Refusal::NotFound)?;
    read_whole(&slot.volume, &state.store, path).map_err(|e| refusal_of_vfs(&e))?
  };
  let span = {
    let w = state.works.get(&id).ok_or(Refusal::NotFound)?;
    crate::merge_service::with_work_base(state, w, |base| {
      w.files.get(base, path).map(|old| match old {
        Some(old) => changed_span(&old, &mounted),
        None => Some((0, 0, 0..mounted.len())),
      })
    })
    .map_err(|_| crate::merge_service::unreadable_base(w))?
  };
  let Some((at, delete_len, inserted)) = span else {
    return Ok(());
  };
  declared(crate::verbs::edit_op(
    state,
    principal,
    work,
    path,
    (at, delete_len),
    mounted.get(inserted).unwrap_or_default(),
  ))
}

/// The whole of the file at `path` in `volume`'s head.
fn read_whole(volume: &Volume, store: &Store, path: &str) -> Result<Vec<u8>, VfsError> {
  let inode = volume.resolve(store, path)?.inode;
  let len = usize::try_from(volume.stat(store, inode)?.size).map_err(|_| VfsError::FileTooLarge)?;
  let mut bytes = vec![0u8; len];
  let mut filled = 0;
  while let Some(rest) = bytes.get_mut(filled..).filter(|rest| !rest.is_empty()) {
    let read = volume.read(
      store,
      inode,
      u64::try_from(filled).unwrap_or(u64::MAX),
      rest,
    )?;
    if read == 0 {
      break;
    }
    filled = filled.saturating_add(read);
  }
  bytes.truncate(filled);
  Ok(bytes)
}

/// A declaring verb's reply as a result: its refusal, or nothing.
fn declared(reply: ReplyBody) -> Result<(), Refusal> {
  match reply {
    ReplyBody::Refused { refusal } => Err(refusal),
    _ => Ok(()),
  }
}

/// The store-backed view of green `id`'s head, for a landing (§4.15 over §4.16, A-774 piece 3): a slot under the
/// green's own id holding its head's files and symlinks, so the landing engine plans, presents and lands it as it
/// does any volume's head, under the same grant (R10). Kept while the green's head stays where it was made, and made
/// again once the head moves, so a presentation and the granted landing after it read one state. A named snapshot of a
/// green is refused: a green's versions are its snapshots, and only its head lands.
pub(crate) fn ensure_green_view(
  state: &mut ShardState,
  id: DbVolumeId,
  snapshot: Option<slates_ipc::protocol::SnapshotId>,
) -> Result<(), Refusal> {
  if snapshot.is_some() {
    return Err(Refusal::Unsupported {
      feature: "landing a named version of a green: land its head".to_owned(),
    });
  }
  let record = state
    .db
    .partition()
    .volume(id)
    .cloned()
    .ok_or(Refusal::NotFound)?;
  let head = state
    .greens
    .get(&id)
    .map(|green| green.head())
    .ok_or(Refusal::NotFound)?;
  if state.merge.green_views.get(&id) == Some(&head) && state.by_id.contains_key(&id) {
    return Ok(());
  }
  release(state, id);
  let admitted = crate::verbs::admit(
    state,
    &record.owner,
    SizeClass::Dynamic {
      max: MOUNT_QUOTA_NONE,
    },
    NamePolicy::Exact,
    false,
    None,
  )
  .map_err(|reply| match *reply {
    ReplyBody::Refused { refusal } => refusal,
    _ => Refusal::NotFound,
  })?;
  let crate::verbs::Admitted {
    mut volume,
    host,
    credits,
  } = admitted;
  let seeded = {
    let (greens, store) = (&state.greens, &mut state.store);
    match greens.get(&id) {
      Some(green) => seed_green(&green, store, &mut volume),
      None => Err(VfsError::StaleHandle),
    }
  };
  if let Err(e) = seeded {
    crate::verbs::discard_admitted(
      state,
      crate::verbs::Admitted {
        volume,
        host,
        credits,
      },
    );
    return Err(refusal_of_vfs(&e));
  }
  install(state, (id, record.name), volume, host, credits)?;
  state.merge.green_views.insert(id, head);
  Ok(())
}

/// Writes `green`'s head into `volume`: its files, then its symlinks.
fn seed_green(
  green: &slates_merge::engine::Green,
  store: &mut Store,
  volume: &mut Volume,
) -> Result<(), VfsError> {
  for (path, bytes) in green.files() {
    seed_file(volume, store, &path, bytes)?;
  }
  for (path, target) in green.symlinks() {
    let (parent, name) = match path.rsplit_once('/') {
      Some((parent, name)) => (parent, name),
      None => ("", path.as_str()),
    };
    let dir = directory(volume, store, parent)?;
    volume.symlink(store, dir, name, target)?;
  }
  Ok(())
}

/// Registers `volume` as the slot of merge volume `id` named `name`, or discards it with its credits.
fn install(
  state: &mut ShardState,
  (id, name): (DbVolumeId, String),
  mut volume: Volume,
  host: Option<slates_base::BaseHost>,
  credits: crate::verbs::Credits,
) -> Result<(), Refusal> {
  if !state.volumes.has_room() {
    let capacity = state.volumes.max_slots();
    crate::verbs::discard_admitted(
      state,
      crate::verbs::Admitted {
        volume,
        host,
        credits,
      },
    );
    return Err(crate::error::refusal_of_mem(
      &slates_mem::MemError::SlabFull { capacity },
    ));
  }
  crate::content_cipher::key_volume(&mut state.store, &mut volume, id.bytes);
  let slot = VolumeSlot {
    id,
    name,
    volume,
    host,
    reservation: credits.reservation,
    version_credit: credits.version,
    metadata_credit: credits.metadata,
    lock_credit: credits.lock,
    adopted_seal: None,
  };
  let handle = state
    .volumes
    .insert(slot)
    .map_err(|e| crate::error::refusal_of_mem(&e))?;
  state.by_id.insert(id, handle);
  Ok(())
}

/// Gives back merge volume `id`'s slot, if it has one (a mounted work's, a green's landing view): at its destroy, and
/// before a green's view is made again.
pub(crate) fn release(state: &mut ShardState, id: DbVolumeId) {
  state.merge.mounted.remove(&id);
  state.merge.green_views.remove(&id);
  let Some(handle) = state.by_id.remove(&id) else {
    return;
  };
  if let Ok(slot) = state.volumes.remove(handle) {
    crate::verbs::release_slot_credits(state, &slot);
    // What the discard could not give back stays counted where every discarded volume's is.
    let _discarded = slot.volume.discard_partial(&mut state.store);
  }
}

/// The head version each green's landing view holds, by green ([`ensure_green_view`]).
pub(crate) type GreenViews = std::collections::BTreeMap<DbVolumeId, u64>;

/// The base snapshot of a mounted work, by work: what its mount changed is its op log since it.
pub(crate) type MountedBases = std::collections::BTreeMap<DbVolumeId, SnapshotId>;

#[cfg(test)]
mod tests {
  use super::changed_span;

  /// A-774. Do: compare an editor's save with the file it replaced. Expect: the splice spans exactly the changed
  /// bytes, the common prefix and suffix kept; equal files give none; a pure insertion or deletion deletes or inserts
  /// nothing on the other side; and applying the splice to the old bytes gives the new.
  #[test]
  fn a_save_is_declared_as_the_span_it_changed() {
    let cases: [(&[u8], &[u8]); 6] = [
      (b"a\nb\nc\n", b"a\nB\nc\n"),
      (b"same", b"same"),
      (b"abc", b"abXc"),
      (b"abXc", b"abc"),
      (b"", b"new"),
      (b"aaaa", b"aa"),
    ];
    for (old, new) in cases {
      match changed_span(old, new) {
        None => assert_eq!(old, new),
        Some((at, deleted, inserted)) => {
          let at = usize::try_from(at).unwrap();
          let deleted = usize::try_from(deleted).unwrap();
          let mut spliced = old[..at].to_vec();
          spliced.extend_from_slice(&new[inserted.clone()]);
          spliced.extend_from_slice(&old[at + deleted..]);
          assert_eq!(spliced, new, "{old:?} -> {new:?}");
          assert!(deleted + inserted.len() <= old.len().max(new.len()));
        }
      }
    }
    assert_eq!(changed_span(b"a\nb\nc\n", b"a\nB\nc\n"), Some((2, 1, 2..3)));
    assert_eq!(changed_span(b"abc", b"abXc"), Some((2, 0, 2..3)));
  }
}
