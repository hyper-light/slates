//! The typed channel's file verbs (§4.12 `slates.fs`; condition 13): write, remove, rename and make a directory in a
//! plain volume, for an agent that works through MCP or the SDK instead of a mount. Every OS serves them (the NFS,
//! FUSE, WinFsp and virtio-fs bridges are each one OS's; this channel is every OS's).
//!
//! **Under an attachment, through the shared bridge.** A verb names the caller's own write attachment of the volume,
//! as a mount's requests ride the mount's (§4.4): the attachment must be the caller's, of this volume, for writing,
//! and not a snapshot's. Its requests are admitted in the shard's attachment registry like a mount's (`begin`/`end`,
//! so a barrier sees them), and served by [`VolumeBridge`], the semantic layer every mount uses: the overlay rules
//! over a base, copy-on-write, the recall gate in front of an NFSv4 client's delegation (A-79), the op log a
//! mounted client's cache follows, a scoped attachment's subtree. The verb is recorded, and the shard republishes
//! before it answers (`verbs::mutates_shard_image`), the barrier a mounted write stands behind.
//!
//! **Authority is the volume's grant.** The typed channel authorizes by the attachment's rights (§4.13), as its
//! reads of a plain volume do; POSIX mode bits stay the rule between the users of a mount, which a mount's own
//! transport applies. A file the verb creates is owned by a Unix caller (a uid, or a consumer's host account).
//!
//! **What a write is.** `FsWrite` replaces a file's whole content: an absent file is created with the verb's mode in
//! an existing directory; a present one is truncated and rewritten. A refusal partway (the volume's quota) is typed
//! and leaves the file truncated, as `open(O_TRUNC)` and a failed `write` leave it.

use slates_bridge_core::volume_bridge::new_handle_store;
use slates_bridge_core::{Bridge, ObjectId, OpContext, RenameFlags, SetAttr, VolumeBridge};
use slates_db::catalog::{Principal, Role};
use slates_ipc::protocol::{Refusal, ReplyBody, VolumeId};
use slates_vfs::error::VfsError;
use slates_vfs::host::HostFs;
use slates_vfs::inode::Kind;

use crate::error::refusal_of_vfs;
use crate::state::{MountAttachment, ShardState};
use crate::verbs::{find, forbidden, refused, to_db_volume};

/// One file verb.
pub(crate) enum FsOp<'a> {
  /// Replace the file at `path` with `bytes`, creating it with `mode` when absent.
  Write {
    /// The file.
    path: &'a str,
    /// Its whole new content.
    bytes: &'a [u8],
    /// The permission bits a created file takes.
    mode: u32,
  },
  /// Remove the file, symbolic link or empty directory at `path`.
  Remove {
    /// The name removed.
    path: &'a str,
  },
  /// Rename `from` to `to`.
  Rename {
    /// The source.
    from: &'a str,
    /// The destination.
    to: &'a str,
  },
  /// Make the directory `path` with `mode`.
  Mkdir {
    /// The directory.
    path: &'a str,
    /// Its permission bits.
    mode: u32,
  },
}

/// Serves `op` on `volume` under the caller's write `attachment` (the module doc).
pub(crate) fn serve(
  state: &mut ShardState,
  principal: &Principal,
  (volume, attachment): (VolumeId, u64),
  op: FsOp<'_>,
) -> ReplyBody {
  let (handle, record) = match find(state, volume) {
    Ok(found) => found,
    Err(reply) => return *reply,
  };
  if record.policy.role != Role::Plain {
    return refused(Refusal::BadRequest {
      reason: "the file verbs change a plain volume; a work changes through edit and declare"
        .to_owned(),
    });
  }
  let Some(catalog) = state.db.partition().attachment(attachment).cloned() else {
    return refused(Refusal::NotFound);
  };
  if catalog.volume != to_db_volume(volume)
    || catalog.principal != *principal
    || !catalog.rights.write
    || catalog.snapshot.is_some()
  {
    return forbidden("fs");
  }
  // The registry is bounded: at its bound the verb is refused retryable, as a full cross-shard queue is.
  let Some(registry) = admit(state, volume, attachment, &catalog) else {
    return refused(Refusal::Overloaded { shard: state.shard });
  };
  let scope = catalog.form.scope();
  let ShardState {
    store,
    volumes,
    attachments,
    ..
  } = state;
  let Ok(slot) = volumes.get_mut(handle) else {
    return refused(Refusal::NotFound);
  };
  let mut handles = new_handle_store();
  let mut bridge = VolumeBridge::attached(
    to_db_volume(volume),
    &mut slot.volume,
    store,
    &mut handles,
    slot.host.as_mut().map(|host| host as &mut dyn HostFs),
  );
  let mut cx = match attachments.begin(registry) {
    Ok(cx) => cx,
    Err(e) => return refused(refusal_of_vfs(&e)),
  };
  cx.owner_uid = unix_uid(principal);
  let mut scoped;
  let served: &mut dyn Bridge = match scope {
    Some(scope) => {
      scoped = slates_bridge_core::scoped::ScopedBridge::new(&mut bridge, scope);
      &mut scoped
    }
    None => &mut bridge,
  };
  let outcome = run(served, &cx, op);
  attachments.end(registry);
  match outcome {
    Ok(size) => ReplyBody::FsDone { size },
    Err(e) => refused(refusal_of_vfs(&e)),
  }
}

/// The registry attachment the catalog `attachment` rides, admitted on its first verb as a mount's is on its first
/// request (`crate::nfs`), and ended with the record (`verbs::end_attachment`). Its writes are durable when answered,
/// so a snapshot over it is complete. `None` when the registry is at its bound.
fn admit(
  state: &mut ShardState,
  volume: VolumeId,
  attachment: u64,
  catalog: &slates_db::catalog::AttachmentRecord,
) -> Option<slates_bridge_core::AttachmentId> {
  if let Some(mounted) = state.mount_attachments.get(&attachment) {
    return Some(mounted.registry);
  }
  let rights = slates_bridge_core::Rights {
    read: catalog.rights.read,
    write: catalog.rights.write,
  };
  let registry = state
    .attachments
    .attach(
      to_db_volume(volume),
      slates_bridge_core::View::Current,
      catalog.principal.clone(),
      rights,
    )
    .ok()?;
  state.mount_attachments.insert(
    attachment,
    MountAttachment {
      registry,
      boundary: slates_ipc::protocol::SnapshotBoundary::Complete,
    },
  );
  Some(registry)
}

/// The owner a Unix caller's created files take: its uid, or a consumer's host account; none for a principal with no
/// Unix identity (the bridge's default owner).
fn unix_uid(principal: &Principal) -> Option<u32> {
  match principal {
    Principal::Uid { uid } => Some(*uid),
    Principal::Consumer { account, .. } => Some(*account),
    Principal::Sid { .. } | Principal::Certificate { .. } => None,
  }
}

/// Runs `op` on `bridge` under `cx`: the changed object's size.
fn run(bridge: &mut dyn Bridge, cx: &OpContext, op: FsOp<'_>) -> Result<u64, VfsError> {
  match op {
    FsOp::Write { path, bytes, mode } => {
      let (parent, name) = parent_of(bridge, cx, path)?;
      write_file(bridge, cx, (parent, name), bytes, mode)
    }
    FsOp::Remove { path } => {
      let (parent, name) = parent_of(bridge, cx, path)?;
      let found = bridge.lookup(parent, cx, name)?;
      if found.kind == Kind::Dir {
        bridge.rmdir(parent, cx, name)?;
      } else {
        bridge.unlink(parent, cx, name)?;
      }
      Ok(0)
    }
    FsOp::Rename { from, to } => {
      let (old_parent, old_name) = parent_of(bridge, cx, from)?;
      let (new_parent, new_name) = parent_of(bridge, cx, to)?;
      bridge.rename(
        old_parent,
        new_parent,
        cx,
        old_name,
        new_name,
        RenameFlags::default(),
      )?;
      Ok(bridge.lookup(new_parent, cx, new_name)?.size)
    }
    FsOp::Mkdir { path, mode } => {
      let (parent, name) = parent_of(bridge, cx, path)?;
      bridge.mkdir(parent, cx, name, mode)?;
      Ok(0)
    }
  }
}

/// Replaces the file `name` in `parent` with `bytes` (the module doc's "What a write is").
fn write_file(
  bridge: &mut dyn Bridge,
  cx: &OpContext,
  (parent, name): (ObjectId, &str),
  bytes: &[u8],
  mode: u32,
) -> Result<u64, VfsError> {
  let file = match bridge.lookup(parent, cx, name) {
    Ok(found) if found.kind == Kind::Dir => return Err(VfsError::IsDirectory),
    Ok(found) => {
      let file = ObjectId::new(found.ino, found.generation);
      bridge.setattr(
        file,
        cx,
        SetAttr {
          size: Some(0),
          ..SetAttr::default()
        },
      )?;
      file
    }
    Err(VfsError::NotFound) => {
      let (created, fh) = bridge.create(parent, cx, name, mode, 0)?;
      let file = ObjectId::new(created.ino, created.generation);
      bridge.release(file, cx, fh)?;
      file
    }
    Err(e) => return Err(e),
  };
  let mut written = 0usize;
  while let Some(rest) = bytes.get(written..)
    && !rest.is_empty()
  {
    let wrote = bridge.write(file, cx, u64::try_from(written).unwrap_or(u64::MAX), rest)?;
    let wrote = usize::try_from(wrote).unwrap_or(0);
    if wrote == 0 {
      return Err(VfsError::NoSpace);
    }
    written = written.saturating_add(wrote);
  }
  Ok(u64::try_from(written).unwrap_or(u64::MAX))
}

/// The directory that holds `path`'s last component, walked from the attachment's root, and that component. A path
/// is `/`-separated components, none of them `.` or `..`, and names at least one.
fn parent_of<'p>(
  bridge: &mut dyn Bridge,
  cx: &OpContext,
  path: &'p str,
) -> Result<(ObjectId, &'p str), VfsError> {
  let mut components = path.split('/').filter(|part| !part.is_empty());
  let mut last = components.next().ok_or(VfsError::Invalid)?;
  let root = bridge.root(cx)?;
  let mut dir = ObjectId::new(root, 0);
  for next in components {
    if last == "." || last == ".." {
      return Err(VfsError::Invalid);
    }
    let found = bridge.lookup(dir, cx, last)?;
    if found.kind != Kind::Dir {
      return Err(VfsError::NotDirectory);
    }
    dir = ObjectId::new(found.ino, found.generation);
    last = next;
  }
  if last == "." || last == ".." {
    return Err(VfsError::Invalid);
  }
  Ok((dir, last))
}
