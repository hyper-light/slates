//! A work mounted like any volume (§4.16 "Role", GAP-A9-14, A-774): a work's files served through the same bridges a
//! plain volume's are, written by ordinary tools, with what they wrote declared into the work at its submit, so a
//! mounted work's journal is its changeset and the verdict is the one the declared path gets.
//!
//! **The slot, over the green where it lies (part 2).** A work has no store slot of its own (`crate::work_files`: a
//! delta over its green's version). Its first mount makes one ([`ensure_slot`]): an overlay volume, registered under the
//! work's own id so every bridge finds it as it finds any volume, whose base is the work's green at the work's base
//! version, pinned for the mount's life ([`MountPin`], counted in `merge_service::reachable_floor`) and read where it
//! lies: the slot's host is a `slates_vfs::host::tree::TreeHost`, lent with a [`crate::green_tree::GreenTree`] for each
//! request ([`lend`]). So making a mount walks nothing and copies nothing, a read is served from the green's own bytes,
//! and the overlay holds only what its user changed (the base plane's copy-up, §4.5). Before 2026-11-20 part 2 the slot
//! was a scratch volume seeded with a copy of every file: 8 mounts over a 64 MiB green added 548 MB resident, and a
//! mount took 34 ms against 1.6 ms over a 16-file green (`tests/mounted_work_cow.rs`, debug, macOS arm64).
//!
//! **The declaration.** A submit of a mounted work first declares what the mount changed ([`declare_mount`]): the
//! overlay's diverged set (created, copied-up, whiteouted entries, §4.5 AC-1.10), each declared through the work's own
//! `edit` and `declare` verbs, so it is recorded in the work's log and charged as a declared change is. A file is
//! declared as the one splice that turns the work's bytes into the mount's ([`changed_span`]), so an editor's save
//! (written whole to a temporary and renamed over the original) is the span it changed. The declaration compares, so
//! it is idempotent: a second submit declares only what differs from the work by then. A finer minimiser (several
//! spans a file, D-O17) replaces [`changed_span`] at its one call site.
//!
//! **What is owed (GAPS, A-774).** The slot is not recorded: a restart rebuilds it from the work, and a mount's writes
//! not yet submitted are lost with it. A mount is not refreshed when its work's base moves past its pin: it shows the
//! pinned version under its own changes, and a later submit of a file it changed declares its bytes against the work's,
//! undoing another work's change to that file. A work holding declared changes when first mounted is refused. A
//! directory renamed through the mount is refused at the submit, as is a special file. Only the NFS and FUSE bridges
//! lend the tree; the others refuse a mounted work's base reads typed. A green on another shard than its work's mounts
//! nothing yet.

use slates_db::catalog::{Role, VolumeId as DbVolumeId};
use slates_ipc::protocol::{NamePolicy, Refusal, ReplyBody, SizeClass, VolumeId, WorkOp};
use slates_vfs::base::{Diverged, Divergence};
use slates_vfs::dir::Child;
use slates_vfs::error::VfsError;
use slates_vfs::host::HostFs;
use slates_vfs::volume::Volume;

use crate::error::refusal_of_vfs;
use crate::state::{ShardState, VolumeSlot};

/// Shape: a mount's quota: none of its own. A dynamic volume reserves nothing and is charged to the shard's budget
/// for each byte it grows by (`crate::verbs::admit`), as the work it mounts is charged for what it holds; that budget
/// is the mount's bound, as it is the work's, so a quota of its own would only refuse a write the work could make.
const MOUNT_QUOTA_NONE: u64 = u64::MAX;

/// A mounted work's base: its green and the version the mount reads, kept readable while the mount lives.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct MountPin {
  /// The green.
  pub(crate) green: DbVolumeId,
  /// The version the mount's base shows.
  pub(crate) version: u64,
}

/// Each mounted work's pin, by work: one entry a mounted work of this shard, dropped with its slot.
pub(crate) type MountedPins = std::collections::BTreeMap<DbVolumeId, MountPin>;

/// The tree identity of `pin`: its fingerprints' device, telling one mount's base from another's.
fn identity(pin: MountPin) -> u64 {
  let mut word = [0u8; 8];
  for (at, byte) in pin.green.bytes.iter().enumerate() {
    if let Some(slot) = word.get_mut(at % 8) {
      *slot ^= *byte;
    }
  }
  u64::from_le_bytes(word) ^ pin.version.rotate_left(32)
}

/// The slot of mounted work `id`, made on its first mount (the module doc): `true` when the work has one now, `false`
/// when `id` is no work of this shard's or its slot could not be made (counted). A plain volume's slot answers `true`.
pub(crate) fn ensure_slot(state: &mut ShardState, id: DbVolumeId) -> bool {
  if state.by_id.contains_key(&id) {
    return true;
  }
  let Some(record) = state.db.partition().volume(id).cloned() else {
    return false;
  };
  if !matches!(record.policy.role, Role::Work { .. }) {
    return false;
  }
  let Some(work) = state.works.get(&id) else {
    return false;
  };
  // A work with declared changes of its own would show its green's version without them (owed: write them into the
  // overlay); one whose green is elsewhere has no green here to read.
  if work.files.changed_entries().next().is_some() || state.greens.get(&work.green).is_none() {
    state.count(MOUNT_SLOT_REFUSED, 1);
    return false;
  }
  let pin = MountPin {
    green: work.green,
    version: work.base_version,
  };
  let Ok(admitted) = crate::verbs::admit_over(
    state,
    &record.owner,
    (
      SizeClass::Dynamic {
        max: MOUNT_QUOTA_NONE,
      },
      NamePolicy::Exact,
      false,
    ),
    crate::verbs::AdmitBase::Tree {
      identity: identity(pin),
    },
  ) else {
    state.count(MOUNT_SLOT_REFUSED, 1);
    return false;
  };
  let crate::verbs::Admitted {
    volume,
    host,
    credits,
  } = admitted;
  if install(state, (id, record.name), volume, host, credits).is_err() {
    state.count(MOUNT_SLOT_REFUSED, 1);
    return false;
  }
  state.merge.mounted.insert(id, pin);
  true
}

/// Format: the status counter of a work's mount slot that could not be made (no budget, no room in the registry, a
/// work holding declared changes, a green elsewhere); the mount then finds no volume, as one of a volume it may not see.
pub(crate) const MOUNT_SLOT_REFUSED: &str = "merge.mount_slot_refused";

/// Lends `host` for one request on `volume`'s slot: a mounted work's tree host lent with its green at its pin, any other
/// host as it is. `tree` is the caller's place for the lend, `source` the green's tree it reads (from [`source`]).
pub(crate) fn lend<'a>(
  host: &'a mut slates_base::BaseHost,
  source: Option<&'a crate::green_tree::GreenTree<'a>>,
  tree: &'a mut Option<slates_vfs::host::tree::TreeLens<'a>>,
) -> &'a mut dyn HostFs {
  match (host, source) {
    (slates_base::BaseHost::Tree(host), Some(source)) => {
      tree.insert(host.lend(source)) as &mut dyn HostFs
    }
    (host, _) => host,
  }
}

/// The green tree mounted work `volume` reads, if it is one: its green, lent from `greens`, at its pin.
pub(crate) fn source<'a>(
  greens: &'a crate::greens::Greens,
  mounted: &MountedPins,
  volume: DbVolumeId,
) -> Option<(crate::greens::GreenRef<'a>, u64)> {
  let pin = mounted.get(&volume)?;
  Some((greens.get(&pin.green)?, pin.version))
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

/// Declares into `work` what its mount changed (the module doc). Nothing to do for a work with no mount. A refusal of
/// any declaration is the submit's: what was declared before it stays declared (each is recorded as its verb records
/// it), and a retry compares again and declares the rest.
pub(crate) fn declare_mount(
  state: &mut ShardState,
  principal: &slates_db::catalog::Principal,
  work: VolumeId,
) -> Result<(), Refusal> {
  let id = crate::verbs::to_db_volume(work);
  let Some(&handle) = state.by_id.get(&id) else {
    return Ok(());
  };
  if !state.merge.mounted.contains_key(&id) {
    return Ok(());
  }
  let diverged = {
    let slot = state.volumes.get(handle).map_err(|_| Refusal::NotFound)?;
    slot.volume.diverged(&state.store)
  };
  let mut whiteouts: Vec<&Diverged> = diverged
    .iter()
    .filter(|entry| entry.kind == Divergence::Whiteout)
    .collect();
  // Deepest first, so a directory is empty when it goes.
  whiteouts.sort_by_key(|entry| std::cmp::Reverse(entry.path.matches('/').count()));
  for entry in whiteouts {
    declare_removal(state, principal, (work, id), &entry.path)?;
  }
  // Shallowest first, so a directory's parent is there when it is made.
  let mut made: Vec<&Diverged> = diverged
    .iter()
    .filter(|entry| entry.kind != Divergence::Whiteout)
    .collect();
  made.sort_by_key(|entry| entry.path.matches('/').count());
  for entry in made {
    if entry.kind == Divergence::Redirect {
      return Err(Refusal::Unsupported {
        feature: format!(
          "a directory renamed through a mounted work ({}): declare the rename",
          entry.path
        ),
      });
    }
    declare_entry(state, principal, (work, id, handle), &entry.path)?;
  }
  Ok(())
}

/// What the mount holds at `path`, read through its base.
enum Mounted {
  /// A directory.
  Dir,
  /// A file's bytes.
  File(Vec<u8>),
  /// A symlink's target.
  Symlink(Box<str>),
  /// A special file.
  Special,
}

/// Reads the mount's entry at `path`, through the slot's base lent with its green.
fn read_mounted(
  state: &mut ShardState,
  (id, handle): (DbVolumeId, slates_mem::Handle<VolumeSlot>),
  path: &str,
) -> Result<Mounted, VfsError> {
  let ShardState {
    store,
    volumes,
    greens,
    merge,
    ..
  } = state;
  let lent = source(greens, &merge.mounted, id);
  let tree = lent
    .as_ref()
    .map(|(green, version)| crate::green_tree::GreenTree::at(green, *version));
  let slot = volumes.get_mut(handle).map_err(|_| VfsError::StaleHandle)?;
  let mut lens = None;
  let Some(host) = slot.host.as_mut() else {
    return Err(VfsError::StaleHandle);
  };
  let host = lend(host, tree.as_ref(), &mut lens);
  let mut overlay = slot.volume.with_host(host);
  let located = overlay.resolve(store, path)?;
  match located.child {
    Child::Dir(_) => Ok(Mounted::Dir),
    Child::Symlink(no) => {
      let mut target = vec![0u8; usize::try_from(overlay.stat(store, no)?.size).unwrap_or(0)];
      let read = overlay.read(store, no, 0, &mut target)?;
      target.truncate(read);
      Ok(Mounted::Symlink(
        String::from_utf8_lossy(&target)
          .into_owned()
          .into_boxed_str(),
      ))
    }
    Child::File(no) => {
      let len =
        usize::try_from(overlay.stat(store, no)?.size).map_err(|_| VfsError::FileTooLarge)?;
      let mut bytes = vec![0u8; len];
      let mut filled = 0;
      while let Some(rest) = bytes.get_mut(filled..).filter(|rest| !rest.is_empty()) {
        let read = overlay.read(store, no, u64::try_from(filled).unwrap_or(u64::MAX), rest)?;
        if read == 0 {
          break;
        }
        filled = filled.saturating_add(read);
      }
      bytes.truncate(filled);
      Ok(Mounted::File(bytes))
    }
    _ => Ok(Mounted::Special),
  }
}

/// Declares the mount's entry at `path` into the work, where it differs from the work's.
fn declare_entry(
  state: &mut ShardState,
  principal: &slates_db::catalog::Principal,
  (work, id, handle): (VolumeId, DbVolumeId, slates_mem::Handle<VolumeSlot>),
  path: &str,
) -> Result<(), Refusal> {
  let path = crate::merge_service::canonical_path(path);
  match read_mounted(state, (id, handle), path).map_err(|e| refusal_of_vfs(&e))? {
    Mounted::Dir => {
      if work_has_dir(state, id, path) {
        return Ok(());
      }
      declared(crate::verbs::declare_op(
        state,
        principal,
        work,
        WorkOp::Mkdir {
          path: path.to_owned(),
        },
      ))
    }
    Mounted::Symlink(target) => {
      let same = state
        .works
        .get(&id)
        .and_then(|w| state.greens.get(&w.green))
        .is_some_and(|green| green.symlink(path) == Some(&*target));
      if same {
        return Ok(());
      }
      declared(crate::verbs::declare_op(
        state,
        principal,
        work,
        WorkOp::Symlink {
          path: path.to_owned(),
          target: target.into_string(),
        },
      ))
    }
    Mounted::File(mounted) => {
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
    Mounted::Special => Err(Refusal::Unsupported {
      feature: format!("a special file made through a mounted work ({path}): declare it"),
    }),
  }
}

/// Whether the work holds a directory at `path`: one its green records at the work's base.
fn work_has_dir(state: &ShardState, id: DbVolumeId, path: &str) -> bool {
  state
    .works
    .get(&id)
    .and_then(|w| state.greens.get(&w.green))
    .is_some_and(|green| green.is_dir(path) || !green.beneath(path).is_empty())
}

/// Declares the removal of `path`, a base entry the mount removed, where the work still has it.
fn declare_removal(
  state: &mut ShardState,
  principal: &slates_db::catalog::Principal,
  (work, id): (VolumeId, DbVolumeId),
  path: &str,
) -> Result<(), Refusal> {
  let path = crate::merge_service::canonical_path(path);
  let has_file = {
    let w = state.works.get(&id).ok_or(Refusal::NotFound)?;
    crate::merge_service::with_work_base(state, w, |base| {
      w.files.get(base, path).map(|file| file.is_some())
    })
    .map_err(|_| crate::merge_service::unreadable_base(w))?
  };
  let op = if has_file {
    WorkOp::Unlink {
      path: path.to_owned(),
    }
  } else if work_has_dir(state, id, path) {
    WorkOp::Rmdir {
      path: path.to_owned(),
    }
  } else {
    return Ok(());
  };
  declared(crate::verbs::declare_op(state, principal, work, op))
}

/// A declaring verb's reply as a result: its refusal, or nothing.
fn declared(reply: ReplyBody) -> Result<(), Refusal> {
  match reply {
    ReplyBody::Refused { refusal } => Err(refusal),
    _ => Ok(()),
  }
}

/// The store-backed view of green `id`'s head, for a landing (§4.15 over §4.16, A-774 parts 1 and 3): a slot under the
/// green's own id, an overlay over the green at its head read where it lies (as a mount's, [`ensure_slot`]), its tree
/// loaded (its directories listed, each entry an inode, no byte copied) so the landing's snapshot holds every entry. The
/// landing plans its whole tree and reads each file from the green a window at a time
/// (`slates_land::engine::Beginning::materializing`), under the grant any volume's landing takes (R10). Kept while the
/// green's head stays where it was made, and made again once the head moves, so a presentation and the granted landing
/// after it read one state. A named snapshot of a green is refused: only its head lands. Before part 3 the view was a
/// scratch volume holding a copy of the head: presenting a 64 MiB green's landing added 72 MB resident.
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
  let pin = MountPin {
    green: id,
    version: head,
  };
  let admitted = crate::verbs::admit_over(
    state,
    &record.owner,
    (
      SizeClass::Dynamic {
        max: MOUNT_QUOTA_NONE,
      },
      NamePolicy::Exact,
      false,
    ),
    crate::verbs::AdmitBase::Tree {
      identity: identity(pin),
    },
  )
  .map_err(|reply| match *reply {
    ReplyBody::Refused { refusal } => refusal,
    _ => Refusal::NotFound,
  })?;
  let crate::verbs::Admitted {
    volume,
    host,
    credits,
  } = admitted;
  install(state, (id, record.name), volume, host, credits)?;
  state.merge.green_views.insert(id, head);
  if let Err(e) = load_tree(state, id) {
    release(state, id);
    return Err(refusal_of_vfs(&e));
  }
  Ok(())
}

/// Lists every directory of green view `id`'s tree through its base, so each entry is an inode of the view (no byte read).
/// Proportional to the green's paths (owed: a unit at a time under the shard's slice).
fn load_tree(state: &mut ShardState, id: DbVolumeId) -> Result<(), VfsError> {
  let ShardState {
    store,
    volumes,
    by_id,
    greens,
    merge,
    ..
  } = state;
  let handle = *by_id.get(&id).ok_or(VfsError::StaleHandle)?;
  let lent = view_source(greens, &merge.green_views, id);
  let tree = lent
    .as_ref()
    .map(|(green, version)| crate::green_tree::GreenTree::at(green, *version));
  let slot = volumes.get_mut(handle).map_err(|_| VfsError::StaleHandle)?;
  let mut lens = None;
  let Some(host) = slot.host.as_mut() else {
    return Err(VfsError::StaleHandle);
  };
  let host = lend(host, tree.as_ref(), &mut lens);
  let mut overlay = slot.volume.with_host(host);
  let Child::Dir(root) = overlay.resolve(store, "")?.child else {
    return Err(VfsError::NotDirectory);
  };
  let mut stack = vec![root];
  while let Some(dir) = stack.pop() {
    let names: Vec<String> = overlay
      .readdir(store, dir)?
      .into_iter()
      .filter(|row| row.kind == slates_vfs::inode::Kind::Dir)
      .map(|row| row.name.to_owned())
      .collect();
    for name in names {
      if let Child::Dir(child) = overlay.lookup(store, dir, &name)?.child {
        stack.push(child);
      }
    }
  }
  Ok(())
}

/// The green tree green view `id` reads: the green, lent from `greens`, at the head its view was made at.
pub(crate) fn view_source<'a>(
  greens: &'a crate::greens::Greens,
  views: &GreenViews,
  id: DbVolumeId,
) -> Option<(crate::greens::GreenRef<'a>, u64)> {
  let version = views.get(&id)?;
  Some((greens.get(&id)?, *version))
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
