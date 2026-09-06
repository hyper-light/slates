//! The NFSv3 procedures over the shared operation layer (§4.6, Phase 4 task 3), driven end to end
//! against an in-memory scratch volume through a real `VolumeBridge` — the same seam the FUSE mount
//! uses — on every host with no socket and no mount. The client's opening walk is exercised: MOUNT
//! `MNT` yields the export's root handle, `LOOKUP` resolves a name to a handle and attributes, and
//! `GETATTR` on that handle returns the same object; a foreign handle and a missing name are typed
//! NFS statuses, not panics.
// Test harness code: an unwrap here is a failed test.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use slates_bridge_core::{Attachments, Bridge, ObjectId, OpContext, Rights, View, VolumeBridge};
use slates_bridge_nfs::mount::MountReply;
use slates_bridge_nfs::nfs::{Fattr3, Ftype3, Nfsfh3, Nfsstat3, PostOpAttr};
use slates_bridge_nfs::procedures::{
  Export, NFSPROC3_ACCESS, NFSPROC3_FSINFO, NFSPROC3_FSSTAT, NFSPROC3_GETATTR, NFSPROC3_NULL,
};
use slates_bridge_nfs::xdr::{XdrReader, XdrWriter};
use slates_db::catalog::{Principal, VolumeId};
use slates_mem::arena::ChunkArena;
use slates_mem::region::Region;
use slates_vfs::clock::HostClock;
use slates_vfs::names::NameEquivalence;
use slates_vfs::quota::Quota;
use slates_vfs::volume::{Store, StoreConfig, Volume, VolumeConfig};

/// A read-write context minted through the attachment registry.
fn write_cx() -> OpContext {
  let mut attachments = Attachments::new();
  let id = attachments
    .attach(
      VolumeId { bytes: [0x11; 16] },
      View::Current,
      Principal::Uid { uid: 0 },
      Rights {
        read: true,
        write: true,
      },
    )
    .unwrap();
  attachments.context(id).unwrap()
}

const PAGE: usize = 4096;
const REGION_PAGES: usize = 4096;

fn store() -> Store {
  let mut arena = ChunkArena::new(PAGE);
  arena
    .add_region(Region::map(PAGE * REGION_PAGES, PAGE, false).unwrap())
    .unwrap();
  Store::new(
    &StoreConfig {
      page: PAGE,
      cache_line: 128,
      max_dirs: 64,
      max_inodes: 256,
      max_chunks: REGION_PAGES,
      max_dir_blocks: 64,
      dir_cutover: 16,
    },
    arena,
  )
}

fn volume(store: &mut Store) -> Volume {
  Volume::create(
    store,
    VolumeConfig {
      prefix: 1,
      names: NameEquivalence::Exact,
      quota: Quota::Bounded { limit: 1 << 30 },
      journal_bytes: 1 << 16,
      clock: Box::new(HostClock::default()),
    },
  )
  .unwrap()
}

/// The client's opening walk: MNT gives the root handle, LOOKUP resolves a name to a handle and
/// attributes, and GETATTR on that handle returns the same object.
#[test]
fn mount_lookup_and_getattr_walk_the_export() {
  let mut store = store();
  let mut vol = volume(&mut store);
  let mut bridge = VolumeBridge::new(&mut vol, &mut store);
  let root_ino = bridge.root().unwrap();
  bridge.create(root_ino, "hello", 0o644, 0).unwrap();

  let mut export = Export::new(&mut bridge, VolumeId { bytes: [0x11; 16] });

  // NULL is an empty result.
  assert!(
    export
      .serve_nfs(NFSPROC3_NULL, &mut XdrReader::new(&[]))
      .unwrap()
      .is_empty()
  );

  // MNT yields the export's root handle.
  let root_fh = match export.mnt("/") {
    MountReply::Ok { handle, .. } => handle,
    MountReply::Err(status) => panic!("MNT failed: {status:?}"),
  };

  // LOOKUP "hello" in the root.
  let mut args = XdrWriter::new();
  root_fh.encode(&mut args);
  args.opaque("hello".as_bytes());
  let reply = export.lookup(&mut XdrReader::new(args.as_slice()));
  let mut r = XdrReader::new(&reply);
  assert_eq!(r.u32().unwrap(), Nfsstat3::Ok.wire(), "LOOKUP succeeded");
  let child_fh = Nfsfh3::decode(&mut r).unwrap();
  let child = PostOpAttr::decode(&mut r).unwrap().0.expect("object attrs");
  assert_eq!(child.kind, Ftype3::Reg);
  assert_eq!(child.mode, 0o644);
  let _dir = PostOpAttr::decode(&mut r).unwrap();

  // GETATTR on the looked-up handle returns the same object (same fileid and mode).
  let mut gargs = XdrWriter::new();
  child_fh.encode(&mut gargs);
  let greply = export.serve_nfs(NFSPROC3_GETATTR, &mut XdrReader::new(gargs.as_slice()));
  let mut gr = XdrReader::new(greply.as_ref().unwrap());
  assert_eq!(gr.u32().unwrap(), Nfsstat3::Ok.wire(), "GETATTR succeeded");
  let attr = Fattr3::decode(&mut gr).unwrap();
  assert_eq!(attr.fileid, child.fileid, "GETATTR names the same inode");
  assert_eq!(attr.mode, 0o644);
}

/// A file handle for another volume is refused stale, and a malformed one is a bad handle — never
/// a panic and never another volume's object.
#[test]
fn a_foreign_or_malformed_handle_is_refused() {
  let mut store = store();
  let mut vol = volume(&mut store);
  let mut bridge = VolumeBridge::new(&mut vol, &mut store);
  let mut export = Export::new(&mut bridge, VolumeId { bytes: [0x11; 16] });

  // A well-formed handle naming a different volume.
  let foreign = slates_bridge_nfs::FileHandle {
    volume: VolumeId { bytes: [0x22; 16] },
    inode: 1,
    generation: 0,
  }
  .to_fh();
  let mut args = XdrWriter::new();
  foreign.encode(&mut args);
  let reply = export.getattr(&mut XdrReader::new(args.as_slice()));
  assert_eq!(
    XdrReader::new(&reply).u32().unwrap(),
    Nfsstat3::Stale.wire(),
    "a foreign volume's handle is stale"
  );

  // A too-short opaque handle is a bad handle.
  let mut short = XdrWriter::new();
  short.opaque(&[0u8; 4]);
  let reply = export.getattr(&mut XdrReader::new(short.as_slice()));
  assert_eq!(
    XdrReader::new(&reply).u32().unwrap(),
    Nfsstat3::Badhandle.wire(),
    "a malformed handle is refused"
  );
}

/// A LOOKUP of a name that does not exist is `NFS3ERR_NOENT`.
#[test]
fn a_missing_name_is_noent() {
  let mut store = store();
  let mut vol = volume(&mut store);
  let mut bridge = VolumeBridge::new(&mut vol, &mut store);
  let root_ino = bridge.root().unwrap();
  let mut export = Export::new(&mut bridge, VolumeId { bytes: [0x11; 16] });
  let root_fh = slates_bridge_nfs::FileHandle {
    volume: VolumeId { bytes: [0x11; 16] },
    inode: root_ino,
    generation: 0,
  }
  .to_fh();

  let mut args = XdrWriter::new();
  root_fh.encode(&mut args);
  args.opaque("absent".as_bytes());
  let reply = export.lookup(&mut XdrReader::new(args.as_slice()));
  assert_eq!(
    XdrReader::new(&reply).u32().unwrap(),
    Nfsstat3::Noent.wire(),
    "a missing name is ENOENT"
  );
}

/// The post-mount queries answer over the root handle: FSINFO reports transfer sizes and the
/// symlink capability, ACCESS grants what it is asked, and FSSTAT reports the volume's space.
#[test]
fn the_post_mount_queries_answer_over_the_root() {
  let mut store = store();
  let mut vol = volume(&mut store);
  let mut bridge = VolumeBridge::new(&mut vol, &mut store);
  let mut export = Export::new(&mut bridge, VolumeId { bytes: [0x11; 16] });
  let root_fh = match export.mnt("/") {
    MountReply::Ok { handle, .. } => handle,
    MountReply::Err(status) => panic!("MNT failed: {status:?}"),
  };

  // FSINFO: OK, then the object attrs, then the transfer sizes and the properties word.
  let mut args = XdrWriter::new();
  root_fh.encode(&mut args);
  let reply = export
    .serve_nfs(NFSPROC3_FSINFO, &mut XdrReader::new(args.as_slice()))
    .unwrap();
  let mut r = XdrReader::new(&reply);
  assert_eq!(r.u32().unwrap(), Nfsstat3::Ok.wire());
  PostOpAttr::decode(&mut r).unwrap();
  assert_eq!(r.u32().unwrap(), 256 * 1024, "rtmax is one arena chunk");
  for _ in 0..6 {
    r.u32().unwrap(); // rtpref, rtmult, wtmax, wtpref, wtmult, dtpref
  }
  r.u64().unwrap(); // maxfilesize
  r.u32().unwrap(); // time_delta seconds
  r.u32().unwrap(); // time_delta nseconds
  let properties = r.u32().unwrap();
  assert!(
    properties & 0x2 != 0,
    "the server advertises symlink support"
  );

  // ACCESS: the requested bits are granted.
  let mut args = XdrWriter::new();
  root_fh.encode(&mut args);
  args.u32(0x1 | 0x2); // ACCESS3_READ | ACCESS3_LOOKUP
  let reply = export
    .serve_nfs(NFSPROC3_ACCESS, &mut XdrReader::new(args.as_slice()))
    .unwrap();
  let mut r = XdrReader::new(&reply);
  assert_eq!(r.u32().unwrap(), Nfsstat3::Ok.wire());
  PostOpAttr::decode(&mut r).unwrap();
  assert_eq!(
    r.u32().unwrap(),
    0x1 | 0x2,
    "the requested access is granted"
  );

  // FSSTAT: the volume reports some total space.
  let mut args = XdrWriter::new();
  root_fh.encode(&mut args);
  let reply = export
    .serve_nfs(NFSPROC3_FSSTAT, &mut XdrReader::new(args.as_slice()))
    .unwrap();
  let mut r = XdrReader::new(&reply);
  assert_eq!(r.u32().unwrap(), Nfsstat3::Ok.wire());
  PostOpAttr::decode(&mut r).unwrap();
  assert!(r.u64().unwrap() > 0, "the volume reports total space");
}

/// ACCESS reflects the object's permissions, not the request: a read-only file grants READ but
/// refuses MODIFY (audit correction — it must not echo the requested bits).
#[test]
fn access_reflects_the_mode_not_the_request() {
  let mut store = store();
  let mut vol = volume(&mut store);
  let mut bridge = VolumeBridge::new(&mut vol, &mut store);
  let root_ino = bridge.root().unwrap();
  bridge.create(root_ino, "ro", 0o444, 0).unwrap();
  let mut export = Export::new(&mut bridge, VolumeId { bytes: [0x11; 16] });
  let root_fh = match export.mnt("/") {
    MountReply::Ok { handle, .. } => handle,
    MountReply::Err(status) => panic!("MNT failed: {status:?}"),
  };

  let mut la = XdrWriter::new();
  root_fh.encode(&mut la);
  la.opaque("ro".as_bytes());
  let lreply = export.lookup(&mut XdrReader::new(la.as_slice()));
  let mut lr = XdrReader::new(&lreply);
  assert_eq!(lr.u32().unwrap(), Nfsstat3::Ok.wire());
  let file_fh = Nfsfh3::decode(&mut lr).unwrap();

  // Request READ | MODIFY; a 0o444 file grants only READ.
  let mut aa = XdrWriter::new();
  file_fh.encode(&mut aa);
  aa.u32(0x1 | 0x4);
  let areply = export
    .serve_nfs(NFSPROC3_ACCESS, &mut XdrReader::new(aa.as_slice()))
    .unwrap();
  let mut ar = XdrReader::new(&areply);
  assert_eq!(ar.u32().unwrap(), Nfsstat3::Ok.wire());
  PostOpAttr::decode(&mut ar).unwrap();
  assert_eq!(
    ar.u32().unwrap(),
    0x1,
    "a read-only file grants READ but not MODIFY"
  );
}

/// A handle whose generation no longer matches the object's is refused stale — the encoded
/// generation is validated, not discarded (audit correction).
#[test]
fn a_stale_generation_handle_is_refused() {
  let mut store = store();
  let mut vol = volume(&mut store);
  let mut bridge = VolumeBridge::new(&mut vol, &mut store);
  let root_ino = bridge.root().unwrap();
  let mut export = Export::new(&mut bridge, VolumeId { bytes: [0x11; 16] });

  // A handle to a live inode but carrying a generation the object does not have.
  let stale = slates_bridge_nfs::FileHandle {
    volume: VolumeId { bytes: [0x11; 16] },
    inode: root_ino,
    generation: 1,
  }
  .to_fh();
  let mut args = XdrWriter::new();
  stale.encode(&mut args);
  let reply = export.getattr(&mut XdrReader::new(args.as_slice()));
  assert_eq!(
    XdrReader::new(&reply).u32().unwrap(),
    Nfsstat3::Stale.wire(),
    "a handle whose generation no longer matches is stale"
  );
}

/// A handle to an inode the volume has reclaimed is refused stale (not NOENT): inode numbers are
/// never reused, so a gone number is a gone object.
#[test]
fn a_handle_to_a_reclaimed_inode_is_stale() {
  let mut store = store();
  let mut vol = volume(&mut store);
  let mut bridge = VolumeBridge::new(&mut vol, &mut store);
  let root_ino = bridge.root().unwrap();
  let (attr, _fh) = bridge.create(root_ino, "gone", 0o644, 0).unwrap();
  let gone_ino = attr.ino;
  // No volume reference is held, so removing the name reclaims the inode and frees its number.
  bridge.unlink(root_ino, "gone").unwrap();

  let mut export = Export::new(&mut bridge, VolumeId { bytes: [0x11; 16] });
  let handle = slates_bridge_nfs::FileHandle {
    volume: VolumeId { bytes: [0x11; 16] },
    inode: gone_ino,
    generation: 0,
  }
  .to_fh();
  let mut args = XdrWriter::new();
  handle.encode(&mut args);
  let reply = export.getattr(&mut XdrReader::new(args.as_slice()));
  assert_eq!(
    XdrReader::new(&reply).u32().unwrap(),
    Nfsstat3::Stale.wire(),
    "a handle to a reclaimed inode is stale, not noent"
  );
}

/// Object identity survives copy-on-write: a handle taken before a write still names the same
/// object after it (same fileid), with the updated size — the handle generation is stable across
/// CoW, not the slab-slot generation (Ada review point 5).
#[test]
fn a_handle_survives_copy_on_write_of_its_object() {
  let mut store = store();
  let mut vol = volume(&mut store);
  let mut bridge = VolumeBridge::new(&mut vol, &mut store);
  let root_ino = bridge.root().unwrap();
  let (created, _fh) = bridge.create(root_ino, "f", 0o644, 0).unwrap();
  let file_ino = created.ino;

  // Take a handle, then write through the file (a copy-on-write of the inode version).
  let handle = slates_bridge_nfs::FileHandle {
    volume: VolumeId { bytes: [0x11; 16] },
    inode: file_ino,
    generation: 0,
  }
  .to_fh();
  bridge
    .write(
      ObjectId {
        inode: file_ino,
        generation: 0,
      },
      &write_cx(),
      0,
      b"hello world",
    )
    .unwrap();

  let mut export = Export::new(&mut bridge, VolumeId { bytes: [0x11; 16] });
  let mut args = XdrWriter::new();
  handle.encode(&mut args);
  let reply = export
    .serve_nfs(NFSPROC3_GETATTR, &mut XdrReader::new(args.as_slice()))
    .unwrap();
  let mut r = XdrReader::new(&reply);
  assert_eq!(r.u32().unwrap(), Nfsstat3::Ok.wire());
  let attr = Fattr3::decode(&mut r).unwrap();
  assert_eq!(
    attr.fileid, file_ino,
    "the handle names the same object after CoW"
  );
  assert_eq!(attr.size, 11, "with the written bytes");
}
