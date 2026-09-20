//! A-26 / §4.5: IPC names obey namespace and copy-on-write rules without acquiring file contents.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use slates_vfs::VfsError;
use slates_vfs::clock::StepClock;
use slates_vfs::inode::Kind;
use slates_vfs::recover::{BodyImage, VolumeImage};
use slates_vfs::volume::Volume;

/// AC-1.15 / A-26: derivation names the IPC kind, attributes and shared inode without content hunks.
#[test]
fn ipc_derivation_preserves_kind_metadata_and_link_identity() {
  let mut store = common::store();
  let mut volume = common::volume(&mut store, 1 << 24);
  let base = volume.snapshot(&mut store).unwrap();
  let root = volume.root_inode(&store).unwrap();
  let pipe = volume
    .mknod_no(&mut store, root, "pipe", 0o640, Kind::Fifo)
    .unwrap();
  volume.link_no(&mut store, root, "pipe-link", pipe).unwrap();
  volume.chown(&mut store, pipe, 123, 456).unwrap();
  let document = volume.derive(&store, base).unwrap();
  assert_ipc_derived_metadata(&document);
  assert_eq!(document.specials[0].path.as_ref(), "/pipe");
  assert_eq!(document.specials[0].link_to, None);
  assert_eq!(document.specials[1].link_to.as_deref(), Some("/pipe"));
  assert_eq!(
    document.encode(),
    volume.derive(&store, base).unwrap().encode()
  );
  let next = volume.snapshot(&mut store).unwrap();
  volume.chmod(&mut store, pipe, 0o600).unwrap();
  let changed = volume.derive(&store, next).unwrap();
  assert_eq!(
    changed.specials.len(),
    2,
    "metadata mutation reaches both names"
  );
  assert!(changed.specials.iter().all(|node| node.attrs.mode == 0o600));
}

/// The oracle observes the delta's metadata and proves that no content operation was invented.
fn assert_ipc_derived_metadata(document: &slates_vfs::derive::OpsDocument) {
  use slates_vfs::derive::SpecialKind;
  assert!(document.files.is_empty());
  assert_eq!(document.specials.len(), 2);
  for node in &document.specials {
    assert_eq!(node.kind, SpecialKind::Fifo);
    assert_eq!(
      (
        node.attrs.uid,
        node.attrs.gid,
        node.attrs.nlink,
        node.attrs.size
      ),
      (123, 456, 2, 0)
    );
  }
}

/// AC-1.6 / A-26: links, rename, metadata and unlink preserve each frozen namespace after recovery.
#[test]
fn ipc_names_survive_links_snapshots_clones_and_recovery() {
  let mut store = common::store();
  let mut volume = common::volume(&mut store, 1 << 24);
  let root = volume.root_inode(&store).unwrap();
  for (name, kind) in [("pipe", Kind::Fifo), ("socket", Kind::Socket)] {
    let inode = volume
      .mknod_no(&mut store, root, name, 0o640, kind)
      .unwrap();
    volume.chown(&mut store, inode, 123, 456).unwrap();
    volume
      .link_no(&mut store, root, &format!("{name}-link"), inode)
      .unwrap();
  }
  let snapshot = volume.snapshot(&mut store).unwrap();
  let mut clone = Volume::clone_of(&store, &mut volume, snapshot, common::clone_config(8)).unwrap();
  let pipe = clone.resolve(&store, "/pipe").unwrap().inode;
  clone.chmod(&mut store, pipe, 0o600).unwrap();
  clone
    .rename_no(&mut store, root, "socket", root, "moved")
    .unwrap();
  clone.unlink_no(&mut store, root, "socket-link").unwrap();
  assert_eq!(volume.stat(&store, pipe).unwrap().mode, 0o640);
  assert_eq!(clone.stat(&store, pipe).unwrap().mode, 0o600);
  let bytes = clone.to_image(&store, None).unwrap().to_content();
  let mut fresh = common::store();
  let recovered = Volume::from_image(
    &mut fresh,
    &VolumeImage::from_content(&bytes).unwrap(),
    Box::new(StepClock::new(0, 1)),
    1 << 16,
    None,
  )
  .unwrap();
  for (name, kind, links) in [("pipe", Kind::Fifo, 2), ("moved", Kind::Socket, 1)] {
    let inode = recovered.resolve(&fresh, name).unwrap().inode;
    assert_eq!(recovered.kind(&fresh, inode).unwrap(), kind);
    let attrs = recovered.stat(&fresh, inode).unwrap();
    assert_eq!(
      (attrs.uid, attrs.gid, attrs.nlink, attrs.size),
      (123, 456, links, 0)
    );
  }
  assert_eq!(
    recovered.to_image(&fresh, None).unwrap().to_content(),
    bytes
  );
  let (_, frozen_root) = volume.snapshot_info(snapshot).unwrap();
  assert_eq!(volume.readdir_in(&store, frozen_root).unwrap().len(), 4);
  assert!(volume.resolve_in(&store, snapshot, "/socket").is_ok());
}

/// AC-3.10 / A-26: regular-file operations refuse IPC names before changing their state.
#[test]
fn ipc_names_never_acquire_regular_file_contents() {
  let mut store = common::store();
  let mut volume = common::volume(&mut store, 1 << 24);
  let root = volume.root_inode(&store).unwrap();
  for (name, kind) in [("pipe", Kind::Fifo), ("socket", Kind::Socket)] {
    let inode = volume
      .mknod_no(&mut store, root, name, 0o600, kind)
      .unwrap();
    let snapshot = volume.snapshot(&mut store).unwrap();
    let before = volume.to_image(&store, None).unwrap().to_content();
    assert_eq!(
      volume.read(&store, inode, 0, &mut [0]),
      Err(VfsError::SpecialFileOperation)
    );
    assert_eq!(
      volume.read_in(&store, snapshot, inode, 0, &mut [0]),
      Err(VfsError::SpecialFileOperation)
    );
    assert_eq!(
      volume.write(&mut store, inode, 0, b"stream"),
      Err(VfsError::SpecialFileOperation)
    );
    assert_eq!(
      volume.write(&mut store, inode, 0, b""),
      Err(VfsError::SpecialFileOperation)
    );
    assert_eq!(
      volume.truncate(&mut store, inode, 1),
      Err(VfsError::SpecialFileOperation)
    );
    assert_eq!(
      volume.edit(&mut store, inode, 0, 0, b"stream"),
      Err(VfsError::SpecialFileOperation)
    );
    assert_eq!(volume.to_image(&store, None).unwrap().to_content(), before);
  }
}

/// AC-7.3 / A-26: an archive carries IPC kinds and owners without reading or hashing a stream.
#[test]
fn archives_preserve_ipc_metadata_without_stream_bytes() {
  use slates_archive::codec::CodecPolicy;
  use slates_vfs::export::{Progress, SnapshotArchiver, kind_of_mode};
  let mut store = common::store();
  let mut volume = common::volume(&mut store, 1 << 24);
  let root = volume.root_inode(&store).unwrap();
  for (name, kind) in [("pipe", Kind::Fifo), ("socket", Kind::Socket)] {
    let inode = volume
      .mknod_no(&mut store, root, name, 0o640, kind)
      .unwrap();
    volume.chown(&mut store, inode, 123, 456).unwrap();
  }
  let snapshot = volume.snapshot(&mut store).unwrap();
  let mut archiver =
    SnapshotArchiver::new(&volume, &store, snapshot, 0, 0, CodecPolicy::raw_only()).unwrap();
  let mut result = None;
  // Two leaf entries and the root close, each charged one content chunk.
  for _ in 0..3 {
    if let Progress::Done(archive) = archiver.advance(&volume, &store, 1).unwrap() {
      result = Some(archive);
      break;
    }
  }
  let restored = slates_archive::restore(&result.expect("bounded walk completes")).unwrap();
  for (name, kind) in [("pipe", Kind::Fifo), ("socket", Kind::Socket)] {
    let meta = &restored.metadata[name];
    assert_eq!(kind_of_mode(meta.mode), Some(kind));
    assert_eq!((meta.uid, meta.gid), (123, 456));
    assert!(restored.files[name].is_empty());
  }
}

/// T-2.1 / A-26: a foreign recovery image cannot smuggle content into a FIFO inode.
#[test]
fn recovery_refuses_content_attached_to_an_ipc_name() {
  let mut store = common::store();
  let mut volume = common::volume(&mut store, 1 << 24);
  let root = volume.root_inode(&store).unwrap();
  let pipe = volume
    .mknod_no(&mut store, root, "pipe", 0o600, Kind::Fifo)
    .unwrap();
  let mut image = volume.to_image(&store, None).unwrap();
  image
    .inodes
    .iter_mut()
    .find(|inode| inode.no == pipe.0)
    .unwrap()
    .body = BodyImage::Symlink {
    target: "host-endpoint".to_owned(),
  };
  let mut fresh = common::store();
  assert!(matches!(
    Volume::from_image(
      &mut fresh,
      &image,
      Box::new(StepClock::new(0, 1)),
      1 << 16,
      None
    ),
    Err(VfsError::RecoveryIncomplete)
  ));
}
