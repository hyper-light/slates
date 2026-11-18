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

/// The errno a FUSE reply carries, as Linux names it (the out-header's `error`, a signed little-endian i32 at offset 4,
/// is its negation).
fn errno(out: &[u8]) -> i32 {
  -i32::from_le_bytes(out[4..8].try_into().unwrap())
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
  assert_eq!(answered.error, errno(&reply));
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
  expect_every_other_request_left_to_the_dispatch();
}

/// The requests that are not the owner word, in `shards_owner_word_is_taken_from_the_guests_root_on_the_shares_root_alone`:
/// expect nothing taken from a SETXATTR of another name, from another opcode, or from bytes with no header; the dispatch
/// answers each.
fn expect_every_other_request_left_to_the_dispatch() {
  assert_eq!(
    owner_word(&message_as(
      Opcode::SetXattr as u32,
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
    expect_each_owner_shown_through_the_view(&mut ask);
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

/// The reads of `a_streams_owner_view_shows_the_hosts_files_as_the_runs_and_keeps_the_runs_ids_as_the_hosts`, each
/// through `ask` (unique, opcode, node, body) to a bridge viewing owners as a stream does once its guest's root named the
/// run's owner. Expect LOOKUP to show the host's file as the run's, the run's as the host's and the neither's as it is;
/// GETATTR to show the host's root as the run's; and a READDIRPLUS of the root to swap the host's and the run's files
/// the same way.
fn expect_each_owner_shown_through_the_view(
  ask: &mut impl FnMut(u64, Opcode, u64, &[u8]) -> Vec<u8>,
) {
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
}

/// A GETXATTR (`fuse_getxattr_in`: size, padding, then the name) or, with no name, a LISTXATTR, asking for `size` bytes.
fn getxattr_body(size: u32, name: Option<&[u8]>) -> Vec<u8> {
  let mut body = size.to_le_bytes().to_vec();
  body.extend_from_slice(&0u32.to_le_bytes());
  if let Some(name) = name {
    body.extend_from_slice(name);
    body.push(0);
  }
  body
}

/// A REMOVEXATTR of `name` (the name and its NUL).
fn removexattr_body(name: &[u8]) -> Vec<u8> {
  let mut body = name.to_vec();
  body.push(0);
  body
}

/// The names a LISTXATTR reply lists, each without its NUL.
fn listed_names(reply: &[u8]) -> Vec<Vec<u8>> {
  reply[OUT_HEADER_LEN..]
    .split(|byte| *byte == 0)
    .filter(|name| !name.is_empty())
    .map(<[u8]>::to_vec)
    .collect()
}

/// Linux's `ENODATA` and `EOPNOTSUPP`.
const ENODATA: i32 = 61;
const EOPNOTSUPP: i32 = 95;

/// A-290 (a guest's own attribute namespaces, through the dispatch every transport shares). Do: through a bridge that
/// serves a guest, set `security.capability` on a file, `trusted.overlay.opaque` on a directory, `trusted.overlay.origin`
/// on a symlink and a `user.` attribute on the file, then read and list each; then, through a host's bridge over the
/// same volume, list, read, set, replace and remove names in the guest's namespaces; then remove the guest's own
/// through its bridge. Expect the guest to read back every value and list every name; the host to list only `user.`,
/// read the guest's names `ENODATA` and have every set or removal of one refused `EOPNOTSUPP`, the guest's values
/// unchanged by it; and the guest's removal to take its attribute away.
#[test]
fn a_guests_own_attributes_are_kept_for_its_route_and_no_hosts_mount_reaches_them() {
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
  let root = vol.root_inode(&store).unwrap();
  let file = vol.create_file_no(&mut store, root, "bin", 0o755).unwrap();
  let dir = vol.mkdir_no(&mut store, root, "upper", 0o755).unwrap();
  let link = vol.symlink_no(&mut store, root, "link", "bin").unwrap();
  let cx = test_cx();
  let capability = [
    1u8, 0, 0, 2, 0, 32, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
  ];
  let guests: [(u64, &[u8], &[u8]); 4] = [
    (file.0, b"security.capability", &capability),
    (dir.0, b"trusted.overlay.opaque", b"y"),
    (link.0, b"trusted.overlay.origin", b"\x00\xfb\x21"),
    (file.0, b"user.mime_type", b"application/x-executable"),
  ];
  {
    let mut guest =
      VolumeBridge::new(VolumeId { bytes: [0; 16] }, &mut vol, &mut store).serving_a_guest();
    for (unique, (node, name, value)) in (1u64..).zip(guests) {
      let set = exchange(
        &mut guest,
        &cx,
        &message(
          Opcode::SetXattr as u32,
          unique,
          node,
          &setxattr_body(name, value),
        ),
      );
      assert_eq!(errno(&set), 0, "the guest sets {name:?}");
      let got = exchange(
        &mut guest,
        &cx,
        &message(
          Opcode::GetXattr as u32,
          unique + 100,
          node,
          &getxattr_body(256, Some(name)),
        ),
      );
      assert_eq!(
        (errno(&got), &got[OUT_HEADER_LEN..]),
        (0, value),
        "the guest reads {name:?}"
      );
    }
    let listed = exchange(
      &mut guest,
      &cx,
      &message(
        Opcode::ListXattr as u32,
        200,
        file.0,
        &getxattr_body(4096, None),
      ),
    );
    assert_eq!(
      listed_names(&listed),
      vec![b"security.capability".to_vec(), b"user.mime_type".to_vec()],
      "the guest lists its own names beside user."
    );
  }
  expect_no_hosts_mount_reaches_a_guests_own(&mut vol, &mut store, &cx, file.0, &guests[..3]);
  let mut guest =
    VolumeBridge::new(VolumeId { bytes: [0; 16] }, &mut vol, &mut store).serving_a_guest();
  for (unique, (node, name, value)) in (450u64..).zip(&guests[..3]) {
    let got = exchange(
      &mut guest,
      &cx,
      &message(
        Opcode::GetXattr as u32,
        unique,
        *node,
        &getxattr_body(256, Some(name)),
      ),
    );
    assert_eq!(
      (errno(&got), &got[OUT_HEADER_LEN..]),
      (0, *value),
      "{name:?} is the guest's as it set it"
    );
  }
  let removed = exchange(
    &mut guest,
    &cx,
    &message(
      Opcode::RemoveXattr as u32,
      500,
      file.0,
      &removexattr_body(b"security.capability"),
    ),
  );
  assert_eq!(errno(&removed), 0, "the guest removes its own");
  let gone = exchange(
    &mut guest,
    &cx,
    &message(
      Opcode::GetXattr as u32,
      501,
      file.0,
      &getxattr_body(256, Some(b"security.capability")),
    ),
  );
  assert_eq!(errno(&gone), ENODATA);
}

/// The host's half of `a_guests_own_attributes_are_kept_for_its_route_and_no_hosts_mount_reaches_them`. Do: through a
/// host's bridge over `vol`, list `file`'s names, then read, set (plain and replacing) and remove each of `guests`, the
/// guest's own names as the guest set them. Expect the list to name `user.` alone, each read `ENODATA`, and every set or
/// removal refused `EOPNOTSUPP`.
fn expect_no_hosts_mount_reaches_a_guests_own(
  vol: &mut Volume,
  store: &mut Store,
  cx: &OpContext,
  file: u64,
  guests: &[(u64, &[u8], &[u8])],
) {
  let mut host = VolumeBridge::new(VolumeId { bytes: [0; 16] }, vol, store);
  let listed = exchange(
    &mut host,
    cx,
    &message(
      Opcode::ListXattr as u32,
      300,
      file,
      &getxattr_body(4096, None),
    ),
  );
  assert_eq!(
    listed_names(&listed),
    vec![b"user.mime_type".to_vec()],
    "a host's mount lists user. alone"
  );
  for (unique, (node, name, _)) in (400u64..).zip(guests) {
    let got = exchange(
      &mut host,
      cx,
      &message(
        Opcode::GetXattr as u32,
        unique,
        *node,
        &getxattr_body(256, Some(name)),
      ),
    );
    assert_eq!(errno(&got), ENODATA, "a host reads no {name:?}");
    for flags in [0u32, 2] {
      let mut body = setxattr_body(name, b"host");
      body[4..8].copy_from_slice(&flags.to_le_bytes());
      let set = exchange(
        &mut host,
        cx,
        &message(Opcode::SetXattr as u32, unique + 10, *node, &body),
      );
      assert_eq!(
        errno(&set),
        EOPNOTSUPP,
        "a host sets no {name:?} (flags {flags})"
      );
    }
    let removed = exchange(
      &mut host,
      cx,
      &message(
        Opcode::RemoveXattr as u32,
        unique + 20,
        *node,
        &removexattr_body(name),
      ),
    );
    assert_eq!(errno(&removed), EOPNOTSUPP, "a host removes no {name:?}");
  }
}

/// Linux's `EINVAL` and `EACCES`.
const EINVAL: i32 = 22;
const EACCES: i32 = 13;

/// Linux's ACL entry tags (`include/uapi/linux/posix_acl.h`): the owner, a named user, the owning group, a named
/// group, the mask and everyone else; and the id an entry naming no one carries (`ACL_UNDEFINED_ID`).
const USER_OBJ: u16 = 0x01;
const USER: u16 = 0x02;
const GROUP_OBJ: u16 = 0x04;
const GROUP: u16 = 0x08;
const MASK: u16 = 0x10;
const OTHER: u16 = 0x20;
const NONE: u32 = u32::MAX;

/// A POSIX ACL value (version 2, then each entry's tag, permission set and id), as a guest's kernel sends one.
fn acl(entries: &[(u16, u16, u32)]) -> Vec<u8> {
  let mut value = 2u32.to_le_bytes().to_vec();
  for &(tag, perm, id) in entries {
    value.extend_from_slice(&tag.to_le_bytes());
    value.extend_from_slice(&perm.to_le_bytes());
    value.extend_from_slice(&id.to_le_bytes());
  }
  value
}

/// The bytes of attribute `name` the volume keeps on `no`, read past any bridge.
fn kept_value(vol: &Volume, store: &Store, no: slates_vfs::ids::InodeNo, name: &[u8]) -> Vec<u8> {
  let len = usize::try_from(vol.xattr_len(store, no, name).unwrap()).unwrap();
  let mut value = vec![0u8; len];
  assert_eq!(vol.xattr_read(store, no, name, 0, &mut value).unwrap(), len);
  value
}

/// A-302 (a guest's POSIX ACLs, through the dispatch every transport shares). Do: through a bridge that serves a guest
/// whose run named its owner (the host's 501:20 shown as the run's 1000:1001), set an access ACL on a file naming the
/// run, a third user and the run's group, and a default ACL on the root; read both back and list the file; read what
/// the volume keeps past any bridge; set a default ACL on the file, then malformed ACLs; then, through a host's bridge,
/// list, read and set the ACL names. Expect the guest to read back exactly what it set; the volume to keep the run's
/// ids as the host's, in id order; the default ACL on a file refused `EACCES`; a torn value `EINVAL` and another
/// version `EOPNOTSUPP`, the kept ACL unchanged by them; and the host's bridge to list neither name, read each `ENODATA`
/// and have a set refused `EOPNOTSUPP`.
#[test]
fn a_guests_acls_are_kept_through_its_owner_view_and_no_hosts_mount_reaches_them() {
  let mut store = store();
  let mut vol = owned_volume(&mut store);
  let root = vol.root_inode(&store).unwrap();
  let file = vol
    .create_file_no(&mut store, root, "shared.txt", 0o664)
    .unwrap();
  let cx = test_cx();
  let access = acl(&[
    (USER_OBJ, 6, NONE),
    (USER, 6, RUN.0),
    (USER, 4, NEITHER.0),
    (GROUP_OBJ, 4, NONE),
    (GROUP, 6, RUN.1),
    (MASK, 6, NONE),
    (OTHER, 4, NONE),
  ]);
  let default = acl(&[(USER_OBJ, 7, NONE), (GROUP_OBJ, 5, NONE), (OTHER, 5, NONE)]);
  {
    let mut guest = VolumeBridge::new(VolumeId { bytes: [0; 16] }, &mut vol, &mut store)
      .viewing_owners(OwnerView::swapping(HOST, RUN))
      .serving_a_guest();
    let mut ask = |unique: u64, opcode: Opcode, nodeid: u64, body: &[u8]| {
      exchange(
        &mut guest,
        &cx,
        &message(opcode as u32, unique, nodeid, body),
      )
    };
    let set = ask(
      1,
      Opcode::SetXattr,
      file.0,
      &setxattr_body(b"system.posix_acl_access", &access),
    );
    assert_eq!(errno(&set), 0, "the guest sets its file's ACL");
    let set = ask(
      2,
      Opcode::SetXattr,
      1,
      &setxattr_body(b"system.posix_acl_default", &default),
    );
    assert_eq!(errno(&set), 0, "the guest sets its directory's default ACL");
    for (unique, node, name, value) in [
      (3, file.0, &b"system.posix_acl_access"[..], &access),
      (4, 1, &b"system.posix_acl_default"[..], &default),
    ] {
      let got = ask(
        unique,
        Opcode::GetXattr,
        node,
        &getxattr_body(256, Some(name)),
      );
      assert_eq!(
        (errno(&got), &got[OUT_HEADER_LEN..]),
        (0, value.as_slice()),
        "the guest reads its {name:?} as it set it"
      );
    }
    let listed = ask(5, Opcode::ListXattr, file.0, &getxattr_body(4096, None));
    assert_eq!(
      listed_names(&listed),
      vec![b"system.posix_acl_access".to_vec()]
    );
    let on_a_file = ask(
      6,
      Opcode::SetXattr,
      file.0,
      &setxattr_body(b"system.posix_acl_default", &default),
    );
    assert_eq!(
      errno(&on_a_file),
      EACCES,
      "a default ACL is a directory's alone"
    );
    expect_a_malformed_acl_refused(&mut ask, file.0, &access);
  }
  assert_eq!(
    kept_value(&vol, &store, file, b"system.posix_acl_access"),
    acl(&[
      (USER_OBJ, 6, NONE),
      (USER, 6, HOST.0),
      (USER, 4, NEITHER.0),
      (GROUP_OBJ, 4, NONE),
      (GROUP, 6, HOST.1),
      (MASK, 6, NONE),
      (OTHER, 4, NONE),
    ]),
    "the volume keeps the run's ids as the host's, in id order, and no refused set changed it"
  );
  expect_no_hosts_mount_reaches_a_guests_acls(&mut vol, &mut store, &cx, file.0, &default);
}

/// The malformed ACLs of `a_guests_acls_are_kept_through_its_owner_view_and_no_hosts_mount_reaches_them`. Do: through
/// `ask` to the guest's bridge, set on `file` the access ACL `access` torn by its last byte, then under another version.
/// Expect the torn one refused `EINVAL` and the other version `EOPNOTSUPP`.
fn expect_a_malformed_acl_refused(
  ask: &mut impl FnMut(u64, Opcode, u64, &[u8]) -> Vec<u8>,
  file: u64,
  access: &[u8],
) {
  let mut torn = access.to_vec();
  torn.pop();
  let mut other_version = access.to_vec();
  other_version[0] = 1;
  for (unique, value, refused) in [(7, torn, EINVAL), (8, other_version, EOPNOTSUPP)] {
    let set = ask(
      unique,
      Opcode::SetXattr,
      file,
      &setxattr_body(b"system.posix_acl_access", &value),
    );
    assert_eq!(errno(&set), refused);
  }
}

/// The host's half of `a_guests_acls_are_kept_through_its_owner_view_and_no_hosts_mount_reaches_them`. Do: through a
/// host's bridge over `vol`, list `file`'s names, then read each ACL name (the file's access ACL, the root's default) and
/// set it to `default`. Expect no name listed, each read `ENODATA` and each set refused `EOPNOTSUPP`.
fn expect_no_hosts_mount_reaches_a_guests_acls(
  vol: &mut Volume,
  store: &mut Store,
  cx: &OpContext,
  file: u64,
  default: &[u8],
) {
  let mut host = VolumeBridge::new(VolumeId { bytes: [0; 16] }, vol, store);
  let listed = exchange(
    &mut host,
    cx,
    &message(
      Opcode::ListXattr as u32,
      20,
      file,
      &getxattr_body(4096, None),
    ),
  );
  assert_eq!(
    listed_names(&listed),
    Vec::<Vec<u8>>::new(),
    "a host's mount lists no ACL"
  );
  for (unique, node, name) in [
    (21, file, &b"system.posix_acl_access"[..]),
    (22, 1, &b"system.posix_acl_default"[..]),
  ] {
    let got = exchange(
      &mut host,
      cx,
      &message(
        Opcode::GetXattr as u32,
        unique,
        node,
        &getxattr_body(256, Some(name)),
      ),
    );
    assert_eq!(errno(&got), ENODATA, "{name:?}");
    let set = exchange(
      &mut host,
      cx,
      &message(
        Opcode::SetXattr as u32,
        unique + 10,
        node,
        &setxattr_body(name, default),
      ),
    );
    assert_eq!(errno(&set), EOPNOTSUPP, "{name:?}");
  }
}

/// Linux's `FUSE_DONT_MASK`, `FUSE_POSIX_ACL` and `FUSE_INIT_EXT` bits (`include/uapi/linux/fuse.h`), as a guest's kernel
/// offers them at INIT and reads them in the reply.
const FUSE_DONT_MASK: u32 = 1 << 6;
const FUSE_POSIX_ACL: u32 = 1 << 20;
const FUSE_INIT_EXT: u32 = 1 << 30;
/// Format: where `fuse_init_out.flags` lies in an INIT reply (after major, minor and max_readahead).
const INIT_FLAGS_AT: usize = OUT_HEADER_LEN + 12;
/// Format: where `fuse_attr.mode` lies in it (after ino, size, blocks, the three times and their nanosecond words).
const ATTR_MODE_AT: usize = 60;
/// Format: where `fuse_setattr_in.mode` lies in it (`include/uapi/linux/fuse.h`).
const SETATTR_MODE_AT: usize = 68;
/// Format: `S_IFREG`, `S_IFIFO` and the permission bits of `st_mode` (`<sys/stat.h>`).
const S_IFREG: u32 = 0o100_000;
const S_IFIFO: u32 = 0o010_000;
const PERMISSIONS: u32 = 0o7777;
/// Format: `open(2)`'s `O_RDWR | O_CREAT`, the flags a create carries.
const O_RDWR_CREAT: u32 = 0o102;

/// The `fuse_init_in` a Linux 6.x guest's kernel sends: protocol 7.41, offering `flags` and the second flags word.
fn init_offering(flags: u32) -> Vec<u8> {
  let mut init = Vec::new();
  for word in [7u32, 41, 128 * 1024, flags | FUSE_INIT_EXT, 0] {
    init.extend_from_slice(&word.to_le_bytes());
  }
  init.resize(64, 0);
  init
}

/// Whether an INIT reply kept `FUSE_POSIX_ACL`: whether the guest's kernel checks each access against the access ACL
/// it reads from its server (`fs/fuse/inode.c` `process_init_reply` sets `posix_acl` and `default_permissions`).
fn keeps_posix_acl(reply: &[u8]) -> bool {
  u32::from_le_bytes(reply[INIT_FLAGS_AT..INIT_FLAGS_AT + 4].try_into().unwrap()) & FUSE_POSIX_ACL
    != 0
}

/// A caller as a guest's kernel judges one: its fsuid and every group it is in, and no capability.
struct Caller {
  uid: u32,
  groups: &'static [u32],
}

/// Linux's `MAY_READ`, `MAY_WRITE` and `MAY_EXEC` (`include/linux/fs.h`): what a caller asks of a node.
const MAY_READ: u32 = 4;
const MAY_WRITE: u32 = 2;
const MAY_EXEC: u32 = 1;

/// Whether a guest's kernel grants `caller` all of `want` on a node its server shows with `mode`, `owner` and, as its
/// access ACL, `acl`: Linux's `acl_permission_check` (`fs/namei.c`) and `posix_acl_permission` (`fs/posix_acl.c`),
/// v7.2-rc4, for a caller with no capability. The owner is judged by the owner bits alone; anyone else by the ACL when
/// there is one and the mode's group bits are not all clear, and otherwise by the group bits for a member of the
/// owning group and by the other bits for the rest.
fn kernel_grants(
  mode: u32,
  owner: (u32, u32),
  acl: Option<&[u8]>,
  caller: &Caller,
  want: u32,
) -> bool {
  if caller.uid == owner.0 {
    return want & !(mode >> 6) & 0o7 == 0;
  }
  if let Some(acl) = acl.filter(|_| mode & 0o070 != 0) {
    return acl_grants(acl, owner, caller, want);
  }
  let class = if caller.groups.contains(&owner.1) {
    mode >> 3
  } else {
    mode
  };
  want & !class & 0o7 == 0
}

/// `posix_acl_permission` over `acl`'s entries: the first entry naming the caller decides, a named user's or a group's
/// permissions cut by the mask; a caller in a group none of whose entries grants `want` is refused, never judged as
/// everyone else.
fn acl_grants(acl: &[u8], owner: (u32, u32), caller: &Caller, want: u32) -> bool {
  let entries: Vec<(u16, u32, u32)> = acl[4..]
    .chunks_exact(8)
    .map(|entry| {
      (
        u16::from_le_bytes(entry[0..2].try_into().unwrap()),
        u32::from(u16::from_le_bytes(entry[2..4].try_into().unwrap())),
        u32::from_le_bytes(entry[4..8].try_into().unwrap()),
      )
    })
    .collect();
  let mask = entries
    .iter()
    .find(|entry| entry.0 == MASK)
    .map_or(0o7, |entry| entry.1);
  let granted = |perm: u32| want & !perm == 0;
  let mut in_a_group = false;
  for &(tag, perm, id) in &entries {
    match tag {
      USER_OBJ if caller.uid == owner.0 => return granted(perm),
      USER if caller.uid == id => return granted(perm & mask),
      GROUP_OBJ | GROUP
        if caller
          .groups
          .contains(&if tag == GROUP_OBJ { owner.1 } else { id }) =>
      {
        in_a_group = true;
        if granted(perm) {
          return granted(perm & mask);
        }
      }
      OTHER => return !in_a_group && granted(perm),
      _ => {}
    }
  }
  false
}

/// What a guest's kernel holds of a node when it judges an access: the mode and owner GETATTR answers, and the access
/// ACL GETXATTR answers when its INIT kept `FUSE_POSIX_ACL` (none for `ENODATA`).
struct Judged {
  mode: u32,
  owner: (u32, u32),
  acl: Option<Vec<u8>>,
}

impl Judged {
  /// Asks for node `node` through `ask` as the guest's kernel does before it judges an access.
  fn of(ask: &mut dyn FnMut(Opcode, u64, &[u8]) -> Vec<u8>, node: u64, reads_acls: bool) -> Judged {
    let attr = ask(Opcode::GetAttr, node, &[0u8; 16]);
    assert_eq!(errno(&attr), 0, "GETATTR");
    let at = ATTR_OUT_ATTR_AT + ATTR_MODE_AT;
    let mode = u32::from_le_bytes(attr[at..at + 4].try_into().unwrap()) & PERMISSIONS;
    let acl = reads_acls
      .then(|| {
        let got = ask(
          Opcode::GetXattr,
          node,
          &getxattr_body(4096, Some(b"system.posix_acl_access")),
        );
        match errno(&got) {
          0 => Some(got[OUT_HEADER_LEN..].to_vec()),
          ENODATA => None,
          other => panic!("GETXATTR of the access ACL answered errno {other}"),
        }
      })
      .flatten();
    Judged {
      mode,
      owner: owner_at(&attr, ATTR_OUT_ATTR_AT),
      acl,
    }
  }

  /// Whether the guest's kernel grants `caller` all of `want` on the node.
  fn grants(&self, caller: &Caller, want: u32) -> bool {
    kernel_grants(self.mode, self.owner, self.acl.as_deref(), caller, want)
  }
}

/// A-314. Do: send the INIT a Linux guest's kernel sends, offering `FUSE_POSIX_ACL`, through a bridge that serves a guest
/// and through a host's; and an INIT offering none through the guest's. Expect the guest's reply to keep it, so the
/// guest's kernel checks every access against the ACLs it reads (and turns `default_permissions` on with it); the
/// host's to leave it, since a host's mount reaches no ACL and its kernel must never ask for one; and a kernel that
/// offers no ACL check to be given none.
#[test]
fn a_guests_kernel_is_asked_to_check_its_acls_and_a_hosts_is_not() {
  let mut store = store();
  let mut vol = owned_volume(&mut store);
  let cx = test_cx();
  let offer = init_offering(FUSE_POSIX_ACL | FUSE_DONT_MASK);
  {
    let mut guest =
      VolumeBridge::new(VolumeId { bytes: [0; 16] }, &mut vol, &mut store).serving_a_guest();
    assert!(
      init_keeps_posix_acl(&mut guest, &cx, &offer),
      "a guest's kernel is asked to check the ACLs it reads"
    );
    assert!(
      !init_keeps_posix_acl(&mut guest, &cx, &init_offering(FUSE_DONT_MASK)),
      "a kernel that offers no ACL check is given none"
    );
  }
  let mut host = VolumeBridge::new(VolumeId { bytes: [0; 16] }, &mut vol, &mut store);
  assert!(
    !init_keeps_posix_acl(&mut host, &cx, &offer),
    "a host's kernel is never asked for an ACL its mount cannot read"
  );
}

/// Sends INIT `offer` through `bridge` and says whether its reply kept `FUSE_POSIX_ACL`.
fn init_keeps_posix_acl(bridge: &mut VolumeBridge<'_>, cx: &OpContext, offer: &[u8]) -> bool {
  let reply = exchange(bridge, cx, &message(Opcode::Init as u32, 1, 0, offer));
  assert_eq!(errno(&reply), 0, "INIT served");
  keeps_posix_acl(&reply)
}

/// A-314 (what a guest's `setfacl` grants or denies holds for its accesses). Do: on a file the run owns at mode 0600,
/// set through the guest's route the ACL `setfacl -m u:4242:rw,u:4343:-` makes (owner rw, 4242 rw, 4343 none, the
/// owning group r, mask rw, everyone else none), and judge accesses as the guest's kernel judges them over what its
/// GETATTR and GETXATTR answer; chmod the file 0640 through a host's mount; remove the ACL through the guest; then
/// set the minimal ACL `setfacl -b` leaves (owner rwx, group r, other none). Expect, after the set, the mode 0660 (the
/// mask in the group bits), and 4242 to read and write and not execute, 4343 not to read, a member of the owning group
/// to read and not write, and anyone else nothing; after the host's chmod, the guest to read the ACL with its mask
/// r--, so 4242 reads and does not write; the removal to leave the mode 0640 and grant 4242 nothing; and the minimal
/// ACL to become the mode 0740 alone, read back as no ACL (`ENODATA`), as Linux's `posix_acl_update_mode` keeps it.
#[test]
fn what_a_guests_setfacl_grants_or_denies_holds_for_its_kernels_checks() {
  let mut store = store();
  let mut vol = owned_volume(&mut store);
  let root = vol.root_inode(&store).unwrap();
  let file = vol
    .create_file_no(&mut store, root, "secret.txt", 0o600)
    .unwrap();
  vol.chown(&mut store, file, RUN.0, RUN.1).unwrap();
  let cx = test_cx();
  let owner = Caller {
    uid: RUN.0,
    groups: &[RUN.1],
  };
  let reader = Caller {
    uid: 4242,
    groups: &[4242],
  };
  let refused = Caller {
    uid: 4343,
    groups: &[4343],
  };
  let member = Caller {
    uid: 5000,
    groups: &[5000, RUN.1],
  };
  let stranger = Caller {
    uid: 6000,
    groups: &[6000],
  };
  let mut unique = 0u64;
  let mut next = move || {
    unique += 1;
    unique
  };
  let reads_acls = {
    let mut guest =
      VolumeBridge::new(VolumeId { bytes: [0; 16] }, &mut vol, &mut store).serving_a_guest();
    let mut ask = |opcode: Opcode, nodeid: u64, body: &[u8]| {
      exchange(
        &mut guest,
        &cx,
        &message(opcode as u32, next(), nodeid, body),
      )
    };
    let reads_acls = keeps_posix_acl(&ask(
      Opcode::Init,
      0,
      &init_offering(FUSE_POSIX_ACL | FUSE_DONT_MASK),
    ));
    assert!(!Judged::of(&mut ask, file.0, reads_acls).grants(&reader, MAY_READ));
    let set = ask(
      Opcode::SetXattr,
      file.0,
      &setxattr_body(
        b"system.posix_acl_access",
        &acl(&[
          (USER_OBJ, 6, NONE),
          (USER, 6, 4242),
          (USER, 0, 4343),
          (GROUP_OBJ, 4, NONE),
          (MASK, 6, NONE),
          (OTHER, 0, NONE),
        ]),
      ),
    );
    assert_eq!(errno(&set), 0, "the guest sets the file's ACL");
    let judged = Judged::of(&mut ask, file.0, reads_acls);
    assert_eq!(judged.mode, 0o660, "the mask is the mode's group bits");
    for (caller, want, granted, what) in [
      (
        &owner,
        MAY_READ | MAY_WRITE,
        true,
        "the owner reads and writes",
      ),
      (&reader, MAY_READ | MAY_WRITE, true, "4242 reads and writes"),
      (&reader, MAY_EXEC, false, "4242 does not execute"),
      (&refused, MAY_READ, false, "4343 does not read"),
      (&member, MAY_READ, true, "the owning group reads"),
      (&member, MAY_WRITE, false, "the owning group does not write"),
      (&stranger, MAY_READ, false, "anyone else reads nothing"),
    ] {
      assert_eq!(judged.grants(caller, want), granted, "{what}");
    }
    reads_acls
  };
  {
    let mut host = VolumeBridge::new(VolumeId { bytes: [0; 16] }, &mut vol, &mut store);
    let mut chmod = vec![0u8; 88];
    chmod[0..4].copy_from_slice(&SetAttrIn::FATTR_MODE.to_le_bytes());
    chmod[SETATTR_MODE_AT..SETATTR_MODE_AT + 4].copy_from_slice(&(S_IFREG | 0o640).to_le_bytes());
    let changed = exchange(
      &mut host,
      &cx,
      &message(Opcode::SetAttr as u32, next(), file.0, &chmod),
    );
    assert_eq!(errno(&changed), 0, "a host's mount chmods the file 0640");
  }
  let mut guest =
    VolumeBridge::new(VolumeId { bytes: [0; 16] }, &mut vol, &mut store).serving_a_guest();
  let mut ask = |opcode: Opcode, nodeid: u64, body: &[u8]| {
    exchange(
      &mut guest,
      &cx,
      &message(opcode as u32, next(), nodeid, body),
    )
  };
  let judged = Judged::of(&mut ask, file.0, reads_acls);
  assert_eq!(
    judged.acl.as_deref(),
    Some(
      &acl(&[
        (USER_OBJ, 6, NONE),
        (USER, 6, 4242),
        (USER, 0, 4343),
        (GROUP_OBJ, 4, NONE),
        (MASK, 4, NONE),
        (OTHER, 0, NONE),
      ])[..]
    ),
    "the host's chmod is the ACL's mask r--, as Linux's posix_acl_chmod makes it"
  );
  assert!(judged.grants(&reader, MAY_READ), "4242 still reads");
  assert!(!judged.grants(&reader, MAY_WRITE), "and no longer writes");
  let removed = ask(
    Opcode::RemoveXattr,
    file.0,
    &removexattr_body(b"system.posix_acl_access"),
  );
  assert_eq!(errno(&removed), 0, "the guest removes the ACL");
  let judged = Judged::of(&mut ask, file.0, reads_acls);
  assert_eq!(
    (judged.mode, judged.acl.as_deref()),
    (0o640, None),
    "a removal leaves the mode as it was"
  );
  assert!(!judged.grants(&reader, MAY_READ), "and grants 4242 nothing");
  let minimal = ask(
    Opcode::SetXattr,
    file.0,
    &setxattr_body(
      b"system.posix_acl_access",
      &acl(&[(USER_OBJ, 7, NONE), (GROUP_OBJ, 4, NONE), (OTHER, 0, NONE)]),
    ),
  );
  assert_eq!(errno(&minimal), 0, "the guest sets a minimal ACL");
  let judged = Judged::of(&mut ask, file.0, reads_acls);
  assert_eq!(
    (judged.mode, judged.acl.as_deref()),
    (0o740, None),
    "a minimal ACL is the mode alone"
  );
}

/// A CREATE (`fuse_create_in`: flags, mode, umask, open flags, then the name) of `name` with `mode` under `umask`.
fn create_body(name: &str, mode: u32, umask: u32) -> Vec<u8> {
  let mut body = Vec::new();
  for word in [O_RDWR_CREAT, S_IFREG | mode, umask, 0] {
    body.extend_from_slice(&word.to_le_bytes());
  }
  body.extend_from_slice(name.as_bytes());
  body.push(0);
  body
}

/// The permission bits of the attributes an entry reply (`fuse_entry_out`) carries.
fn entry_mode(reply: &[u8]) -> u32 {
  let at = ENTRY_ATTR_AT + ATTR_MODE_AT;
  u32::from_le_bytes(reply[at..at + 4].try_into().unwrap()) & PERMISSIONS
}

/// A-314 (a directory's default ACL is what it gives what is made in it, through every route). Do: through the guest's
/// route, set on a directory the default ACL `setfacl -d -m u:4242:rwx,o::-` makes (owner rwx, 4242 rwx, the owning
/// group r-x, mask rwx, everyone else none); create a file in it with mode 0666 under umask 022, make a directory with
/// 0777 under 022 and a FIFO with 0644 under 077, and a symlink; create a file with 0666 under 022 in the root, which has
/// no default ACL; then, through a host's route that carries no umask (as NFS creates), create a file with 0644 in the
/// directory. Expect each umask ignored under the default ACL and each mode masked by it (Linux's `posix_acl_create`):
/// the file 0660 with the default's entries as its access ACL, mask rw- and no default ACL of its own, so 4242 reads and
/// writes it and does not execute it; the directory 0770 with the default as both its access ACL and its default
/// ACL; the FIFO 0640, mask r--; the symlink with no ACL; the root's file 0644 by its umask, with no ACL; and the host's
/// file 0640, mask r--, so 4242 reads it and does not write it, as on a bind mount whose host writes into the guest's
/// directory.
#[test]
fn a_directorys_default_acl_is_what_it_gives_what_is_made_in_it_through_every_route() {
  let mut store = store();
  let mut vol = owned_volume(&mut store);
  let root = vol.root_inode(&store).unwrap();
  let shared = vol.mkdir_no(&mut store, root, "shared", 0o755).unwrap();
  vol.chown(&mut store, shared, RUN.0, RUN.1).unwrap();
  let cx = test_cx();
  let reader = Caller {
    uid: 4242,
    groups: &[4242],
  };
  let default = acl(&[
    (USER_OBJ, 7, NONE),
    (USER, 7, 4242),
    (GROUP_OBJ, 5, NONE),
    (MASK, 7, NONE),
    (OTHER, 0, NONE),
  ]);
  let with_mask_and_owner = |owner_perm: u16, mask: u16| {
    acl(&[
      (USER_OBJ, owner_perm, NONE),
      (USER, 7, 4242),
      (GROUP_OBJ, 5, NONE),
      (MASK, mask, NONE),
      (OTHER, 0, NONE),
    ])
  };
  let mut unique = 0u64;
  let mut next = move || {
    unique += 1;
    unique
  };
  let reads_acls = {
    let mut guest =
      VolumeBridge::new(VolumeId { bytes: [0; 16] }, &mut vol, &mut store).serving_a_guest();
    let mut ask = |opcode: Opcode, nodeid: u64, body: &[u8]| {
      exchange(
        &mut guest,
        &cx,
        &message_as(opcode as u32, next(), nodeid, RUN, body),
      )
    };
    let reads_acls = keeps_posix_acl(&ask(
      Opcode::Init,
      0,
      &init_offering(FUSE_POSIX_ACL | FUSE_DONT_MASK),
    ));
    let set = ask(
      Opcode::SetXattr,
      shared.0,
      &setxattr_body(b"system.posix_acl_default", &default),
    );
    assert_eq!(errno(&set), 0, "the guest sets the directory's default ACL");
    let access_of = |ask: &mut dyn FnMut(Opcode, u64, &[u8]) -> Vec<u8>, node: u64, name: &[u8]| {
      let got = ask(Opcode::GetXattr, node, &getxattr_body(4096, Some(name)));
      (errno(&got) == 0).then(|| got[OUT_HEADER_LEN..].to_vec())
    };

    let file = ask(
      Opcode::Create,
      shared.0,
      &create_body("new.txt", 0o666, 0o022),
    );
    assert_eq!(errno(&file), 0, "CREATE");
    assert_eq!(
      entry_mode(&file),
      0o660,
      "the file's mode is masked by the default ACL, not the umask"
    );
    let file = body_u64(&file);
    assert_eq!(
      access_of(&mut ask, file, b"system.posix_acl_access"),
      Some(with_mask_and_owner(6, 6)),
      "the file takes the default's entries as its access ACL"
    );
    assert_eq!(
      access_of(&mut ask, file, b"system.posix_acl_default"),
      None,
      "a file takes no default ACL"
    );
    let judged = Judged::of(&mut ask, file, reads_acls);
    assert!(
      judged.grants(&reader, MAY_READ | MAY_WRITE),
      "4242 reads and writes the file"
    );
    assert!(!judged.grants(&reader, MAY_EXEC), "and does not execute it");

    let mut mkdir = Vec::new();
    for word in [0o777u32, 0o022] {
      mkdir.extend_from_slice(&word.to_le_bytes());
    }
    mkdir.extend_from_slice(b"sub\0");
    let sub = ask(Opcode::MkDir, shared.0, &mkdir);
    assert_eq!((errno(&sub), entry_mode(&sub)), (0, 0o770), "MKDIR");
    let sub = body_u64(&sub);
    assert_eq!(
      access_of(&mut ask, sub, b"system.posix_acl_access"),
      Some(with_mask_and_owner(7, 7)),
      "the directory takes the default's entries as its access ACL"
    );
    assert_eq!(
      access_of(&mut ask, sub, b"system.posix_acl_default"),
      Some(default.clone()),
      "and the default ACL as its own"
    );

    let mut mknod = Vec::new();
    for word in [S_IFIFO | 0o644, 0, 0o077, 0] {
      mknod.extend_from_slice(&word.to_le_bytes());
    }
    mknod.extend_from_slice(b"pipe\0");
    let pipe = ask(Opcode::MkNod, shared.0, &mknod);
    assert_eq!((errno(&pipe), entry_mode(&pipe)), (0, 0o640), "MKNOD");
    assert_eq!(
      access_of(&mut ask, body_u64(&pipe), b"system.posix_acl_access"),
      Some(with_mask_and_owner(6, 4)),
      "the FIFO takes the default's entries, mask r--"
    );

    let link = ask(Opcode::SymLink, shared.0, b"link\0new.txt\0");
    assert_eq!(errno(&link), 0, "SYMLINK");
    assert_eq!(
      access_of(&mut ask, body_u64(&link), b"system.posix_acl_access"),
      None,
      "a symlink takes no ACL"
    );

    let plain = ask(Opcode::Create, 1, &create_body("plain.txt", 0o666, 0o022));
    assert_eq!(
      (errno(&plain), entry_mode(&plain)),
      (0, 0o644),
      "without a default ACL the umask is applied"
    );
    assert_eq!(
      access_of(&mut ask, body_u64(&plain), b"system.posix_acl_access"),
      None,
      "and no ACL is given"
    );
    reads_acls
  };
  let from_host = {
    use slates_bridge_core::Bridge as _;
    let mut host = VolumeBridge::new(VolumeId { bytes: [0; 16] }, &mut vol, &mut store);
    let (made, _handle) = host
      .create(
        slates_bridge_core::ObjectId::new(shared.0, 0),
        &cx,
        "from-host.txt",
        0o644,
        O_RDWR_CREAT,
      )
      .unwrap();
    assert_eq!(
      made.mode & PERMISSIONS,
      0o640,
      "the host's file is masked by the default ACL"
    );
    made.ino
  };
  let mut guest =
    VolumeBridge::new(VolumeId { bytes: [0; 16] }, &mut vol, &mut store).serving_a_guest();
  let mut ask = |opcode: Opcode, nodeid: u64, body: &[u8]| {
    exchange(
      &mut guest,
      &cx,
      &message(opcode as u32, next(), nodeid, body),
    )
  };
  let judged = Judged::of(&mut ask, from_host, reads_acls);
  assert_eq!(
    judged.acl,
    Some(with_mask_and_owner(6, 4)),
    "the host's file takes the default's entries, mask r--"
  );
  assert!(
    judged.grants(&reader, MAY_READ),
    "4242 reads the host's file"
  );
  assert!(!judged.grants(&reader, MAY_WRITE), "and does not write it");
}
