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
  NFSPROC3_READ, NFSPROC3_REMOVE, NFSPROC3_RENAME, NFSPROC3_RMDIR, NFSPROC3_WRITE,
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

/// The object at inode `ino` (generation zero - the volume core does not track generations yet).
fn oid(ino: u64) -> ObjectId {
  ObjectId::new(ino, 0)
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
  let mut bridge = VolumeBridge::new(VolumeId { bytes: [0x11; 16] }, &mut vol, &mut store);
  let cx = write_cx();
  let root_ino = bridge.root(&cx).unwrap();
  bridge
    .create(oid(root_ino), &cx, "hello", 0o644, 0)
    .unwrap();

  let mut export = Export::new(
    &mut bridge,
    VolumeId { bytes: [0x11; 16] },
    Principal::Uid { uid: 0 },
    Rights {
      read: true,
      write: true,
    },
  )
  .unwrap();

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
  let mut bridge = VolumeBridge::new(VolumeId { bytes: [0x11; 16] }, &mut vol, &mut store);
  let mut export = Export::new(
    &mut bridge,
    VolumeId { bytes: [0x11; 16] },
    Principal::Uid { uid: 0 },
    Rights {
      read: true,
      write: true,
    },
  )
  .unwrap();

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
  let mut bridge = VolumeBridge::new(VolumeId { bytes: [0x11; 16] }, &mut vol, &mut store);
  let cx = write_cx();
  let root_ino = bridge.root(&cx).unwrap();
  let mut export = Export::new(
    &mut bridge,
    VolumeId { bytes: [0x11; 16] },
    Principal::Uid { uid: 0 },
    Rights {
      read: true,
      write: true,
    },
  )
  .unwrap();
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
  let mut bridge = VolumeBridge::new(VolumeId { bytes: [0x11; 16] }, &mut vol, &mut store);
  let mut export = Export::new(
    &mut bridge,
    VolumeId { bytes: [0x11; 16] },
    Principal::Uid { uid: 0 },
    Rights {
      read: true,
      write: true,
    },
  )
  .unwrap();
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
  let mut bridge = VolumeBridge::new(VolumeId { bytes: [0x11; 16] }, &mut vol, &mut store);
  let cx = write_cx();
  let root_ino = bridge.root(&cx).unwrap();
  bridge.create(oid(root_ino), &cx, "ro", 0o444, 0).unwrap();
  let mut export = Export::new(
    &mut bridge,
    VolumeId { bytes: [0x11; 16] },
    Principal::Uid { uid: 0 },
    Rights {
      read: true,
      write: true,
    },
  )
  .unwrap();
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
  let mut bridge = VolumeBridge::new(VolumeId { bytes: [0x11; 16] }, &mut vol, &mut store);
  let cx = write_cx();
  let root_ino = bridge.root(&cx).unwrap();
  let mut export = Export::new(
    &mut bridge,
    VolumeId { bytes: [0x11; 16] },
    Principal::Uid { uid: 0 },
    Rights {
      read: true,
      write: true,
    },
  )
  .unwrap();

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
  let mut bridge = VolumeBridge::new(VolumeId { bytes: [0x11; 16] }, &mut vol, &mut store);
  let cx = write_cx();
  let root_ino = bridge.root(&cx).unwrap();
  let (attr, fh) = bridge.create(oid(root_ino), &cx, "gone", 0o644, 0).unwrap();
  let gone_ino = attr.ino;
  bridge.unlink(oid(root_ino), &cx, "gone").unwrap();
  // Drop the references the create took — the open handle and the kernel's lookup — so the
  // unlinked inode is reclaimed and its number freed (an open/looked-up inode survives unlink).
  bridge.release(oid(gone_ino), &cx, fh).unwrap();
  bridge.forget(oid(gone_ino), &cx, 1);

  let mut export = Export::new(
    &mut bridge,
    VolumeId { bytes: [0x11; 16] },
    Principal::Uid { uid: 0 },
    Rights {
      read: true,
      write: true,
    },
  )
  .unwrap();
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
  let mut bridge = VolumeBridge::new(VolumeId { bytes: [0x11; 16] }, &mut vol, &mut store);
  let cx = write_cx();
  let root_ino = bridge.root(&cx).unwrap();
  let (created, _fh) = bridge.create(oid(root_ino), &cx, "f", 0o644, 0).unwrap();
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

  let mut export = Export::new(
    &mut bridge,
    VolumeId { bytes: [0x11; 16] },
    Principal::Uid { uid: 0 },
    Rights {
      read: true,
      write: true,
    },
  )
  .unwrap();
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

/// A WRITE then a READ over the export round-trips file content through the shared inode-addressed
/// interface: the bytes NFSPROC3_WRITE stores come back from NFSPROC3_READ, the WRITE reports the
/// post-write size and FILE_SYNC, and the READ reports the byte count and the end-of-file flag.
#[test]
fn a_write_then_read_round_trips_over_the_export() {
  let mut store = store();
  let mut vol = volume(&mut store);
  let mut bridge = VolumeBridge::new(VolumeId { bytes: [0x11; 16] }, &mut vol, &mut store);
  let cx = write_cx();
  let root_ino = bridge.root(&cx).unwrap();
  let (created, _fh) = bridge.create(oid(root_ino), &cx, "data", 0o644, 0).unwrap();
  let file_ino = created.ino;

  let mut export = Export::new(
    &mut bridge,
    VolumeId { bytes: [0x11; 16] },
    Principal::Uid { uid: 0 },
    Rights {
      read: true,
      write: true,
    },
  )
  .unwrap();
  let file_fh = slates_bridge_nfs::FileHandle {
    volume: VolumeId { bytes: [0x11; 16] },
    inode: file_ino,
    generation: 0,
  }
  .to_fh();

  // WRITE the payload at offset 0.
  let payload: &[u8] = b"slates";
  let count = u32::try_from(payload.len()).unwrap();
  let mut wargs = XdrWriter::new();
  file_fh.encode(&mut wargs);
  wargs.u64(0); // offset
  wargs.u32(count); // count (advisory; the data length is authoritative)
  wargs.u32(2); // stable = FILE_SYNC (advisory)
  wargs.opaque(payload);
  let wreply = export
    .serve_nfs(NFSPROC3_WRITE, &mut XdrReader::new(wargs.as_slice()))
    .unwrap();
  let mut wr = XdrReader::new(&wreply);
  assert_eq!(wr.u32().unwrap(), Nfsstat3::Ok.wire(), "WRITE succeeded");
  // wcc_data: pre_op_attr absent, then post_op_attr present with the new size.
  assert!(!wr.bool().unwrap(), "no pre-op attributes");
  let post = PostOpAttr::decode(&mut wr).unwrap().0.expect("post attrs");
  assert_eq!(
    post.size,
    u64::from(count),
    "the file grew to the written size"
  );
  assert_eq!(wr.u32().unwrap(), count, "WRITE reports the byte count");
  assert_eq!(wr.u32().unwrap(), 2, "slates commits FILE_SYNC");

  // READ the whole file back (asking for more than it holds).
  let mut rargs = XdrWriter::new();
  file_fh.encode(&mut rargs);
  rargs.u64(0); // offset
  rargs.u32(64); // count larger than the file
  let rreply = export
    .serve_nfs(NFSPROC3_READ, &mut XdrReader::new(rargs.as_slice()))
    .unwrap();
  let mut rr = XdrReader::new(&rreply);
  assert_eq!(rr.u32().unwrap(), Nfsstat3::Ok.wire(), "READ succeeded");
  PostOpAttr::decode(&mut rr).unwrap();
  assert_eq!(rr.u32().unwrap(), count, "READ returns the byte count");
  assert!(rr.bool().unwrap(), "the read reached end of file");
  let data = rr.opaque(64).unwrap();
  assert_eq!(data, payload, "the bytes round-trip");
}

/// A WRITE through a read-only export is refused by the seam before any effect — authorization at
/// the NFS edge, not only ACCESS reporting — while a READ through the same export is allowed.
#[test]
fn a_write_through_a_read_only_export_is_refused() {
  let mut store = store();
  let mut vol = volume(&mut store);
  let mut bridge = VolumeBridge::new(VolumeId { bytes: [0x11; 16] }, &mut vol, &mut store);
  let cx = write_cx();
  let root_ino = bridge.root(&cx).unwrap();
  let (created, _fh) = bridge.create(oid(root_ino), &cx, "ro", 0o644, 0).unwrap();
  let file_ino = created.ino;

  // A read-only export: its attachment carries read but not write.
  let mut export = Export::new(
    &mut bridge,
    VolumeId { bytes: [0x11; 16] },
    Principal::Uid { uid: 0 },
    Rights {
      read: true,
      write: false,
    },
  )
  .unwrap();
  let file_fh = slates_bridge_nfs::FileHandle {
    volume: VolumeId { bytes: [0x11; 16] },
    inode: file_ino,
    generation: 0,
  }
  .to_fh();

  let mut wargs = XdrWriter::new();
  file_fh.encode(&mut wargs);
  wargs.u64(0);
  wargs.u32(1);
  wargs.u32(2);
  wargs.opaque(b"x");
  let wreply = export
    .serve_nfs(NFSPROC3_WRITE, &mut XdrReader::new(wargs.as_slice()))
    .unwrap();
  assert_eq!(
    XdrReader::new(&wreply).u32().unwrap(),
    Nfsstat3::Perm.wire(),
    "a read-only export refuses WRITE before any effect"
  );

  // A READ through the same export is allowed.
  let mut rargs = XdrWriter::new();
  file_fh.encode(&mut rargs);
  rargs.u64(0);
  rargs.u32(16);
  let rreply = export
    .serve_nfs(NFSPROC3_READ, &mut XdrReader::new(rargs.as_slice()))
    .unwrap();
  assert_eq!(
    XdrReader::new(&rreply).u32().unwrap(),
    Nfsstat3::Ok.wire(),
    "but a read-only export can READ"
  );
}

/// REMOVE over the export unlinks the name from its directory: afterwards a LOOKUP of the name is
/// NFS3ERR_NOENT, and the reply carries the directory's post-op attributes (wcc_data).
#[test]
fn a_remove_over_the_export_unlinks_the_name() {
  let mut store = store();
  let mut vol = volume(&mut store);
  let mut bridge = VolumeBridge::new(VolumeId { bytes: [0x11; 16] }, &mut vol, &mut store);
  let cx = write_cx();
  let root_ino = bridge.root(&cx).unwrap();
  bridge
    .create(oid(root_ino), &cx, "doomed", 0o644, 0)
    .unwrap();

  let mut export = Export::new(
    &mut bridge,
    VolumeId { bytes: [0x11; 16] },
    Principal::Uid { uid: 0 },
    Rights {
      read: true,
      write: true,
    },
  )
  .unwrap();
  let root_fh = match export.mnt("/") {
    MountReply::Ok { handle, .. } => handle,
    MountReply::Err(status) => panic!("MNT failed: {status:?}"),
  };

  // REMOVE "doomed" from the root.
  let mut args = XdrWriter::new();
  root_fh.encode(&mut args);
  args.opaque("doomed".as_bytes());
  let reply = export
    .serve_nfs(NFSPROC3_REMOVE, &mut XdrReader::new(args.as_slice()))
    .unwrap();
  let mut rr = XdrReader::new(&reply);
  assert_eq!(rr.u32().unwrap(), Nfsstat3::Ok.wire(), "REMOVE succeeded");
  assert!(!rr.bool().unwrap(), "wcc: no pre-op attributes");
  PostOpAttr::decode(&mut rr)
    .unwrap()
    .0
    .expect("dir post attrs");

  // LOOKUP of the removed name now misses.
  let mut la = XdrWriter::new();
  root_fh.encode(&mut la);
  la.opaque("doomed".as_bytes());
  let lreply = export.lookup(&mut XdrReader::new(la.as_slice()));
  assert_eq!(
    XdrReader::new(&lreply).u32().unwrap(),
    Nfsstat3::Noent.wire(),
    "the removed name is gone"
  );
}

/// RMDIR over the export removes an empty directory; its name then misses.
#[test]
fn a_rmdir_over_the_export_removes_the_directory() {
  let mut store = store();
  let mut vol = volume(&mut store);
  let mut bridge = VolumeBridge::new(VolumeId { bytes: [0x11; 16] }, &mut vol, &mut store);
  let cx = write_cx();
  let root_ino = bridge.root(&cx).unwrap();
  bridge.mkdir(oid(root_ino), &cx, "sub", 0o755).unwrap();

  let mut export = Export::new(
    &mut bridge,
    VolumeId { bytes: [0x11; 16] },
    Principal::Uid { uid: 0 },
    Rights {
      read: true,
      write: true,
    },
  )
  .unwrap();
  let root_fh = match export.mnt("/") {
    MountReply::Ok { handle, .. } => handle,
    MountReply::Err(status) => panic!("MNT failed: {status:?}"),
  };

  let mut args = XdrWriter::new();
  root_fh.encode(&mut args);
  args.opaque("sub".as_bytes());
  let reply = export
    .serve_nfs(NFSPROC3_RMDIR, &mut XdrReader::new(args.as_slice()))
    .unwrap();
  assert_eq!(
    XdrReader::new(&reply).u32().unwrap(),
    Nfsstat3::Ok.wire(),
    "RMDIR of an empty directory succeeds"
  );

  let mut la = XdrWriter::new();
  root_fh.encode(&mut la);
  la.opaque("sub".as_bytes());
  let lreply = export.lookup(&mut XdrReader::new(la.as_slice()));
  assert_eq!(
    XdrReader::new(&lreply).u32().unwrap(),
    Nfsstat3::Noent.wire(),
    "the directory is gone"
  );
}

/// RENAME over the export moves an entry: the old name misses and the new name resolves to the same
/// object (same fileid).
#[test]
fn a_rename_over_the_export_moves_the_entry() {
  let mut store = store();
  let mut vol = volume(&mut store);
  let mut bridge = VolumeBridge::new(VolumeId { bytes: [0x11; 16] }, &mut vol, &mut store);
  let cx = write_cx();
  let root_ino = bridge.root(&cx).unwrap();
  let (created, _fh) = bridge
    .create(oid(root_ino), &cx, "before", 0o644, 0)
    .unwrap();
  let file_ino = created.ino;

  let mut export = Export::new(
    &mut bridge,
    VolumeId { bytes: [0x11; 16] },
    Principal::Uid { uid: 0 },
    Rights {
      read: true,
      write: true,
    },
  )
  .unwrap();
  let root_fh = match export.mnt("/") {
    MountReply::Ok { handle, .. } => handle,
    MountReply::Err(status) => panic!("MNT failed: {status:?}"),
  };

  // RENAME "before" -> "after", both under the root.
  let mut args = XdrWriter::new();
  root_fh.encode(&mut args);
  args.opaque("before".as_bytes());
  root_fh.encode(&mut args);
  args.opaque("after".as_bytes());
  let reply = export
    .serve_nfs(NFSPROC3_RENAME, &mut XdrReader::new(args.as_slice()))
    .unwrap();
  assert_eq!(
    XdrReader::new(&reply).u32().unwrap(),
    Nfsstat3::Ok.wire(),
    "RENAME succeeded"
  );

  // The old name misses.
  let mut la = XdrWriter::new();
  root_fh.encode(&mut la);
  la.opaque("before".as_bytes());
  let lold = export.lookup(&mut XdrReader::new(la.as_slice()));
  assert_eq!(
    XdrReader::new(&lold).u32().unwrap(),
    Nfsstat3::Noent.wire(),
    "the old name is gone"
  );

  // The new name resolves to the same object.
  let mut na = XdrWriter::new();
  root_fh.encode(&mut na);
  na.opaque("after".as_bytes());
  let lnew = export.lookup(&mut XdrReader::new(na.as_slice()));
  let mut nr = XdrReader::new(&lnew);
  assert_eq!(
    nr.u32().unwrap(),
    Nfsstat3::Ok.wire(),
    "the new name resolves"
  );
  let _fh = Nfsfh3::decode(&mut nr).unwrap();
  let attr = PostOpAttr::decode(&mut nr)
    .unwrap()
    .0
    .expect("object attrs");
  assert_eq!(
    attr.fileid, file_ino,
    "the same object moved to the new name"
  );
}
