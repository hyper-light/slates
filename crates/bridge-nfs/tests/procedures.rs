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
  Export, NFSPROC3_ACCESS, NFSPROC3_COMMIT, NFSPROC3_CREATE, NFSPROC3_FSINFO, NFSPROC3_FSSTAT,
  NFSPROC3_GETATTR, NFSPROC3_LINK, NFSPROC3_LOOKUP, NFSPROC3_MKDIR, NFSPROC3_MKNOD, NFSPROC3_NULL,
  NFSPROC3_PATHCONF, NFSPROC3_READ, NFSPROC3_READDIR, NFSPROC3_READDIRPLUS, NFSPROC3_READLINK,
  NFSPROC3_REMOVE, NFSPROC3_RENAME, NFSPROC3_RMDIR, NFSPROC3_SETATTR, NFSPROC3_SYMLINK,
  NFSPROC3_WRITE,
};
use slates_bridge_nfs::xdr::{XdrReader, XdrWriter};
use slates_bridge_nfs::{MultiExport, NfsService, OwnedVolumeSet};
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
    0,
  )
}

fn volume(store: &mut Store) -> Volume {
  volume_with_names(store, NameEquivalence::Exact)
}

/// A volume with the given name-equivalence policy — `Exact` is case-sensitive, `Fold` case-folding
/// (APFS-style), the property PATHCONF reports.
fn volume_with_names(store: &mut Store, names: NameEquivalence) -> Volume {
  Volume::create(
    store,
    VolumeConfig {
      prefix: 1,
      names,
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

/// COMMIT over the export reports the file's writes stable and returns the same `writeverf3` a WRITE
/// returns — a client's `fsync` (which the kernel issues as COMMIT) succeeds, since every slates
/// write already lands FILE_SYNC. Without COMMIT the fsync would fail PROC_UNAVAIL. The matching
/// verifier is what tells the client its writes survived, so it need not resend them.
#[test]
fn a_commit_over_the_export_reports_the_write_stable() {
  let mut store = store();
  let mut vol = volume(&mut store);
  let mut bridge = VolumeBridge::new(VolumeId { bytes: [0x11; 16] }, &mut vol, &mut store);
  let cx = write_cx();
  let root_ino = bridge.root(&cx).unwrap();
  let (created, _fh) = bridge
    .create(oid(root_ino), &cx, "synced", 0o644, 0)
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
  let file_fh = slates_bridge_nfs::FileHandle {
    volume: VolumeId { bytes: [0x11; 16] },
    inode: file_ino,
    generation: 0,
  }
  .to_fh();

  // WRITE some bytes, capturing the verifier the write returns.
  let payload: &[u8] = b"durable";
  let mut wargs = XdrWriter::new();
  file_fh.encode(&mut wargs);
  wargs.u64(0); // offset
  wargs.u32(u32::try_from(payload.len()).unwrap()); // count
  wargs.u32(2); // stable = FILE_SYNC
  wargs.opaque(payload);
  let wreply = export
    .serve_nfs(NFSPROC3_WRITE, &mut XdrReader::new(wargs.as_slice()))
    .unwrap();
  let mut wr = XdrReader::new(&wreply);
  assert_eq!(wr.u32().unwrap(), Nfsstat3::Ok.wire(), "WRITE succeeded");
  assert!(!wr.bool().unwrap()); // wcc: no pre-op attributes
  PostOpAttr::decode(&mut wr).unwrap(); // post-op attributes
  wr.u32().unwrap(); // count
  wr.u32().unwrap(); // committed stability
  let write_verf: [u8; 8] = wr.fixed(8).unwrap().try_into().unwrap();

  // COMMIT the file (offset 0, count 0 = the whole file, RFC 1813 §3.3.21).
  let mut cargs = XdrWriter::new();
  file_fh.encode(&mut cargs);
  cargs.u64(0); // offset
  cargs.u32(0); // count (0 means to the end of the file)
  let creply = export
    .serve_nfs(NFSPROC3_COMMIT, &mut XdrReader::new(cargs.as_slice()))
    .unwrap();
  let mut cr = XdrReader::new(&creply);
  assert_eq!(cr.u32().unwrap(), Nfsstat3::Ok.wire(), "COMMIT succeeded");
  assert!(!cr.bool().unwrap(), "wcc: no pre-op attributes");
  let post = PostOpAttr::decode(&mut cr)
    .unwrap()
    .0
    .expect("file post attrs");
  assert_eq!(
    post.size,
    u64::try_from(payload.len()).unwrap(),
    "the committed file's size"
  );
  let commit_verf: [u8; 8] = cr.fixed(8).unwrap().try_into().unwrap();
  assert_eq!(
    commit_verf, write_verf,
    "COMMIT returns the same writeverf3 as WRITE, so the client keeps its writes"
  );
}

/// A COMMIT of the synthetic host root succeeds as a no-op: a client that `fsync`s the root mount
/// (`mount /`) gets NFS3_OK, not PROC_UNAVAIL, and the reply carries the root's verifier.
#[test]
fn a_commit_of_the_host_root_is_a_no_op() {
  let store = store();
  let set = OwnedVolumeSet::new(store);
  let mut multi = MultiExport::new(
    set,
    Principal::Uid { uid: 0 },
    Rights {
      read: true,
      write: true,
    },
  );
  let root_fh = match multi.serve_mount("/") {
    MountReply::Ok { handle, .. } => handle,
    MountReply::Err(status) => panic!("MNT / failed: {status:?}"),
  };

  let mut cargs = XdrWriter::new();
  root_fh.encode(&mut cargs);
  cargs.u64(0); // offset
  cargs.u32(0); // count
  let creply = multi
    .serve_procedure(NFSPROC3_COMMIT, &mut XdrReader::new(cargs.as_slice()))
    .expect("COMMIT of the root is answered");
  let mut cr = XdrReader::new(&creply);
  assert_eq!(
    cr.u32().unwrap(),
    Nfsstat3::Ok.wire(),
    "COMMIT of the root is a successful no-op"
  );
  assert!(!cr.bool().unwrap(), "wcc: no pre-op attributes");
  PostOpAttr::decode(&mut cr).unwrap(); // absent post-op attributes
  cr.fixed(8).unwrap(); // the verifier
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

/// SETATTR over the export changes the mode (a chmod), and a following GETATTR shows it stuck; the
/// reply's wcc carries the new attributes.
#[test]
fn a_setattr_over_the_export_chmods() {
  let mut store = store();
  let mut vol = volume(&mut store);
  let mut bridge = VolumeBridge::new(VolumeId { bytes: [0x11; 16] }, &mut vol, &mut store);
  let cx = write_cx();
  let root_ino = bridge.root(&cx).unwrap();
  let (created, _fh) = bridge.create(oid(root_ino), &cx, "cfg", 0o644, 0).unwrap();
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

  // SETATTR: set only the mode to 0o600.
  let mut args = XdrWriter::new();
  file_fh.encode(&mut args);
  args.bool(true); // mode set
  args.u32(0o600);
  args.bool(false); // uid unset
  args.bool(false); // gid unset
  args.bool(false); // size unset
  args.u32(0); // atime DONT_CHANGE
  args.u32(0); // mtime DONT_CHANGE
  args.bool(false); // guard unset
  let reply = export
    .serve_nfs(NFSPROC3_SETATTR, &mut XdrReader::new(args.as_slice()))
    .unwrap();
  let mut r = XdrReader::new(&reply);
  assert_eq!(r.u32().unwrap(), Nfsstat3::Ok.wire(), "SETATTR succeeded");
  assert!(!r.bool().unwrap(), "wcc: no pre-op attributes");
  let post = PostOpAttr::decode(&mut r).unwrap().0.expect("post attrs");
  assert_eq!(post.mode, 0o600, "the wcc reports the new mode");

  // GETATTR confirms it stuck.
  let mut ga = XdrWriter::new();
  file_fh.encode(&mut ga);
  let greply = export
    .serve_nfs(NFSPROC3_GETATTR, &mut XdrReader::new(ga.as_slice()))
    .unwrap();
  let mut gr = XdrReader::new(&greply);
  assert_eq!(gr.u32().unwrap(), Nfsstat3::Ok.wire());
  assert_eq!(Fattr3::decode(&mut gr).unwrap().mode, 0o600);
}

/// SETATTR resolves both time modes: SET_TO_CLIENT_TIME keeps the client's explicit value,
/// SET_TO_SERVER_TIME is resolved to the volume's wall clock at the seam (AC-3.10).
#[test]
fn a_setattr_sets_client_and_server_times() {
  let mut store = store();
  let mut vol = volume(&mut store);
  let mut bridge = VolumeBridge::new(VolumeId { bytes: [0x11; 16] }, &mut vol, &mut store);
  let cx = write_cx();
  let root_ino = bridge.root(&cx).unwrap();
  let (created, _fh) = bridge.create(oid(root_ino), &cx, "t", 0o644, 0).unwrap();
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

  // SETATTR: atime = client 123.000000456, mtime = server time.
  let mut args = XdrWriter::new();
  file_fh.encode(&mut args);
  args.bool(false); // mode
  args.bool(false); // uid
  args.bool(false); // gid
  args.bool(false); // size
  args.u32(2); // atime SET_TO_CLIENT_TIME
  args.u32(123); // atime seconds
  args.u32(456); // atime nseconds
  args.u32(1); // mtime SET_TO_SERVER_TIME
  args.bool(false); // guard
  let reply = export
    .serve_nfs(NFSPROC3_SETATTR, &mut XdrReader::new(args.as_slice()))
    .unwrap();
  let mut r = XdrReader::new(&reply);
  assert_eq!(r.u32().unwrap(), Nfsstat3::Ok.wire(), "SETATTR succeeded");
  assert!(!r.bool().unwrap());
  let post = PostOpAttr::decode(&mut r).unwrap().0.expect("post attrs");
  assert_eq!(post.atime.seconds, 123, "atime is the client's value");
  assert_eq!(post.atime.nseconds, 456);
  assert!(
    post.mtime.seconds > 0,
    "mtime is resolved to the server's wall clock, not zero"
  );
}

/// A SETATTR whose guard ctime does not match the object's is refused NFS3ERR_NOT_SYNC and applies
/// nothing — the compare-and-set protects against a lost update on stale client state.
#[test]
fn a_setattr_guard_mismatch_is_not_sync() {
  let mut store = store();
  let mut vol = volume(&mut store);
  let mut bridge = VolumeBridge::new(VolumeId { bytes: [0x11; 16] }, &mut vol, &mut store);
  let cx = write_cx();
  let root_ino = bridge.root(&cx).unwrap();
  let (created, _fh) = bridge.create(oid(root_ino), &cx, "g", 0o644, 0).unwrap();
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

  // SETATTR mode 0o600 with a guard ctime of 1ns, which cannot match the real create ctime.
  let mut args = XdrWriter::new();
  file_fh.encode(&mut args);
  args.bool(true); // mode set
  args.u32(0o600);
  args.bool(false); // uid
  args.bool(false); // gid
  args.bool(false); // size
  args.u32(0); // atime DONT_CHANGE
  args.u32(0); // mtime DONT_CHANGE
  args.bool(true); // guard set
  args.u32(0); // guard ctime seconds
  args.u32(1); // guard ctime nseconds (1ns since the epoch — cannot match)
  let reply = export
    .serve_nfs(NFSPROC3_SETATTR, &mut XdrReader::new(args.as_slice()))
    .unwrap();
  assert_eq!(
    XdrReader::new(&reply).u32().unwrap(),
    Nfsstat3::NotSync.wire(),
    "a guard mismatch is NOT_SYNC"
  );

  // The mode did not change.
  let mut ga = XdrWriter::new();
  file_fh.encode(&mut ga);
  let greply = export
    .serve_nfs(NFSPROC3_GETATTR, &mut XdrReader::new(ga.as_slice()))
    .unwrap();
  let mut gr = XdrReader::new(&greply);
  assert_eq!(gr.u32().unwrap(), Nfsstat3::Ok.wire());
  assert_eq!(
    Fattr3::decode(&mut gr).unwrap().mode,
    0o644,
    "the refused SETATTR applied nothing"
  );
}

/// A helper: build an export over a fresh volume and return the pieces the create/mkdir tests need
/// — the bridge is created inside so borrows stay simple; the export's root handle comes from MNT.
fn root_handle(export: &mut Export<'_>) -> Nfsfh3 {
  match export.mnt("/") {
    MountReply::Ok { handle, .. } => handle,
    MountReply::Err(status) => panic!("MNT failed: {status:?}"),
  }
}

/// CREATE over the export makes a regular file: the reply carries the new file's handle and
/// attributes (the requested mode, a regular file), and a following LOOKUP resolves the name.
#[test]
fn a_create_over_the_export_makes_a_file() {
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
  let root_fh = root_handle(&mut export);

  // CREATE "new" GUARDED with mode 0o600.
  let mut args = XdrWriter::new();
  root_fh.encode(&mut args);
  args.opaque("new".as_bytes());
  args.u32(1); // createmode GUARDED
  args.bool(true); // sattr3 mode set
  args.u32(0o600);
  args.bool(false); // uid
  args.bool(false); // gid
  args.bool(false); // size
  args.u32(0); // atime DONT_CHANGE
  args.u32(0); // mtime DONT_CHANGE
  let reply = export
    .serve_nfs(NFSPROC3_CREATE, &mut XdrReader::new(args.as_slice()))
    .unwrap();
  let mut r = XdrReader::new(&reply);
  assert_eq!(r.u32().unwrap(), Nfsstat3::Ok.wire(), "CREATE succeeded");
  assert!(r.bool().unwrap(), "a file handle follows");
  let _fh = Nfsfh3::decode(&mut r).unwrap();
  let attr = PostOpAttr::decode(&mut r).unwrap().0.expect("object attrs");
  assert_eq!(attr.kind, Ftype3::Reg, "a regular file");
  assert_eq!(attr.mode, 0o600, "the requested mode");

  // LOOKUP resolves the new name.
  let mut la = XdrWriter::new();
  root_fh.encode(&mut la);
  la.opaque("new".as_bytes());
  let lreply = export.lookup(&mut XdrReader::new(la.as_slice()));
  assert_eq!(
    XdrReader::new(&lreply).u32().unwrap(),
    Nfsstat3::Ok.wire(),
    "the created name resolves"
  );
}

/// A GUARDED CREATE onto an existing name is NFS3ERR_EXIST; an UNCHECKED CREATE onto an existing
/// name succeeds (opening it), so a client's `open(O_CREAT)` without `O_EXCL` is idempotent.
#[test]
fn guarded_and_unchecked_create_differ_on_an_existing_name() {
  let mut store = store();
  let mut vol = volume(&mut store);
  let mut bridge = VolumeBridge::new(VolumeId { bytes: [0x11; 16] }, &mut vol, &mut store);
  let cx = write_cx();
  let root_ino = bridge.root(&cx).unwrap();
  bridge.create(oid(root_ino), &cx, "dup", 0o644, 0).unwrap();

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
  let root_fh = root_handle(&mut export);

  // GUARDED onto "dup" (exists) -> EXIST.
  let mut ga = XdrWriter::new();
  root_fh.encode(&mut ga);
  ga.opaque("dup".as_bytes());
  ga.u32(1); // GUARDED
  ga.bool(true);
  ga.u32(0o644);
  ga.bool(false);
  ga.bool(false);
  ga.bool(false);
  ga.u32(0);
  ga.u32(0);
  let greply = export
    .serve_nfs(NFSPROC3_CREATE, &mut XdrReader::new(ga.as_slice()))
    .unwrap();
  assert_eq!(
    XdrReader::new(&greply).u32().unwrap(),
    Nfsstat3::Exist.wire(),
    "GUARDED onto an existing name is EXIST"
  );

  // UNCHECKED onto "dup" (exists) -> OK.
  let mut ua = XdrWriter::new();
  root_fh.encode(&mut ua);
  ua.opaque("dup".as_bytes());
  ua.u32(0); // UNCHECKED
  ua.bool(false); // mode unset
  ua.bool(false);
  ua.bool(false);
  ua.bool(false);
  ua.u32(0);
  ua.u32(0);
  let ureply = export
    .serve_nfs(NFSPROC3_CREATE, &mut XdrReader::new(ua.as_slice()))
    .unwrap();
  assert_eq!(
    XdrReader::new(&ureply).u32().unwrap(),
    Nfsstat3::Ok.wire(),
    "UNCHECKED onto an existing name succeeds"
  );
}

/// MKDIR over the export makes a directory: the reply carries its handle and attributes (a
/// directory, the requested mode), and a following LOOKUP resolves the name.
#[test]
fn a_mkdir_over_the_export_makes_a_directory() {
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
  let root_fh = root_handle(&mut export);

  // MKDIR "d" with mode 0o750.
  let mut args = XdrWriter::new();
  root_fh.encode(&mut args);
  args.opaque("d".as_bytes());
  args.bool(true); // sattr3 mode set
  args.u32(0o750);
  args.bool(false); // uid
  args.bool(false); // gid
  args.bool(false); // size
  args.u32(0); // atime DONT_CHANGE
  args.u32(0); // mtime DONT_CHANGE
  let reply = export
    .serve_nfs(NFSPROC3_MKDIR, &mut XdrReader::new(args.as_slice()))
    .unwrap();
  let mut r = XdrReader::new(&reply);
  assert_eq!(r.u32().unwrap(), Nfsstat3::Ok.wire(), "MKDIR succeeded");
  assert!(r.bool().unwrap(), "a handle follows");
  let _fh = Nfsfh3::decode(&mut r).unwrap();
  let attr = PostOpAttr::decode(&mut r).unwrap().0.expect("object attrs");
  assert_eq!(attr.kind, Ftype3::Dir, "a directory");
  assert_eq!(attr.mode, 0o750, "the requested mode");

  let mut la = XdrWriter::new();
  root_fh.encode(&mut la);
  la.opaque("d".as_bytes());
  let lreply = export.lookup(&mut XdrReader::new(la.as_slice()));
  assert_eq!(
    XdrReader::new(&lreply).u32().unwrap(),
    Nfsstat3::Ok.wire(),
    "the created directory resolves"
  );
}

/// SYMLINK over the export makes a symbolic link: the reply carries the link's handle and
/// attributes (a symlink whose size is the target length), and a following LOOKUP resolves it.
#[test]
fn a_symlink_over_the_export_makes_a_link() {
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
  let root_fh = root_handle(&mut export);

  let target = "to/the/target";
  let mut args = XdrWriter::new();
  root_fh.encode(&mut args);
  args.opaque("link".as_bytes()); // name
  // sattr3: nothing set (a symlink's mode is fixed), times DONT_CHANGE.
  args.bool(false); // mode
  args.bool(false); // uid
  args.bool(false); // gid
  args.bool(false); // size
  args.u32(0); // atime
  args.u32(0); // mtime
  args.opaque(target.as_bytes()); // symlink_data (nfspath3)
  let reply = export
    .serve_nfs(NFSPROC3_SYMLINK, &mut XdrReader::new(args.as_slice()))
    .unwrap();
  let mut r = XdrReader::new(&reply);
  assert_eq!(r.u32().unwrap(), Nfsstat3::Ok.wire(), "SYMLINK succeeded");
  assert!(r.bool().unwrap(), "a handle follows");
  let _fh = Nfsfh3::decode(&mut r).unwrap();
  let attr = PostOpAttr::decode(&mut r).unwrap().0.expect("object attrs");
  assert_eq!(attr.kind, Ftype3::Lnk, "a symbolic link");
  assert_eq!(
    attr.size,
    u64::try_from(target.len()).unwrap(),
    "the size is the target length"
  );

  let mut la = XdrWriter::new();
  root_fh.encode(&mut la);
  la.opaque("link".as_bytes());
  let lreply = export.lookup(&mut XdrReader::new(la.as_slice()));
  assert_eq!(
    XdrReader::new(&lreply).u32().unwrap(),
    Nfsstat3::Ok.wire(),
    "the created link resolves"
  );
}

/// READLINK returns a symlink's target over the export, and a READLINK of a non-symlink is
/// NFS3ERR_INVAL — the target round-trips and a directory is refused, never mis-read.
#[test]
fn a_readlink_returns_the_target_and_refuses_a_non_symlink() {
  let mut store = store();
  let mut vol = volume(&mut store);
  let mut bridge = VolumeBridge::new(VolumeId { bytes: [0x11; 16] }, &mut vol, &mut store);
  let cx = write_cx();
  let root_ino = bridge.root(&cx).unwrap();
  let made = bridge
    .symlink(oid(root_ino), &cx, "ln", "the/target/path")
    .unwrap();
  let link_ino = made.ino;

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

  // READLINK the symlink: the target round-trips.
  let link_fh = slates_bridge_nfs::FileHandle {
    volume: VolumeId { bytes: [0x11; 16] },
    inode: link_ino,
    generation: 0,
  }
  .to_fh();
  let mut args = XdrWriter::new();
  link_fh.encode(&mut args);
  let reply = export
    .serve_nfs(NFSPROC3_READLINK, &mut XdrReader::new(args.as_slice()))
    .unwrap();
  let mut r = XdrReader::new(&reply);
  assert_eq!(r.u32().unwrap(), Nfsstat3::Ok.wire(), "READLINK succeeded");
  PostOpAttr::decode(&mut r).unwrap();
  let target = r.opaque(4096).unwrap();
  assert_eq!(target, b"the/target/path", "the symlink target round-trips");

  // READLINK a directory (the root) is INVAL.
  let root_fh = slates_bridge_nfs::FileHandle {
    volume: VolumeId { bytes: [0x11; 16] },
    inode: root_ino,
    generation: 0,
  }
  .to_fh();
  let mut da = XdrWriter::new();
  root_fh.encode(&mut da);
  let dreply = export
    .serve_nfs(NFSPROC3_READLINK, &mut XdrReader::new(da.as_slice()))
    .unwrap();
  assert_eq!(
    XdrReader::new(&dreply).u32().unwrap(),
    Nfsstat3::Inval.wire(),
    "READLINK of a non-symlink is INVAL"
  );
}

/// LINK over the export creates a second name for an existing file — a hard link: after linking "a"
/// as "b", both names LOOKUP to the same object (same fileid) and the file's link count is two, so a
/// client's `ln a b` works over the mount instead of failing PROC_UNAVAIL.
#[test]
fn a_link_over_the_export_makes_a_second_name() {
  let mut store = store();
  let mut vol = volume(&mut store);
  let mut bridge = VolumeBridge::new(VolumeId { bytes: [0x11; 16] }, &mut vol, &mut store);
  let cx = write_cx();
  let root_ino = bridge.root(&cx).unwrap();
  let (created, _fh) = bridge.create(oid(root_ino), &cx, "a", 0o644, 0).unwrap();
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
  let root_fh = root_handle(&mut export);
  let file_fh = slates_bridge_nfs::FileHandle {
    volume: VolumeId { bytes: [0x11; 16] },
    inode: file_ino,
    generation: 0,
  }
  .to_fh();

  // LINK3args: the existing file handle, then diropargs3 (the directory handle, the new name).
  let mut args = XdrWriter::new();
  file_fh.encode(&mut args);
  root_fh.encode(&mut args);
  args.opaque("b".as_bytes());
  let reply = export
    .serve_nfs(NFSPROC3_LINK, &mut XdrReader::new(args.as_slice()))
    .unwrap();
  let mut r = XdrReader::new(&reply);
  assert_eq!(r.u32().unwrap(), Nfsstat3::Ok.wire(), "LINK succeeded");
  let file_attr = PostOpAttr::decode(&mut r)
    .unwrap()
    .0
    .expect("file post attrs");
  assert_eq!(
    file_attr.fileid, file_ino,
    "the link names the linked-to object"
  );
  assert_eq!(
    file_attr.nlink, 2,
    "the link count is two after a hard link"
  );

  // Both names now resolve to the same object.
  for name in ["a", "b"] {
    let mut la = XdrWriter::new();
    root_fh.encode(&mut la);
    la.opaque(name.as_bytes());
    let lreply = export.lookup(&mut XdrReader::new(la.as_slice()));
    let mut lr = XdrReader::new(&lreply);
    assert_eq!(lr.u32().unwrap(), Nfsstat3::Ok.wire(), "the name resolves");
    let _fh = Nfsfh3::decode(&mut lr).unwrap();
    let attr = PostOpAttr::decode(&mut lr)
      .unwrap()
      .0
      .expect("object attrs");
    assert_eq!(
      attr.fileid, file_ino,
      "both names name the same object (a hard link)"
    );
  }
}

/// MKNOD over the export is refused NFS3ERR_NOTSUPP — a typed refusal, not PROC_UNAVAIL (which a
/// truly-unhandled procedure gives) — because a RAM copy-on-write filesystem does not create device,
/// FIFO or socket nodes. The reply is framed as the directory's wcc_data, so the stream stays synced.
#[test]
fn a_mknod_over_the_export_is_notsupp() {
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
  let root_fh = root_handle(&mut export);

  // MKNOD3args: the directory (diropargs3), then mknoddata3 — here a block device with specdata.
  let mut args = XdrWriter::new();
  root_fh.encode(&mut args);
  args.opaque("dev".as_bytes()); // name
  args.u32(3); // ftype3 NF3BLK
  args.bool(false); // sattr3 mode unset
  args.bool(false); // uid
  args.bool(false); // gid
  args.bool(false); // size
  args.u32(0); // atime DONT_CHANGE
  args.u32(0); // mtime DONT_CHANGE
  args.u32(1); // specdata3 major
  args.u32(2); // specdata3 minor
  let reply = export
    .serve_nfs(NFSPROC3_MKNOD, &mut XdrReader::new(args.as_slice()))
    .expect("MKNOD is answered, not PROC_UNAVAIL");
  let mut r = XdrReader::new(&reply);
  assert_eq!(
    r.u32().unwrap(),
    Nfsstat3::Notsupp.wire(),
    "MKNOD is a typed NOTSUPP refusal, not PROC_UNAVAIL"
  );
  assert!(!r.bool().unwrap(), "wcc: no pre-op attributes");
  PostOpAttr::decode(&mut r).unwrap(); // the directory's post-op attributes
}

/// PATHCONF over the export reports the volume's POSIX limits from its own policy: an exact-name
/// volume is case-sensitive (`case_insensitive` false), the link maximum is the u32 counter's range,
/// names are capped at NAME_MAX and refused rather than truncated (`no_trunc`), ownership changes are
/// unrestricted (`chown_restricted` false), and case is preserved — so a client's `pathconf` gets
/// real answers instead of PROC_UNAVAIL and conservative fallbacks.
#[test]
fn pathconf_reports_the_volume_limits() {
  let mut store = store();
  let mut vol = volume(&mut store); // NameEquivalence::Exact
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
  let root_fh = root_handle(&mut export);

  let mut args = XdrWriter::new();
  root_fh.encode(&mut args);
  let reply = export
    .serve_nfs(NFSPROC3_PATHCONF, &mut XdrReader::new(args.as_slice()))
    .expect("PATHCONF is answered, not PROC_UNAVAIL");
  let mut r = XdrReader::new(&reply);
  assert_eq!(r.u32().unwrap(), Nfsstat3::Ok.wire(), "PATHCONF succeeded");
  PostOpAttr::decode(&mut r).unwrap(); // the object's post-op attributes
  assert_eq!(
    r.u32().unwrap(),
    u32::MAX,
    "linkmax is the u32 counter's range"
  );
  assert_eq!(r.u32().unwrap(), 255, "name_max is NAME_MAX");
  assert!(r.bool().unwrap(), "no_trunc: an over-long name is refused");
  assert!(
    !r.bool().unwrap(),
    "chown is not restricted to the superuser"
  );
  assert!(!r.bool().unwrap(), "an exact-name volume is case-sensitive");
  assert!(r.bool().unwrap(), "case is preserved");
}

/// PATHCONF reflects a case-folding volume: a `Fold`-policy volume reports `case_insensitive` true,
/// so a client learns the volume's real case behaviour from its policy rather than assuming
/// case-sensitivity — the value is the inverse of the volume's `NameEquivalence`, not a fixed guess.
#[test]
fn pathconf_reflects_a_case_folding_volume() {
  let mut store = store();
  let mut vol = volume_with_names(&mut store, NameEquivalence::Fold);
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
  let root_fh = root_handle(&mut export);

  let mut args = XdrWriter::new();
  root_fh.encode(&mut args);
  let reply = export
    .serve_nfs(NFSPROC3_PATHCONF, &mut XdrReader::new(args.as_slice()))
    .expect("PATHCONF is answered");
  let mut r = XdrReader::new(&reply);
  assert_eq!(r.u32().unwrap(), Nfsstat3::Ok.wire());
  PostOpAttr::decode(&mut r).unwrap();
  r.u32().unwrap(); // linkmax
  r.u32().unwrap(); // name_max
  r.bool().unwrap(); // no_trunc
  r.bool().unwrap(); // chown_restricted
  assert!(
    r.bool().unwrap(),
    "a case-folding volume reports case_insensitive true"
  );
  assert!(r.bool().unwrap(), "case is still preserved");
}

/// Parses a READDIR reply into (names, last cookie, cookieverf, eof), for the listing tests. The
/// cookieverf is carried so a continuation echoes it, the way a real client does.
fn parse_readdir(reply: &[u8]) -> (Vec<String>, u64, [u8; 8], bool) {
  let mut r = XdrReader::new(reply);
  assert_eq!(r.u32().unwrap(), Nfsstat3::Ok.wire(), "READDIR succeeded");
  PostOpAttr::decode(&mut r).unwrap(); // dir attributes
  let verf: [u8; 8] = r.fixed(8).unwrap().try_into().unwrap();
  let mut names = Vec::new();
  let mut last_cookie = 0;
  while r.bool().unwrap() {
    let _fileid = r.u64().unwrap();
    let name = r.string(255).unwrap().to_owned();
    let cookie = r.u64().unwrap();
    names.push(name);
    last_cookie = cookie;
  }
  let eof = r.bool().unwrap();
  (names, last_cookie, verf, eof)
}

/// READDIR lists a directory's entries and paginates: a generous count returns them all with eof,
/// a small count returns a partial list with a resume cookie, and resuming from it returns the rest;
/// a count too small for even one entry is NFS3ERR_TOOSMALL.
#[test]
fn readdir_lists_entries_and_paginates() {
  let mut store = store();
  let mut vol = volume(&mut store);
  let mut bridge = VolumeBridge::new(VolumeId { bytes: [0x11; 16] }, &mut vol, &mut store);
  let cx = write_cx();
  let root_ino = bridge.root(&cx).unwrap();
  for name in ["a", "b", "c"] {
    bridge.create(oid(root_ino), &cx, name, 0o644, 0).unwrap();
  }

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
  let root_fh = root_handle(&mut export);

  // A READDIR echoing the cookieverf the previous reply gave, the way a client continues a listing.
  let readdir = |export: &mut Export<'_>, cookie: u64, verf: [u8; 8], count: u32| -> Vec<u8> {
    let mut args = XdrWriter::new();
    root_fh.encode(&mut args);
    args.u64(cookie);
    args.fixed(&verf); // cookieverf
    args.u32(count);
    export
      .serve_nfs(NFSPROC3_READDIR, &mut XdrReader::new(args.as_slice()))
      .unwrap()
  };

  // A generous count returns all entries with eof. The first call's verf is ignored (cookie 0).
  let (mut names, _c, verf, eof) = parse_readdir(&readdir(&mut export, 0, [0u8; 8], 8192));
  names.sort();
  assert_eq!(
    names,
    vec![".", "..", "a", "b", "c"],
    "all entries (with the synthesized . and ..) with a generous count"
  );
  assert!(eof, "the whole directory fit, so eof is set");

  // A small count paginates: a partial list without eof, then the rest resumed from the cookie and
  // the cookieverf the first reply returned.
  let (first, cookie, verf, eof1) = parse_readdir(&readdir(&mut export, 0, verf, 170));
  assert!(!eof1, "a partial listing is not at eof");
  assert!(
    !first.is_empty() && first.len() < 5,
    "a partial listing has some but not all entries"
  );
  let (rest, _c2, _v2, eof2) = parse_readdir(&readdir(&mut export, cookie, verf, 8192));
  assert!(eof2, "the resumed listing reaches eof");
  let mut all: Vec<String> = first.into_iter().chain(rest).collect();
  all.sort();
  assert_eq!(
    all,
    vec![".", "..", "a", "b", "c"],
    "pagination returns every entry once"
  );

  // A count too small for even one entry is TOOSMALL.
  let tiny = readdir(&mut export, 0, [0u8; 8], 8);
  assert_eq!(
    XdrReader::new(&tiny).u32().unwrap(),
    Nfsstat3::Toosmall.wire(),
    "a count too small for one entry is TOOSMALL"
  );
}

/// A READDIR continuation whose cookieverf no longer matches the directory's — because the
/// directory changed since the listing began — is refused NFS3ERR_BAD_COOKIE, so a client never
/// resumes a listing against a mutated directory and silently skips or repeats entries.
#[test]
fn a_readdir_continuation_after_the_directory_changes_is_bad_cookie() {
  let mut store = store();
  let mut vol = volume(&mut store);
  let mut bridge = VolumeBridge::new(VolumeId { bytes: [0x11; 16] }, &mut vol, &mut store);
  let cx = write_cx();
  let root_ino = bridge.root(&cx).unwrap();
  for name in ["a", "b", "c"] {
    bridge.create(oid(root_ino), &cx, name, 0o644, 0).unwrap();
  }

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
  let root_fh = root_handle(&mut export);

  let readdir = |export: &mut Export<'_>, cookie: u64, verf: [u8; 8], count: u32| -> Vec<u8> {
    let mut args = XdrWriter::new();
    root_fh.encode(&mut args);
    args.u64(cookie);
    args.fixed(&verf);
    args.u32(count);
    export
      .serve_nfs(NFSPROC3_READDIR, &mut XdrReader::new(args.as_slice()))
      .unwrap()
  };

  // A first partial listing yields a resume cookie and the directory's cookieverf.
  let (first, cookie, verf, eof) = parse_readdir(&readdir(&mut export, 0, [0u8; 8], 170));
  assert!(!eof, "a partial listing is not at eof");
  assert!(!first.is_empty(), "the first page has entries");

  // While the directory is unchanged, the echoed verf resumes the listing.
  let ok = readdir(&mut export, cookie, verf, 8192);
  assert_eq!(
    XdrReader::new(&ok).u32().unwrap(),
    Nfsstat3::Ok.wire(),
    "an unchanged directory resumes with the echoed verf"
  );

  // Change the directory through the export (a CREATE advances its change time), then resume with
  // the now-stale verf: the continuation is refused BAD_COOKIE.
  let mut ca = XdrWriter::new();
  root_fh.encode(&mut ca);
  ca.opaque("d".as_bytes());
  ca.u32(1); // createmode GUARDED
  ca.bool(false); // no sattr fields
  ca.bool(false);
  ca.bool(false);
  ca.bool(false);
  ca.u32(0);
  ca.u32(0);
  let creply = export
    .serve_nfs(NFSPROC3_CREATE, &mut XdrReader::new(ca.as_slice()))
    .unwrap();
  assert_eq!(
    XdrReader::new(&creply).u32().unwrap(),
    Nfsstat3::Ok.wire(),
    "the CREATE changing the directory succeeds"
  );

  let stale = readdir(&mut export, cookie, verf, 8192);
  assert_eq!(
    XdrReader::new(&stale).u32().unwrap(),
    Nfsstat3::BadCookie.wire(),
    "a continuation against the changed directory is BAD_COOKIE"
  );
}

/// Parses a READDIRPLUS reply into (names, handles), asserting each entry carries its attributes
/// (`post_op_attr` present) and a handle — the plus data that saves the client a per-entry lookup.
fn parse_readdirplus(reply: &[u8]) -> (Vec<String>, Vec<Nfsfh3>) {
  let mut r = XdrReader::new(reply);
  assert_eq!(
    r.u32().unwrap(),
    Nfsstat3::Ok.wire(),
    "READDIRPLUS succeeded"
  );
  PostOpAttr::decode(&mut r).unwrap(); // directory attributes
  r.fixed(8).unwrap(); // cookieverf
  let mut names = Vec::new();
  let mut handles = Vec::new();
  while r.bool().unwrap() {
    let _fileid = r.u64().unwrap();
    names.push(r.string(255).unwrap().to_owned());
    let _cookie = r.u64().unwrap();
    PostOpAttr::decode(&mut r)
      .unwrap()
      .0
      .expect("each entry carries its attributes");
    assert!(r.bool().unwrap(), "each entry carries a handle");
    handles.push(Nfsfh3::decode(&mut r).unwrap());
  }
  assert!(r.bool().unwrap(), "the whole directory fit, so eof is set");
  (names, handles)
}

/// READDIRPLUS lists a directory with each entry's attributes and file handle, so a client needs
/// no follow-up GETATTR/LOOKUP per entry: every entry carries its attributes and a handle, and a
/// listed handle resolves to the same object.
#[test]
fn readdirplus_lists_entries_with_attributes_and_handles() {
  let mut store = store();
  let mut vol = volume(&mut store);
  let mut bridge = VolumeBridge::new(VolumeId { bytes: [0x11; 16] }, &mut vol, &mut store);
  let cx = write_cx();
  let root_ino = bridge.root(&cx).unwrap();
  for name in ["a", "b", "c"] {
    bridge.create(oid(root_ino), &cx, name, 0o644, 0).unwrap();
  }

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
  let root_fh = root_handle(&mut export);

  // READDIRPLUS args: dir handle, cookie, cookieverf, dircount, maxcount.
  let mut args = XdrWriter::new();
  root_fh.encode(&mut args);
  args.u64(0);
  args.fixed(&[0u8; 8]);
  args.u32(8192); // dircount (advisory)
  args.u32(8192); // maxcount
  let reply = export
    .serve_nfs(NFSPROC3_READDIRPLUS, &mut XdrReader::new(args.as_slice()))
    .unwrap();

  let (mut names, handles) = parse_readdirplus(&reply);
  names.sort();
  assert_eq!(
    names,
    vec![".", "..", "a", "b", "c"],
    "every entry (with . and ..) is listed"
  );
  assert_eq!(handles.len(), 5, "every entry carries a handle");

  // The first entry's handle is "." — it resolves to the directory itself.
  let mut ga = XdrWriter::new();
  handles[0].encode(&mut ga);
  let greply = export
    .serve_nfs(NFSPROC3_GETATTR, &mut XdrReader::new(ga.as_slice()))
    .unwrap();
  let mut gr = XdrReader::new(&greply);
  assert_eq!(
    gr.u32().unwrap(),
    Nfsstat3::Ok.wire(),
    "a listed handle resolves"
  );
  assert_eq!(
    Fattr3::decode(&mut gr).unwrap().kind,
    Ftype3::Dir,
    "the '.' entry names the directory"
  );
}

/// Every served NFS procedure refuses hostile or truncated input with a typed status and never
/// panics or over-allocates (Part 4: hostile-input tests on every parser of external bytes).
/// `serve_nfs` is the network entry point — a panic or an unbounded allocation there is a denial
/// of service — so each procedure is driven with an empty buffer, a garbage prefix, and a buffer
/// whose leading opaque length is `u32::MAX` (which the XDR reader must refuse before allocating).
#[test]
fn every_procedure_refuses_hostile_input_without_panicking() {
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

  // Every procedure that decodes arguments (NULL takes none and is excluded — it has no parser).
  let procedures = [
    NFSPROC3_GETATTR,
    NFSPROC3_SETATTR,
    NFSPROC3_LOOKUP,
    NFSPROC3_ACCESS,
    NFSPROC3_READLINK,
    NFSPROC3_READ,
    NFSPROC3_WRITE,
    NFSPROC3_CREATE,
    NFSPROC3_MKDIR,
    NFSPROC3_SYMLINK,
    NFSPROC3_REMOVE,
    NFSPROC3_RMDIR,
    NFSPROC3_RENAME,
    NFSPROC3_READDIR,
    NFSPROC3_READDIRPLUS,
    NFSPROC3_FSSTAT,
    NFSPROC3_FSINFO,
  ];

  // A buffer whose first field claims a u32::MAX-byte opaque (the file handle): the reader must
  // refuse it (length past the handle cap and past the buffer) before allocating.
  let hostile_length = {
    let mut w = XdrWriter::new();
    w.u32(u32::MAX);
    w.into_bytes()
  };
  let hostile_inputs: [&[u8]; 4] = [
    &[],                       // empty
    &[0xff, 0x13, 0x37],       // a garbage prefix, too short for even a length word
    &[0x00, 0x00, 0x00, 0x40], // a well-formed length (64) with no bytes following
    &hostile_length,           // a u32::MAX opaque length
  ];

  for &procedure in &procedures {
    for input in &hostile_inputs {
      // Reaching past this call at all proves no panic. A produced reply must be a typed refusal:
      // its leading status word decodes and is never NFS3_OK, since the arguments never parsed.
      let reply = export.serve_nfs(procedure, &mut XdrReader::new(input));
      if let Some(bytes) = reply {
        assert!(
          !bytes.is_empty(),
          "procedure {procedure} produced an empty reply for hostile input"
        );
        let status = XdrReader::new(&bytes).u32().unwrap();
        assert_ne!(
          status,
          Nfsstat3::Ok.wire(),
          "procedure {procedure} accepted hostile input as OK"
        );
      }
    }
  }
}
