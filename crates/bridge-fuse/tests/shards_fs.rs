//! The Shards fs backend's frames and answers (`slates_bridge_fuse::shards_fs`), driven the way the daemon drives them
//! (§4.6, the Shards route; A-161): a guest's request arrives as a frame, a little-endian `u32` length then the FUSE
//! request; [`split_frame`] takes it from the bytes read so far, bounded before anything is allocated for it, and
//! [`answer`] dispatches it through the FUSE bridge and writes the reply frame. Here the frames are bytes in memory, no
//! socket and no OS types, so the guest's whole mount and read runs on every platform and architecture, Windows
//! included. The same functions over a real stream end, inside the daemon, are `crates/server/tests/shards.rs`, and a
//! program handed that end by the CLI is `crates/cli/tests/cli.rs`.

#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::indexing_slicing,
  clippy::panic
)]

use slates_bridge_core::{Attachments, OpContext, OwnerView, Rights, View};
use slates_bridge_fuse::abi::{IN_HEADER_LEN, OUT_HEADER_LEN, Opcode};
use slates_bridge_fuse::reply::{Attr, EntryOut};
use slates_bridge_fuse::request::SetAttrIn;
use slates_bridge_fuse::shards_fs::{
  FRAME_BYTES, Frame, LENGTH_BYTES, OWNER_WORD, OwnerWord, Split, answer, owner_word, split_frame,
};
use slates_bridge_fuse::volume_bridge::VolumeBridge;
use slates_db::catalog::{Principal, VolumeId};
use slates_mem::arena::ChunkArena;
use slates_mem::region::Region;
use slates_vfs::clock::HostClock;
use slates_vfs::names::NameEquivalence;
use slates_vfs::quota::Quota;
use slates_vfs::volume::{Store, StoreConfig, Volume, VolumeConfig};

const PAGE: usize = 4096;
const REGION_PAGES: usize = 1024;

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

fn test_cx() -> OpContext {
  let mut attachments = Attachments::new();
  let id = attachments
    .attach(
      VolumeId { bytes: [0; 16] },
      View::Current,
      Principal::Uid {
        host: slates_db::HostId(0),
        uid: 0,
      },
      Rights {
        read: true,
        write: true,
      },
    )
    .unwrap();
  attachments.context(id).unwrap()
}

/// A FUSE request: the 40-byte in-header (`len`, `opcode`, `unique`, `nodeid`, …) then `body`.
fn message(opcode: u32, unique: u64, nodeid: u64, body: &[u8]) -> Vec<u8> {
  let total = IN_HEADER_LEN + body.len();
  let mut m = vec![0u8; total];
  m[0..4].copy_from_slice(&u32::try_from(total).unwrap().to_le_bytes());
  m[4..8].copy_from_slice(&opcode.to_le_bytes());
  m[8..16].copy_from_slice(&unique.to_le_bytes());
  m[16..24].copy_from_slice(&nodeid.to_le_bytes());
  m[IN_HEADER_LEN..].copy_from_slice(body);
  m
}

/// The errno a FUSE reply carries (the out-header's `error`, a signed little-endian i32 at offset 4).
fn errno(out: &[u8]) -> i32 {
  i32::from_le_bytes(out[4..8].try_into().unwrap())
}

/// The file bytes a guest reads back; distinctive so the READ reply can only be the file's own content.
const HELLO: &[u8] = b"hello from slates over shards virtio-fs\n";

/// A `u64` at the start of a reply's body (the field `EntryOut.nodeid` and `OpenOut.fh` both begin with).
fn body_u64(reply: &[u8]) -> u64 {
  u64::from_le_bytes(
    reply[OUT_HEADER_LEN..OUT_HEADER_LEN + 8]
      .try_into()
      .unwrap(),
  )
}

/// `request` framed as Shards' device frames it: its length, little-endian, then its bytes.
fn framed(request: &[u8]) -> Vec<u8> {
  let mut frame = u32::try_from(request.len()).unwrap().to_le_bytes().to_vec();
  frame.extend_from_slice(request);
  frame
}

/// Frames `request`, takes it back with [`split_frame`] as the daemon reads a stream, answers it with [`answer`], and
/// returns the reply without its length word, which must name exactly the reply's bytes.
fn exchange(bridge: &mut VolumeBridge<'_>, cx: &OpContext, request: &[u8]) -> Vec<u8> {
  let bytes = framed(request);
  let Split::Whole(Frame::Request(taken), used) = split_frame(&bytes, FRAME_BYTES).unwrap() else {
    panic!("a whole request frame");
  };
  assert_eq!((taken, used), (request, bytes.len()));
  let mut frame = Vec::new();
  let answered = answer(taken, bridge, cx, &mut frame);
  let reply = answered.reply(&frame).to_vec();
  let word = u32::from_le_bytes(frame[..LENGTH_BYTES].try_into().unwrap());
  assert_eq!(usize::try_from(word).unwrap(), reply.len());
  assert_eq!(answered.error, -errno(&reply));
  reply
}

/// §4.6, A-161 (the guest's own mount and read, on every platform and architecture, no socket). Do: put a one-file
/// volume behind the bridge, then drive what a guest kernel's virtio-fs client does to read a file — `INIT`, `LOOKUP`
/// of the name under the root, `OPEN` of the returned inode, `READ` of its bytes — each framed as Shards' device frames
/// it, split and answered as the daemon does. Expect every step answered with errno 0, each reply frame's length word
/// naming exactly its reply, and the `READ` returning the file's own bytes.
#[test]
fn the_slates_backend_answers_a_guests_full_mount_and_read_of_a_file() {
  let mut store = store();
  let mut vol = Volume::create(
    &mut store,
    VolumeConfig {
      prefix: 1,
      names: NameEquivalence::Exact,
      quota: Quota::Bounded { limit: 1 << 30 },
      journal_bytes: 1 << 16,
      clock: Box::new(HostClock::default()),
    },
  )
  .unwrap();
  // One file `hello` under the root, with distinctive content, before the bridge borrows the volume.
  let root = vol.root_inode(&store).unwrap();
  let hello = vol
    .create_file_no(&mut store, root, "hello", 0o644)
    .unwrap();
  vol.write(&mut store, hello, 0, HELLO).unwrap();
  let mut bridge = VolumeBridge::new(VolumeId { bytes: [0; 16] }, &mut vol, &mut store);
  let cx = test_cx();

  // FUSE_INIT: major 7, minor 38, max_readahead 0, flags 0 — the codec reads what it needs.
  let mut init = Vec::new();
  init.extend_from_slice(&7u32.to_le_bytes());
  init.extend_from_slice(&38u32.to_le_bytes());
  init.extend_from_slice(&0u32.to_le_bytes());
  init.extend_from_slice(&0u32.to_le_bytes());
  init.resize(64, 0);
  let r = exchange(&mut bridge, &cx, &message(Opcode::Init as u32, 1, 0, &init));
  assert_eq!(errno(&r), 0, "INIT served");

  // LOOKUP "hello" under the root (nodeid 1): the reply's body begins with the file's inode number.
  let r = exchange(
    &mut bridge,
    &cx,
    &message(Opcode::Lookup as u32, 2, 1, b"hello\0"),
  );
  assert_eq!(errno(&r), 0, "LOOKUP of hello served");
  let nodeid = body_u64(&r);
  assert_ne!(nodeid, 0, "the lookup resolved the file to a nodeid");

  // OPEN that inode (fuse_open_in: flags u32, then 4 padding): the reply's body begins with the file handle.
  let mut open_in = 0u32.to_le_bytes().to_vec();
  open_in.extend_from_slice(&[0u8; 4]);
  let r = exchange(
    &mut bridge,
    &cx,
    &message(Opcode::Open as u32, 3, nodeid, &open_in),
  );
  assert_eq!(errno(&r), 0, "OPEN served");
  let fh = body_u64(&r);

  // READ the whole file (fuse_read_in: fh u64, offset u64, size u32, …).
  let mut read_in = vec![0u8; 24];
  read_in[0..8].copy_from_slice(&fh.to_le_bytes());
  read_in[8..16].copy_from_slice(&0u64.to_le_bytes());
  read_in[16..20].copy_from_slice(&u32::try_from(HELLO.len()).unwrap().to_le_bytes());
  let r = exchange(
    &mut bridge,
    &cx,
    &message(Opcode::Read as u32, 4, nodeid, &read_in),
  );
  assert_eq!(errno(&r), 0, "READ served");
  assert_eq!(
    &r[OUT_HEADER_LEN..],
    HELLO,
    "the guest reads the slates file's own bytes through the shards fs backend"
  );
}

/// Hostile input on the frame parser (CLAUDE.md §4: every parser of external bytes): a frame's length is the first four
/// bytes off the stream, the guest's to choose. Do: split a literal `u32::MAX` length, the largest length a request can
/// name, one byte past the caller's bound, a frame that claims 100 bytes and brings 4, and no bytes at all. Expect the
/// first three refused `InvalidData` from the length word alone, before anything is sized by it; the short frame and
/// the empty input partial, naming the bytes they need in all (the daemon then waits for them, and a close before they
/// come is counted as a frame cut short).
#[test]
fn a_hostile_or_truncated_shards_fs_frame_is_refused_before_allocating() {
  use std::io::ErrorKind;
  let bound = 1 << 16;
  for word in [u32::MAX, u32::MAX >> 1, bound + 1] {
    let bytes = word.to_le_bytes();
    assert_eq!(
      split_frame(&bytes, usize::try_from(bound).unwrap())
        .unwrap_err()
        .kind(),
      ErrorKind::InvalidData,
      "length word {word:#x}"
    );
  }
  let mut truncated = 100u32.to_le_bytes().to_vec();
  truncated.extend_from_slice(&[0xAB; 4]);
  assert_eq!(
    split_frame(&truncated, FRAME_BYTES).unwrap(),
    Split::Partial(LENGTH_BYTES + 100)
  );
  assert_eq!(
    split_frame(&[], FRAME_BYTES).unwrap(),
    Split::Partial(LENGTH_BYTES)
  );
}

/// Format: where `fuse_attr`'s uid and gid lie in it (after ino, size, blocks, the three times, their three nanosecond
/// words, the mode and nlink: `include/uapi/linux/fuse.h`).
const ATTR_UID_AT: usize = 68;
const ATTR_GID_AT: usize = 72;
/// Format: where `fuse_attr` lies in a `fuse_entry_out` (after nodeid, generation, the two valid times and their two
/// nanosecond words) and in a `fuse_attr_out` (after its valid time, nanosecond word and padding).
const ENTRY_ATTR_AT: usize = OUT_HEADER_LEN + EntryOut::LEN - Attr::LEN;
const ATTR_OUT_ATTR_AT: usize = OUT_HEADER_LEN + 16;
/// Format: a `fuse_dirent`'s fixed part (ino, off, namelen, type) after its `fuse_entry_out` in a READDIRPLUS reply, and
/// the alignment its name is padded to (`FUSE_DIRENT_ALIGN`).
const DIRENT_HEAD: usize = 24;
const DIRENT_ALIGN: usize = 8;

/// The owner the volume keeps for what its host made, and the run's owner a guest names: distinct, as on a Mac (the
/// user 501 in group 20) under a run of user 1000.
const HOST: (u32, u32) = (501, 20);
const RUN: (u32, u32) = (1000, 1001);
/// An owner neither the host nor the run is: the view leaves it as it is.
const NEITHER: (u32, u32) = (4242, 4343);

/// A FUSE request from the caller `uid`/`gid` (the in-header's `uid` and `gid` at 24 and 28).
fn message_as(
  opcode: u32,
  unique: u64,
  nodeid: u64,
  (uid, gid): (u32, u32),
  body: &[u8],
) -> Vec<u8> {
  let mut m = message(opcode, unique, nodeid, body);
  m[24..28].copy_from_slice(&uid.to_le_bytes());
  m[28..32].copy_from_slice(&gid.to_le_bytes());
  m
}

/// The uid and gid of the `fuse_attr` at `at` in `reply`.
fn owner_at(reply: &[u8], at: usize) -> (u32, u32) {
  let word = |at: usize| u32::from_le_bytes(reply[at..at + 4].try_into().unwrap());
  (word(at + ATTR_UID_AT), word(at + ATTR_GID_AT))
}

/// A SETXATTR of `name` set to `value` (`fuse_setxattr_in`: size, flags, then the name and its NUL, then the value).
fn setxattr_body(name: &[u8], value: &[u8]) -> Vec<u8> {
  let mut body = u32::try_from(value.len()).unwrap().to_le_bytes().to_vec();
  body.extend_from_slice(&0u32.to_le_bytes());
  body.extend_from_slice(name);
  body.push(0);
  body.extend_from_slice(value);
  body
}

/// A-281 (Shards' owner word, as Shards' server takes it). Do: read the word as a stream does, from the guest's root on
/// the share's root, with and without trailing NULs and either flag; from another caller; on another node; with values
/// that are no `UID:GID`; with a value cut short of its size; and read requests that are not the word. Expect the run's
/// owner named, `EPERM` from any caller but root, `EINVAL` on another node or for a malformed value, `EIO` for a body too
/// short for its value, and nothing taken from any other request, which the dispatch answers.
#[test]
fn shards_owner_word_is_taken_from_the_guests_root_on_the_shares_root_alone() {
  let set = Opcode::SetXattr as u32;
  let word = |nodeid: u64, caller: (u32, u32), value: &[u8]| {
    owner_word(&message_as(
      set,
      7,
      nodeid,
      caller,
      &setxattr_body(OWNER_WORD, value),
    ))
  };
  let named = Some(OwnerWord::Named {
    unique: 7,
    uid: 1000,
    gid: 1001,
  });
  assert_eq!(word(1, (0, 0), b"1000:1001"), named);
  assert_eq!(
    word(1, (0, 0), b"1000:1001\0\0"),
    named,
    "trailing NULs are dropped"
  );
  let refused = |errno| Some(OwnerWord::Refused { unique: 7, errno });
  assert_eq!(
    word(1, (1000, 1001), b"1000:1001"),
    refused(1),
    "only the guest's root names it: EPERM"
  );
  assert_eq!(
    word(5, (0, 0), b"1000:1001"),
    refused(22),
    "only on the share's root: EINVAL"
  );
  for malformed in [
    &b"1000"[..],
    b"1000:",
    b":1001",
    b"a:b",
    b"1000:1001:1",
    b"-1:0",
    b"4294967296:0",
    b"",
  ] {
    assert_eq!(
      word(1, (0, 0), malformed),
      refused(22),
      "{:?}",
      String::from_utf8_lossy(malformed)
    );
  }
  let mut short = setxattr_body(OWNER_WORD, b"1000:1001");
  short[0..4].copy_from_slice(&64u32.to_le_bytes());
  assert_eq!(
    owner_word(&message_as(set, 7, 1, (0, 0), &short)),
    refused(5),
    "a value cut short of its size: EIO"
  );
  assert_eq!(
    owner_word(&message_as(
      set,
      7,
      1,
      (0, 0),
      &setxattr_body(b"user.shards.owner", b"1000:1001")
    )),
    None,
    "another name is the dispatch's"
  );
  assert_eq!(
    owner_word(&message_as(
      Opcode::GetXattr as u32,
      7,
      1,
      (0, 0),
      &setxattr_body(OWNER_WORD, b"")
    )),
    None,
    "another opcode is the dispatch's"
  );
  assert_eq!(owner_word(&[0u8; 8]), None, "no header: the dispatch's");
}

/// A volume whose root and three files are kept as the host's, the run's and neither's, behind no bridge yet.
fn owned_volume(store: &mut Store) -> Volume {
  let mut vol = Volume::create(
    store,
    VolumeConfig {
      prefix: 1,
      names: NameEquivalence::Exact,
      quota: Quota::Bounded { limit: 1 << 30 },
      journal_bytes: 1 << 16,
      clock: Box::new(HostClock::default()),
    },
  )
  .unwrap();
  let root = vol.root_inode(store).unwrap();
  vol.chown(store, root, HOST.0, HOST.1).unwrap();
  for (name, owner) in [
    ("host.txt", HOST),
    ("run.txt", RUN),
    ("neither.txt", NEITHER),
  ] {
    let file = vol.create_file_no(store, root, name, 0o644).unwrap();
    vol.chown(store, file, owner.0, owner.1).unwrap();
  }
  vol
}

/// The owner a READDIRPLUS reply shows for `name`.
fn listed_owner(reply: &[u8], name: &str) -> Option<(u32, u32)> {
  let mut at = OUT_HEADER_LEN;
  while at + EntryOut::LEN + DIRENT_HEAD <= reply.len() {
    let dirent = at + EntryOut::LEN;
    let len = usize::try_from(u32::from_le_bytes(
      reply[dirent + 16..dirent + 20].try_into().unwrap(),
    ))
    .unwrap();
    let listed = &reply[dirent + DIRENT_HEAD..dirent + DIRENT_HEAD + len];
    if listed == name.as_bytes() {
      return Some(owner_at(reply, at + EntryOut::LEN - Attr::LEN));
    }
    at = dirent + (DIRENT_HEAD + len).div_ceil(DIRENT_ALIGN) * DIRENT_ALIGN;
  }
  None
}

/// A-281 (a stream's owner view, through the dispatch every transport shares). Do: put a volume whose root and a file
/// are kept as the host's (501:20), one file as the run's (1000:1001) and one as neither's behind a bridge that views
/// owners as a stream does once its guest's root named the run's owner; LOOKUP each file, GETATTR the root, READDIRPLUS
/// the root, CREATE a file as the run's user, and chown the neither's file to the run's ids. Expect the host's files
/// shown as the run's, the run's as the host's and the neither's as they are; the created file and the chowned one kept
/// as the host's in the volume, which every other route reads; and the same requests through a bridge with no view
/// showing each owner as the volume keeps it.
#[test]
fn a_streams_owner_view_shows_the_hosts_files_as_the_runs_and_keeps_the_runs_ids_as_the_hosts() {
  let mut store = store();
  let mut vol = owned_volume(&mut store);
  let cx = test_cx();
  let (made, chowned) = {
    let mut bridge = VolumeBridge::new(VolumeId { bytes: [0; 16] }, &mut vol, &mut store)
      .viewing_owners(OwnerView::swapping(HOST, RUN));
    let mut ask = |unique: u64, opcode: Opcode, nodeid: u64, body: &[u8]| {
      exchange(
        &mut bridge,
        &cx,
        &message_as(opcode as u32, unique, nodeid, RUN, body),
      )
    };
    for (unique, name, shown) in [
      (1, "host.txt", RUN),
      (2, "run.txt", HOST),
      (3, "neither.txt", NEITHER),
    ] {
      let reply = ask(
        unique,
        Opcode::Lookup,
        1,
        &[name.as_bytes(), b"\0"].concat(),
      );
      assert_eq!(errno(&reply), 0, "LOOKUP {name}");
      assert_eq!(
        owner_at(&reply, ENTRY_ATTR_AT),
        shown,
        "{name} is shown as {shown:?}"
      );
    }
    let root = ask(4, Opcode::GetAttr, 1, &[0u8; 16]);
    assert_eq!(
      owner_at(&root, ATTR_OUT_ATTR_AT),
      RUN,
      "the host's root is shown as the run's"
    );
    let opened = ask(5, Opcode::OpenDir, 1, &[0u8; 8]);
    let mut read_in = vec![0u8; 40];
    read_in[0..8].copy_from_slice(&body_u64(&opened).to_le_bytes());
    read_in[16..20].copy_from_slice(&4096u32.to_le_bytes());
    let listed = ask(6, Opcode::ReadDirPlus, 1, &read_in);
    assert_eq!(
      listed_owner(&listed, "host.txt"),
      Some(RUN),
      "a listing shows the host's file as the run's"
    );
    assert_eq!(
      listed_owner(&listed, "run.txt"),
      Some(HOST),
      "and the run's as the host's"
    );
    // CREATE (`fuse_create_in`: flags, mode, umask, open flags; then the name) by the run's user.
    let mut create = vec![0u8; 16];
    create[4..8].copy_from_slice(&0o644u32.to_le_bytes());
    create.extend_from_slice(b"made.txt\0");
    let created = ask(7, Opcode::Create, 1, &create);
    assert_eq!(errno(&created), 0, "CREATE");
    assert_eq!(
      owner_at(&created, ENTRY_ATTR_AT),
      RUN,
      "the run's file is shown as the run's"
    );
    // SETATTR (`fuse_setattr_in`, 88 bytes: the uid at 76, the gid at 80) of the neither's file to the run's ids.
    let neither = body_u64(&ask(8, Opcode::Lookup, 1, b"neither.txt\0"));
    let mut chown = vec![0u8; 88];
    chown[0..4].copy_from_slice(&(SetAttrIn::FATTR_UID | SetAttrIn::FATTR_GID).to_le_bytes());
    chown[76..80].copy_from_slice(&RUN.0.to_le_bytes());
    chown[80..84].copy_from_slice(&RUN.1.to_le_bytes());
    let changed = ask(9, Opcode::SetAttr, neither, &chown);
    assert_eq!(errno(&changed), 0, "SETATTR");
    assert_eq!(
      owner_at(&changed, ATTR_OUT_ATTR_AT),
      RUN,
      "the chown reads back as the guest asked it"
    );
    (body_u64(&created), neither)
  };
  for (no, what) in [(made, "the created file"), (chowned, "the chowned file")] {
    let kept = vol
      .observe(&store, slates_vfs::ids::InodeNo(no))
      .unwrap()
      .attrs;
    assert_eq!(
      (kept.uid, kept.gid),
      HOST,
      "{what} is kept as the host's, as every other route reads it"
    );
  }
  let mut bridge = VolumeBridge::new(VolumeId { bytes: [0; 16] }, &mut vol, &mut store);
  for (unique, name, kept) in [
    (10, "host.txt", HOST),
    (11, "run.txt", RUN),
    (12, "made.txt", HOST),
  ] {
    let reply = exchange(
      &mut bridge,
      &cx,
      &message(
        Opcode::Lookup as u32,
        unique,
        1,
        &[name.as_bytes(), b"\0"].concat(),
      ),
    );
    assert_eq!(
      owner_at(&reply, ENTRY_ATTR_AT),
      kept,
      "with no view {name} is shown as the volume keeps it"
    );
  }
}
