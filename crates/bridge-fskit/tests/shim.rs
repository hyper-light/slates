#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! The FSKit shim codec, exercised by use (R5): requests round-trip through their bytes, hostile
//! messages are typed refusals, and `serve` drives the read path against a real `VolumeBridge` over an
//! in-memory scratch volume — the same seam the FUSE and NFS bridges dispatch onto. No socket, no
//! mount, no Swift: the Rust half of the FSKit bridge is confirmable on any host, exactly as the NFS
//! wire codec was built and confirmed first.

use slates_base::OsHost;
use slates_bridge_core::{Attachments, Bridge, ObjectId, OpContext, Rights, View, VolumeBridge};
use slates_bridge_fskit::mount::MountSession;
use slates_bridge_fskit::{ShimRequest, ShimWireError, serve};
use slates_db::catalog::{Principal, VolumeId};
use slates_mem::arena::ChunkArena;
use slates_mem::region::Region;
use slates_vfs::base::BaseConfig;
use slates_vfs::clock::HostClock;
use slates_vfs::host::HostFs;
use slates_vfs::names::NameEquivalence;
use slates_vfs::quota::Quota;
use slates_vfs::volume::{Store, StoreConfig, Volume, VolumeConfig};

const PAGE: usize = 4096;
const REGION_PAGES: usize = 4096;
// The reply status bytes, mirrored from the codec's wire (a test reads them directly).
const STATUS_OK: u8 = 0;
const STATUS_ERR: u8 = 1;
// The ShimError tag a missing object maps to (NotFound is the first variant).
const SHIM_NOT_FOUND: u8 = 0;

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

fn oid(ino: u64) -> ObjectId {
  ObjectId::new(ino, 0)
}

/// Each request round-trips through its canonical bytes unchanged — the wire is stable.
#[test]
fn a_request_round_trips_through_its_bytes() {
  let requests = [
    ShimRequest::Lookup {
      parent: oid(1),
      name: "hello".to_owned(),
    },
    ShimRequest::GetAttr { object: oid(42) },
    ShimRequest::Read {
      object: oid(42),
      offset: 4096,
      size: 512,
    },
    ShimRequest::Write {
      object: oid(42),
      offset: 4096,
      data: b"payload".to_vec(),
    },
    ShimRequest::OpenDir { object: oid(1) },
    ShimRequest::ReadDir {
      object: oid(1),
      fh: 7,
      offset: 3,
    },
    ShimRequest::Release {
      object: oid(1),
      fh: 7,
    },
    ShimRequest::Create {
      parent: oid(1),
      name: "f".to_owned(),
      mode: 0o644,
      flags: 2,
    },
    ShimRequest::Mkdir {
      parent: oid(1),
      name: "d".to_owned(),
      mode: 0o755,
    },
    ShimRequest::Unlink {
      parent: oid(1),
      name: "f".to_owned(),
    },
    ShimRequest::Rmdir {
      parent: oid(1),
      name: "d".to_owned(),
    },
    ShimRequest::Open {
      object: oid(9),
      flags: 2,
    },
    ShimRequest::Flush {
      object: oid(9),
      fh: 5,
    },
    ShimRequest::Symlink {
      parent: oid(1),
      name: "link".to_owned(),
      target: "/a/b/c".to_owned(),
    },
    ShimRequest::Readlink { object: oid(9) },
    ShimRequest::Link {
      target: oid(9),
      new_parent: oid(1),
      new_name: "alias".to_owned(),
    },
    ShimRequest::Rename {
      old_parent: oid(1),
      new_parent: oid(2),
      old_name: "from".to_owned(),
      new_name: "to".to_owned(),
      no_replace: true,
      exchange: false,
    },
    ShimRequest::Reference { object: oid(9) },
    ShimRequest::Forget {
      object: oid(9),
      nlookup: 3,
    },
    // SetAttr with every field present: the mask carries all six bits and the values follow in order.
    ShimRequest::SetAttr {
      object: oid(42),
      size: Some(4096),
      mode: Some(0o600),
      uid: Some(501),
      gid: Some(20),
      atime: Some(1_700_000_000_000_000_000),
      mtime: Some(-1),
    },
    // A partial set: only the mode and the modification time, so the mask carries two bits and only
    // those two values are on the wire; round-trip must preserve which fields were absent.
    ShimRequest::SetAttr {
      object: oid(7),
      size: None,
      mode: Some(0o755),
      uid: None,
      gid: None,
      atime: None,
      mtime: Some(123),
    },
    // The empty set: a zero mask and no values (a no-op setattr the shim still frames well).
    ShimRequest::SetAttr {
      object: oid(1),
      size: None,
      mode: None,
      uid: None,
      gid: None,
      atime: None,
      mtime: None,
    },
    ShimRequest::Root,
  ];
  for request in requests {
    let bytes = request.encode();
    assert_eq!(
      ShimRequest::decode(&bytes),
      Ok(request),
      "round-trip is identity"
    );
  }
}

/// A truncated, unknown, over-claiming or trailing-byte message is a typed refusal — never a panic or
/// an over-read (the message crossed the sandboxed extension boundary).
#[test]
fn a_hostile_message_is_a_typed_refusal() {
  // Empty: no operation tag.
  assert_eq!(ShimRequest::decode(&[]), Err(ShimWireError::Truncated));
  // An unknown operation tag.
  assert_eq!(
    ShimRequest::decode(&[0xff]),
    Err(ShimWireError::UnknownOp { tag: 0xff })
  );
  // GetAttr with a truncated object (needs 16 bytes, has 4).
  assert_eq!(
    ShimRequest::decode(&[2, 1, 2, 3, 4]),
    Err(ShimWireError::Truncated)
  );
  // A well-formed GetAttr with extra trailing bytes.
  let mut trailing = ShimRequest::GetAttr { object: oid(1) }.encode();
  trailing.push(0);
  assert_eq!(
    ShimRequest::decode(&trailing),
    Err(ShimWireError::TrailingBytes)
  );
  // A Lookup whose name length claims more than the 255-byte cap.
  let mut oversized = vec![1u8];
  oversized.extend_from_slice(&1u64.to_le_bytes()); // parent inode
  oversized.extend_from_slice(&0u64.to_le_bytes()); // parent generation
  oversized.extend_from_slice(&300u32.to_le_bytes()); // name length past the cap
  assert_eq!(
    ShimRequest::decode(&oversized),
    Err(ShimWireError::BadLength)
  );
}

/// `serve` drives the read path against a real bridge: after creating a file, a Lookup finds it, a
/// GetAttr on its inode returns an OK attribute reply, and a Read returns its bytes — the whole path
/// with no ring and no mount.
#[test]
fn serve_drives_the_read_path_over_a_real_bridge() {
  let mut store = store();
  let mut vol = volume(&mut store);
  let mut bridge = VolumeBridge::new(VolumeId { bytes: [0x11; 16] }, &mut vol, &mut store);
  let cx = write_cx();
  let root = bridge.root(&cx).unwrap();
  let created = bridge.create(oid(root), &cx, "hello", 0o644, 0).unwrap();
  let (attr, fh) = created;
  bridge.write(oid(attr.ino), &cx, 0, b"world").unwrap();
  bridge.release(oid(attr.ino), &cx, fh).ok();

  // Lookup finds the child and returns an OK attribute reply naming its inode.
  let lookup = ShimRequest::Lookup {
    parent: oid(root),
    name: "hello".to_owned(),
  }
  .encode();
  let reply = serve(&lookup, &mut bridge, &cx).unwrap();
  assert_eq!(reply[0], STATUS_OK, "lookup succeeded");
  let mut ino_bytes = [0u8; 8];
  ino_bytes.copy_from_slice(&reply[1..9]);
  assert_eq!(
    u64::from_le_bytes(ino_bytes),
    attr.ino,
    "the reply names the child inode"
  );

  // GetAttr on the child returns an OK attribute reply.
  let getattr = ShimRequest::GetAttr {
    object: oid(attr.ino),
  }
  .encode();
  let reply = serve(&getattr, &mut bridge, &cx).unwrap();
  assert_eq!(reply[0], STATUS_OK, "getattr succeeded");

  // Read returns the file's bytes after the OK status and a u32 length.
  let read = ShimRequest::Read {
    object: oid(attr.ino),
    offset: 0,
    size: 16,
  }
  .encode();
  let reply = serve(&read, &mut bridge, &cx).unwrap();
  assert_eq!(reply[0], STATUS_OK, "read succeeded");
  let mut len_bytes = [0u8; 4];
  len_bytes.copy_from_slice(&reply[1..5]);
  let len = usize::try_from(u32::from_le_bytes(len_bytes)).unwrap();
  assert_eq!(
    &reply[5..5 + len],
    b"world",
    "the read returned the written bytes"
  );
}

/// `serve` drives a write through the bridge and reports the count stored, and a following read over
/// the seam returns exactly the written bytes — the write path round-trips end to end.
#[test]
fn serve_writes_through_the_bridge_and_reads_it_back() {
  let mut store = store();
  let mut vol = volume(&mut store);
  let mut bridge = VolumeBridge::new(VolumeId { bytes: [0x11; 16] }, &mut vol, &mut store);
  let cx = write_cx();
  let root = bridge.root(&cx).unwrap();
  let (attr, fh) = bridge.create(oid(root), &cx, "note", 0o644, 0).unwrap();
  bridge.release(oid(attr.ino), &cx, fh).ok();

  // Write through serve; the reply is OK and the u32 count equals the bytes sent.
  let payload = b"fskit bytes";
  let write = ShimRequest::Write {
    object: oid(attr.ino),
    offset: 0,
    data: payload.to_vec(),
  }
  .encode();
  let reply = serve(&write, &mut bridge, &cx).unwrap();
  assert_eq!(reply[0], STATUS_OK, "write succeeded");
  let mut count_bytes = [0u8; 4];
  count_bytes.copy_from_slice(&reply[1..5]);
  assert_eq!(
    usize::try_from(u32::from_le_bytes(count_bytes)).unwrap(),
    payload.len(),
    "the whole payload was written"
  );

  // Read it back over the seam.
  let read = ShimRequest::Read {
    object: oid(attr.ino),
    offset: 0,
    size: 32,
  }
  .encode();
  let reply = serve(&read, &mut bridge, &cx).unwrap();
  assert_eq!(reply[0], STATUS_OK);
  let mut len_bytes = [0u8; 4];
  len_bytes.copy_from_slice(&reply[1..5]);
  let len = usize::try_from(u32::from_le_bytes(len_bytes)).unwrap();
  assert_eq!(
    &reply[5..5 + len],
    payload,
    "the read returned the written bytes"
  );
}

/// `serve` drives a setattr through the bridge: a chmod and a truncate on a written file return an OK
/// attribute reply carrying the new mode and the new (smaller) size, and a following getattr sees the
/// same — the metadata path over the seam, with no ring and no mount. This is the op FSKit's
/// `setAttributes` needs (chmod/chown/truncate/utimes); AC-3.10.
#[test]
fn serve_sets_attributes_through_the_bridge() {
  let mut store = store();
  let mut vol = volume(&mut store);
  let mut bridge = VolumeBridge::new(VolumeId { bytes: [0x11; 16] }, &mut vol, &mut store);
  let cx = write_cx();
  let root = bridge.root(&cx).unwrap();
  let (attr, fh) = bridge.create(oid(root), &cx, "doc", 0o644, 0).unwrap();
  bridge.write(oid(attr.ino), &cx, 0, b"five!").unwrap();
  bridge.release(oid(attr.ino), &cx, fh).ok();

  // Change the mode to 0o600 and truncate the 5-byte file to 2 bytes in one setattr; the unset uid,
  // gid and times are left untouched.
  let set = ShimRequest::SetAttr {
    object: oid(attr.ino),
    size: Some(2),
    mode: Some(0o600),
    uid: None,
    gid: None,
    atime: None,
    mtime: None,
  }
  .encode();
  let reply = serve(&set, &mut bridge, &cx).unwrap();
  assert_eq!(reply[0], STATUS_OK, "setattr succeeded");

  // The reply is the new attributes: mode at bytes 18..22, size at bytes 34..42 (the NodeAttr wire
  // layout the getattr golden pins).
  let mut mode_bytes = [0u8; 4];
  mode_bytes.copy_from_slice(&reply[18..22]);
  assert_eq!(
    u32::from_le_bytes(mode_bytes) & 0o777,
    0o600,
    "the mode changed"
  );
  let mut size_bytes = [0u8; 8];
  size_bytes.copy_from_slice(&reply[34..42]);
  assert_eq!(u64::from_le_bytes(size_bytes), 2, "the file was truncated");

  // A following getattr sees the same, so the change is durable in the volume, not just the reply.
  let getattr = ShimRequest::GetAttr {
    object: oid(attr.ino),
  }
  .encode();
  let reply = serve(&getattr, &mut bridge, &cx).unwrap();
  assert_eq!(reply[0], STATUS_OK);
  let mut size_again = [0u8; 8];
  size_again.copy_from_slice(&reply[34..42]);
  assert_eq!(
    u64::from_le_bytes(size_again),
    2,
    "the truncate persisted in the volume"
  );
}

/// `serve` answers OP_ROOT with the volume's real root object — `compose(prefix, 1)`, NOT the inode 1
/// a FUSE-style handler would assume. This is the root-inode correctness fix: the FSKit handler learns
/// the true root at activate time instead of hardcoding a constant that is wrong for any prefixed
/// volume. §4.6.
#[test]
fn serve_returns_the_real_root_object() {
  let mut store = store();
  let mut vol = volume(&mut store);
  let mut bridge = VolumeBridge::new(VolumeId { bytes: [0x11; 16] }, &mut vol, &mut store);
  let cx = write_cx();
  let root = bridge.root(&cx).unwrap();
  // The test volume's prefix is 1, so its root is compose(1, 1) — provably NOT inode 1, the constant
  // the handler used to assume. A handler hardcoding 1 would address the wrong object on this volume.
  assert_ne!(root, 1, "a prefixed volume's root is not inode 1");

  let reply = serve(&ShimRequest::Root.encode(), &mut bridge, &cx).unwrap();
  assert_eq!(reply[0], STATUS_OK, "root succeeded");
  // The reply is the root's attributes: inode at bytes 1..9, kind at byte 17 (the NodeAttr layout).
  let mut ino_bytes = [0u8; 8];
  ino_bytes.copy_from_slice(&reply[1..9]);
  assert_eq!(
    u64::from_le_bytes(ino_bytes),
    root,
    "OP_ROOT returns the daemon's real root inode, not a constant"
  );
  assert_eq!(reply[17], 1, "the root is a directory (KIND_DIR)");
}

/// `serve` drives the directory lifecycle: mkdir a subdirectory, create a file in it, open and read
/// the directory to see the file, then unlink the file and rmdir the directory — the namespace and
/// enumeration path over the seam, with no ring and no mount.
#[test]
fn serve_drives_the_directory_lifecycle() {
  let mut store = store();
  let mut vol = volume(&mut store);
  let mut bridge = VolumeBridge::new(VolumeId { bytes: [0x11; 16] }, &mut vol, &mut store);
  let cx = write_cx();
  let root = bridge.root(&cx).unwrap();

  // Mkdir a subdirectory and read its inode from the OK attribute reply.
  let mkdir = ShimRequest::Mkdir {
    parent: oid(root),
    name: "sub".to_owned(),
    mode: 0o755,
  }
  .encode();
  let reply = serve(&mkdir, &mut bridge, &cx).unwrap();
  assert_eq!(reply[0], STATUS_OK, "mkdir succeeded");
  let mut dir_ino = [0u8; 8];
  dir_ino.copy_from_slice(&reply[1..9]);
  let dir = u64::from_le_bytes(dir_ino);

  // Create a file inside it (the reply is an attribute then a handle).
  let create = ShimRequest::Create {
    parent: oid(dir),
    name: "file".to_owned(),
    mode: 0o644,
    flags: 0,
  }
  .encode();
  let reply = serve(&create, &mut bridge, &cx).unwrap();
  assert_eq!(reply[0], STATUS_OK, "create succeeded");

  // Open the directory, read it, and confirm the file's name is among the entries.
  let opendir = ShimRequest::OpenDir { object: oid(dir) }.encode();
  let reply = serve(&opendir, &mut bridge, &cx).unwrap();
  assert_eq!(reply[0], STATUS_OK, "opendir succeeded");
  let mut fh_bytes = [0u8; 8];
  fh_bytes.copy_from_slice(&reply[1..9]);
  let fh = u64::from_le_bytes(fh_bytes);

  let readdir = ShimRequest::ReadDir {
    object: oid(dir),
    fh,
    offset: 0,
  }
  .encode();
  let reply = serve(&readdir, &mut bridge, &cx).unwrap();
  assert_eq!(reply[0], STATUS_OK, "readdir succeeded");
  // The reply is a u32 count then each entry (ino u64, kind u8, name len+bytes); scan for "file".
  let mut count_bytes = [0u8; 4];
  count_bytes.copy_from_slice(&reply[1..5]);
  let count = u32::from_le_bytes(count_bytes);
  let mut cursor = 5usize;
  let mut names = Vec::new();
  for _ in 0..count {
    cursor += 8 + 1; // ino + kind
    let mut nlen = [0u8; 4];
    nlen.copy_from_slice(&reply[cursor..cursor + 4]);
    let nlen = usize::try_from(u32::from_le_bytes(nlen)).unwrap();
    cursor += 4;
    names.push(String::from_utf8(reply[cursor..cursor + nlen].to_vec()).unwrap());
    cursor += nlen;
  }
  assert!(
    names.contains(&"file".to_owned()),
    "the created file is enumerated: {names:?}"
  );

  // Unlink the file and rmdir the directory; both are OK unit replies.
  let unlink = ShimRequest::Unlink {
    parent: oid(dir),
    name: "file".to_owned(),
  }
  .encode();
  assert_eq!(
    serve(&unlink, &mut bridge, &cx).unwrap(),
    vec![STATUS_OK],
    "unlink is an OK unit reply"
  );
  let rmdir = ShimRequest::Rmdir {
    parent: oid(root),
    name: "sub".to_owned(),
  }
  .encode();
  assert_eq!(
    serve(&rmdir, &mut bridge, &cx).unwrap(),
    vec![STATUS_OK],
    "rmdir is an OK unit reply"
  );
}

/// `serve` drives a symlink and reads its target back, and renames a file — the link and rename path
/// over the seam.
#[test]
fn serve_drives_symlink_and_rename() {
  let mut store = store();
  let mut vol = volume(&mut store);
  let mut bridge = VolumeBridge::new(VolumeId { bytes: [0x11; 16] }, &mut vol, &mut store);
  let cx = write_cx();
  let root = bridge.root(&cx).unwrap();

  // Symlink "link" -> "/target/path"; readlink returns the same target.
  let target = "/target/path";
  let symlink = ShimRequest::Symlink {
    parent: oid(root),
    name: "link".to_owned(),
    target: target.to_owned(),
  }
  .encode();
  let reply = serve(&symlink, &mut bridge, &cx).unwrap();
  assert_eq!(reply[0], STATUS_OK, "symlink succeeded");
  // The reply is the new attribute; its inode is the first field.
  let mut ino = [0u8; 8];
  ino.copy_from_slice(&reply[1..9]);
  let link_ino = u64::from_le_bytes(ino);

  let readlink = ShimRequest::Readlink {
    object: oid(link_ino),
  }
  .encode();
  let reply = serve(&readlink, &mut bridge, &cx).unwrap();
  assert_eq!(reply[0], STATUS_OK, "readlink succeeded");
  let mut len = [0u8; 4];
  len.copy_from_slice(&reply[1..5]);
  let len = usize::try_from(u32::from_le_bytes(len)).unwrap();
  assert_eq!(
    &reply[5..5 + len],
    target.as_bytes(),
    "readlink returns the target"
  );

  // Create a file and rename it; the rename is an OK unit reply and the new name resolves.
  let (attr, fh) = bridge.create(oid(root), &cx, "before", 0o644, 0).unwrap();
  bridge.release(oid(attr.ino), &cx, fh).ok();
  let rename = ShimRequest::Rename {
    old_parent: oid(root),
    new_parent: oid(root),
    old_name: "before".to_owned(),
    new_name: "after".to_owned(),
    no_replace: false,
    exchange: false,
  }
  .encode();
  assert_eq!(
    serve(&rename, &mut bridge, &cx).unwrap(),
    vec![STATUS_OK],
    "rename is an OK unit reply"
  );
  let lookup = ShimRequest::Lookup {
    parent: oid(root),
    name: "after".to_owned(),
  }
  .encode();
  assert_eq!(
    serve(&lookup, &mut bridge, &cx).unwrap()[0],
    STATUS_OK,
    "the renamed file resolves"
  );
}

/// A golden vector pins the wire so a change is caught across versions: the exact bytes of a GetAttr
/// request and of a NotFound error reply.
#[test]
fn the_wire_is_pinned_by_a_golden_vector() {
  // GetAttr: op tag 2, then the object — inode (u64 LE) then generation (u64 LE).
  let request = ShimRequest::GetAttr { object: oid(7) }.encode();
  let mut golden = vec![2u8];
  golden.extend_from_slice(&7u64.to_le_bytes());
  golden.extend_from_slice(&0u64.to_le_bytes());
  assert_eq!(request, golden, "the GetAttr request wire is stable");

  // A NotFound error reply is the status byte 1 (ERR) then the ShimError tag 0 (NotFound).
  let mut store = store();
  let mut vol = volume(&mut store);
  let mut bridge = VolumeBridge::new(VolumeId { bytes: [0x11; 16] }, &mut vol, &mut store);
  let cx = write_cx();
  let missing = ShimRequest::GetAttr {
    object: oid(12_345),
  }
  .encode();
  let reply = serve(&missing, &mut bridge, &cx).unwrap();
  assert_eq!(
    reply,
    vec![STATUS_ERR, SHIM_NOT_FOUND],
    "the NotFound error reply wire is stable"
  );
}

/// A refusal from the bridge becomes an error reply carrying the mapped ShimError tag — the read of a
/// nonexistent object is NotFound, not a crash.
#[test]
fn serve_maps_a_refusal_to_an_error_reply() {
  let mut store = store();
  let mut vol = volume(&mut store);
  let mut bridge = VolumeBridge::new(VolumeId { bytes: [0x11; 16] }, &mut vol, &mut store);
  let cx = write_cx();

  let getattr = ShimRequest::GetAttr { object: oid(9_999) }.encode();
  let reply = serve(&getattr, &mut bridge, &cx).unwrap();
  assert_eq!(reply[0], STATUS_ERR, "a missing object is an error reply");
  assert_eq!(reply[1], SHIM_NOT_FOUND, "the refusal is NotFound");
}

/// Reads a little-endian `u64` from a reply at `start` (an inode at byte 1 of an attribute reply, an
/// `fh` at byte 1 of a handle reply — both follow the one `STATUS_OK` byte).
fn u64_at(reply: &[u8], start: usize) -> u64 {
  u64::from_le_bytes(reply[start..start + 8].try_into().unwrap())
}

/// A `MountSession` persists open handles across requests — the daemon serves each request with a fresh
/// bridge over the shard's volume, so the open reference an `open` takes and the `fh` it returns must
/// survive to the matching `release`, which is a *separate* request on a *fresh* bridge. Proven through
/// POSIX unlink-while-open: an open file's inode stays alive after its name is removed and is freed only
/// when release drops the handle. If the session did not persist the map, release would not find the
/// handle, the reference would leak, and the inode would never free — so the final getattr failing is
/// the proof. §4.6, §4.8.
#[test]
fn a_mount_session_persists_open_handles_across_requests() {
  let mut store = store();
  let mut vol = volume(&mut store);
  let mut session = MountSession::new(VolumeId { bytes: [0x11; 16] });
  let cx = write_cx();

  // The volume's real root, learned over the seam (each call below is a fresh bridge).
  let root_reply = session
    .serve(&mut store, &mut vol, None, &cx, &ShimRequest::Root.encode())
    .unwrap();
  assert_eq!(root_reply[0], STATUS_OK, "root");
  let root = u64_at(&root_reply, 1);

  // Create "f" under the root and learn its inode (create's reply is an attribute-plus-handle).
  let create_reply = session
    .serve(
      &mut store,
      &mut vol,
      None,
      &cx,
      &ShimRequest::Create {
        parent: oid(root),
        name: "f".to_owned(),
        mode: 0o644,
        flags: 0,
      }
      .encode(),
    )
    .unwrap();
  assert_eq!(create_reply[0], STATUS_OK, "create f");
  let file = u64_at(&create_reply, 1);
  // create is create-and-open: it also takes an open reference and returns a handle *after* the
  // attributes (STATUS_OK, then a 65-byte attribute record, then the fh). Capture it to release later.
  let create_fh = u64_at(&create_reply, 66);

  // Give it content, so there is something to keep alive and then reclaim.
  let write_reply = session
    .serve(
      &mut store,
      &mut vol,
      None,
      &cx,
      &ShimRequest::Write {
        object: oid(file),
        offset: 0,
        data: b"data".to_vec(),
      }
      .encode(),
    )
    .unwrap();
  assert_eq!(write_reply[0], STATUS_OK, "write f");

  // Open it — the open reference and the returned fh land in the session's persistent map.
  let open_reply = session
    .serve(
      &mut store,
      &mut vol,
      None,
      &cx,
      &ShimRequest::Open {
        object: oid(file),
        flags: 0,
      }
      .encode(),
    )
    .unwrap();
  assert_eq!(open_reply[0], STATUS_OK, "open f");
  let fh = u64_at(&open_reply, 1);

  // Remove the name. The inode stays alive because it is open (POSIX unlink-while-open, §4.8).
  let unlink_reply = session
    .serve(
      &mut store,
      &mut vol,
      None,
      &cx,
      &ShimRequest::Unlink {
        parent: oid(root),
        name: "f".to_owned(),
      }
      .encode(),
    )
    .unwrap();
  assert_eq!(unlink_reply[0], STATUS_OK, "unlink f");

  // While still open, a read returns the content — the open reference keeps it alive (§4.8).
  let while_open = session
    .serve(
      &mut store,
      &mut vol,
      None,
      &cx,
      &ShimRequest::Read {
        object: oid(file),
        offset: 0,
        size: 16,
      }
      .encode(),
    )
    .unwrap();
  assert_eq!(
    while_open[0], STATUS_OK,
    "an unlinked but open file keeps its content"
  );

  // Release with the fh from the earlier open. This must find the handle in the persisted map (a fresh
  // per-request bridge that lost it could not) and drop the last reference.
  let release_reply = session
    .serve(
      &mut store,
      &mut vol,
      None,
      &cx,
      &ShimRequest::Release {
        object: oid(file),
        fh,
      }
      .encode(),
    )
    .unwrap();
  assert_eq!(release_reply[0], STATUS_OK, "release the open handle");

  // Drop create's handle too — create is create-and-open, so this is the last reference. Its handle
  // came from the create request, so releasing it here through yet another fresh bridge again requires
  // the map to have persisted across requests.
  let release_create = session
    .serve(
      &mut store,
      &mut vol,
      None,
      &cx,
      &ShimRequest::Release {
        object: oid(file),
        fh: create_fh,
      }
      .encode(),
    )
    .unwrap();
  assert_eq!(release_create[0], STATUS_OK, "release the create handle");

  // Now a read fails: the name is gone and the last open reference is dropped, so the content is
  // reclaimed — which means release found the handle in the persisted map and dropped the reference.
  let after_release = session
    .serve(
      &mut store,
      &mut vol,
      None,
      &cx,
      &ShimRequest::Read {
        object: oid(file),
        offset: 0,
        size: 16,
      }
      .encode(),
    )
    .unwrap();
  assert_eq!(
    after_release[0], STATUS_ERR,
    "after release the content is reclaimed — the handle persisted and release dropped the reference"
  );
}

/// A read-only overlay base directory for tests that need one without a RAM disk: the repo's `crates/`
/// tree, opened read-only. `bridge-fskit` is one of its entries.
fn crates_dir() -> std::path::PathBuf {
  std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    .parent()
    .unwrap()
    .to_path_buf()
}

/// A `MountSession` serves an overlay volume's base through the *borrowed* host: the daemon lends the
/// shard's `OsHost` per request, and a base entry the empty overlay does not hold is found from the
/// host. This exercises `VolumeBridge::attached`'s borrowed-host path (`HostRef::Borrowed`) — the
/// overlay analogue of the borrowed handle map. Read-only, no RAM disk (the base is the repo's crates
/// tree). §4.5, §4.6.
#[test]
fn a_mount_session_serves_an_overlay_base_through_the_borrowed_host() {
  let mut store = store();
  let (mut host, root_dir) = OsHost::open_root(&crates_dir()).unwrap();
  let facts = host.facts(root_dir).unwrap();
  let mut vol = Volume::create_overlay(
    &mut store,
    VolumeConfig {
      prefix: 1,
      names: NameEquivalence::Exact,
      quota: Quota::Bounded { limit: 1 << 30 },
      journal_bytes: 1 << 16,
      clock: Box::new(HostClock::default()),
    },
    BaseConfig {
      root: root_dir,
      facts,
      large_class_bytes: 1 << 20,
    },
  )
  .unwrap();
  let mut session = MountSession::new(VolumeId { bytes: [0x11; 16] });
  let cx = write_cx();

  // The overlay's root, learned over the seam.
  let root_reply = session
    .serve(
      &mut store,
      &mut vol,
      Some(&mut host),
      &cx,
      &ShimRequest::Root.encode(),
    )
    .unwrap();
  assert_eq!(root_reply[0], STATUS_OK, "root");
  let root = u64_at(&root_reply, 1);

  // Look up a base entry the empty overlay does not hold — `bridge-fskit` is a real crates/ subdirectory,
  // served from the base through the borrowed host lent to the transient bridge.
  let lookup = session
    .serve(
      &mut store,
      &mut vol,
      Some(&mut host),
      &cx,
      &ShimRequest::Lookup {
        parent: oid(root),
        name: "bridge-fskit".to_owned(),
      }
      .encode(),
    )
    .unwrap();
  assert_eq!(
    lookup[0], STATUS_OK,
    "a base directory is found through the borrowed host — the overlay serve path works"
  );
}
