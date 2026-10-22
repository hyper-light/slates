//! What a mount's kernel holds of a volume, read from the requests and delivered replies of a real volume bridge (§4.6
//! "Cache posture"; A-141;
//! `docs/bugs/2026-10-22-a-mount-that-falls-behind-the-journal-never-hears-of-the-changes-it-missed.md`). Every reply
//! that hands the kernel a lookup names its entry, a `FORGET` gives lookups back, and the mount's own unlink and rename
//! take names away or move them; an error reply changes nothing. A mount that falls behind the journal invalidates
//! exactly what these leave.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::indexing_slicing
)]

use slates_bridge_core::{Attachments, Bridge, ObjectId, OpContext, Rights, View};
use slates_bridge_fuse::abi::{IN_HEADER_LEN, Opcode};
use slates_bridge_fuse::bridge::{KernelChange, dispatch, kernel_changes};
use slates_bridge_fuse::coherence::KernelHolds;
use slates_bridge_fuse::request::RenameIn;
use slates_bridge_fuse::volume_bridge::VolumeBridge;
use slates_db::catalog::{Principal, VolumeId};

mod common;
use common::{store, volume};

/// Format: the reply buffer — far above any reply these tests make.
const REPLY_BYTES: usize = 8192;
/// Format: `S_IFIFO | 0644`, the one node type `MKNOD` makes here.
const FIFO_MODE: u32 = 0o010_644;
/// Format: `S_IFREG | 0644`.
const FILE_MODE: u32 = 0o100_644;
/// Format: `O_RDWR`.
const O_RDWR: u32 = 2;

fn cx() -> OpContext {
  let mut attachments = Attachments::new();
  let id = attachments
    .attach(
      VolumeId { bytes: [0; 16] },
      View::Current,
      Principal::Uid { uid: 0 },
      Rights {
        read: true,
        write: true,
      },
    )
    .unwrap();
  attachments.set_coherence(id, slates_bridge_core::CacheCoherence::Invalidated);
  attachments.context(id).unwrap()
}

/// A request as the kernel sends it: the header naming `nodeid`, then `body`.
fn message(opcode: Opcode, nodeid: u64, body: &[u8]) -> Vec<u8> {
  let total = IN_HEADER_LEN + body.len();
  let mut m = vec![0u8; total];
  m[0..4].copy_from_slice(&u32::try_from(total).unwrap().to_le_bytes());
  m[4..8].copy_from_slice(&opcode.to_wire().to_le_bytes());
  m[8..16].copy_from_slice(&1u64.to_le_bytes());
  m[16..24].copy_from_slice(&nodeid.to_le_bytes());
  m[IN_HEADER_LEN..].copy_from_slice(body);
  m
}

/// `fixed` bytes (the opcode's fixed part, its first word `first`), then each name NUL-terminated.
fn body(fixed: usize, first: u32, names: &[&str]) -> Vec<u8> {
  let mut b = vec![0u8; fixed];
  if fixed >= 4 {
    b[..4].copy_from_slice(&first.to_le_bytes());
  }
  for name in names {
    b.extend_from_slice(name.as_bytes());
    b.push(0);
  }
  b
}

/// `fuse_create_in` (flags, mode, umask, open_flags), then the name.
fn create(name: &str) -> Vec<u8> {
  let mut b = body(16, O_RDWR, &[name]);
  b[4..8].copy_from_slice(&FILE_MODE.to_le_bytes());
  b
}

/// `fuse_read_in` for a directory page of up to `size` bytes from the start.
fn page(size: u32) -> Vec<u8> {
  let mut b = vec![0u8; 40];
  b[16..20].copy_from_slice(&size.to_le_bytes());
  b
}

/// Serves `request` through the real dispatch and hands back the reply, as the transport would deliver it.
fn serve(bridge: &mut dyn Bridge, cx: &OpContext, request: &[u8]) -> Vec<u8> {
  let mut out = vec![0u8; REPLY_BYTES];
  let len = dispatch(request, bridge, cx, &mut out);
  out.truncate(len);
  out
}

/// The changes `request`, answered by `reply`, made to what the kernel holds, as owned values to compare.
fn changes_of(request: &[u8], reply: &[u8]) -> Vec<String> {
  let mut changes = Vec::new();
  kernel_changes(request, reply, &mut |change| {
    changes.push(format!("{change:?}"))
  });
  changes
}

fn handed(parent: u64, name: &str, node: u64) -> String {
  format!(
    "{:?}",
    KernelChange::HandedOut {
      parent,
      name: Some(name),
      node
    }
  )
}

/// Runs `f` over a real volume bridge whose root holds one file, `a`: the bridge, a read-write context, the root's node
/// id and `a`'s.
fn with_bridge(f: impl FnOnce(&mut dyn Bridge, &OpContext, u64, u64)) {
  let mut store = store();
  let mut vol = volume(&mut store);
  let mut bridge = VolumeBridge::new(VolumeId { bytes: [0; 16] }, &mut vol, &mut store);
  let cx = cx();
  let root = bridge.root(&cx).unwrap();
  let (a, fh) = bridge
    .create(ObjectId::new(root, 0), &cx, "a", 0o644, 0)
    .unwrap();
  bridge.release(ObjectId::new(a.ino, 0), &cx, fh).unwrap();
  f(&mut bridge, &cx, root, a.ino);
}

/// Serves `request`, then checks that its reply handed out exactly `name` in `parent` for the node the name now names,
/// and returns that node.
fn hands_out(
  bridge: &mut dyn Bridge,
  cx: &OpContext,
  request: &[u8],
  parent: u64,
  name: &str,
) -> u64 {
  let reply = serve(bridge, cx, request);
  let node = bridge
    .lookup(ObjectId::new(parent, 0), cx, name)
    .unwrap()
    .ino;
  assert_eq!(
    changes_of(request, &reply),
    vec![handed(parent, name, node)],
    "{name}"
  );
  node
}

/// Every reply that hands the kernel a lookup names its entry. Do: through a real volume bridge, `LOOKUP` a file,
/// `MKDIR` a directory, `CREATE` a file in it, `MKNOD` a fifo, `SYMLINK`, and `LINK` the created file under a second
/// name. Expect each reply read as the one (directory, name, node) it handed out, the link naming the created file's
/// node.
#[test]
fn every_entry_reply_names_what_it_handed_out() {
  with_bridge(|bridge, cx, root, a| {
    let lookup = message(Opcode::Lookup, root, &body(0, 0, &["a"]));
    assert_eq!(hands_out(bridge, cx, &lookup, root, "a"), a);
    let mkdir = message(Opcode::MkDir, root, &body(8, 0o755, &["d"]));
    let d = hands_out(bridge, cx, &mkdir, root, "d");
    let f = hands_out(
      bridge,
      cx,
      &message(Opcode::Create, d, &create("f")),
      d,
      "f",
    );
    let mknod = message(Opcode::MkNod, root, &body(16, FIFO_MODE, &["p"]));
    hands_out(bridge, cx, &mknod, root, "p");
    let symlink = message(Opcode::SymLink, root, &body(0, 0, &["s", "a"]));
    hands_out(bridge, cx, &symlink, root, "s");
    let mut link_body = f.to_le_bytes().to_vec();
    link_body.extend_from_slice(b"g\0");
    let link = message(Opcode::Link, d, &link_body);
    assert_eq!(
      hands_out(bridge, cx, &link, d, "g"),
      f,
      "a link names the same node"
    );
  });
}

/// A directory page hands out every entry but "." and "..", a tmpfile hands out its node under no name, and an error
/// reply hands out nothing. Do: `READDIRPLUS` the root, `TMPFILE`, and `LOOKUP` a missing name. Expect `a` alone from
/// the page, one unnamed node from the tmpfile, and nothing from the miss.
#[test]
fn a_directory_page_and_a_tmpfile_hand_out_their_entries_and_an_error_nothing() {
  with_bridge(|bridge, cx, root, a| {
    let plus = message(Opcode::ReadDirPlus, root, &page(4096));
    let reply = serve(bridge, cx, &plus);
    assert_eq!(
      changes_of(&plus, &reply),
      vec![handed(root, "a", a)],
      "every entry but \".\" and \"..\""
    );
    let tmpfile = message(Opcode::TmpFile, root, &create("/"));
    let reply = serve(bridge, cx, &tmpfile);
    let changes = changes_of(&tmpfile, &reply);
    assert!(
      changes.len() == 1 && changes[0].contains("name: None"),
      "a tmpfile is held under no name: {changes:?}"
    );
    let missing = message(Opcode::Lookup, root, &body(0, 0, &["absent"]));
    let reply = serve(bridge, cx, &missing);
    assert!(
      changes_of(&missing, &reply).is_empty(),
      "an error reply hands out nothing"
    );
  });
}

/// What a mount's own requests take away, by use, and what is left for an overrun to invalidate. Do: hand the kernel
/// four names (two in a directory); then the mount's own `UNLINK` of one, `RENAME` of another, an exchanging `RENAME2`,
/// a `FORGET` and a `BATCH_FORGET`, each fed to a `KernelHolds` as the transport feeds it after delivery. Expect the
/// holdings after each to be exactly the nodes with lookups left and the names the kernel still has, moved and swapped
/// as the renames did; and a node forgotten to zero gone with every name it had.
#[test]
fn a_mounts_own_requests_take_away_what_they_take() {
  let mut store = store();
  let mut vol = volume(&mut store);
  let mut bridge = VolumeBridge::new(VolumeId { bytes: [0; 16] }, &mut vol, &mut store);
  let cx = cx();
  let root = bridge.root(&cx).unwrap();
  let mut holds = KernelHolds::default();
  let feed = |request: &[u8], reply: &[u8], holds: &mut KernelHolds| {
    kernel_changes(request, reply, &mut |change| match change {
      KernelChange::HandedOut { parent, name, node } => holds.handed_out(parent, name, node),
      KernelChange::Forgot { node, count } => holds.forgot(node, count),
      KernelChange::Unnamed { parent, name } => holds.unnamed(parent, name),
      KernelChange::Renamed { from, to, exchange } => holds.renamed(from, to, exchange),
    });
  };

  let mut nodes = Vec::new();
  for name in ["w", "x", "y"] {
    let request = message(Opcode::Create, root, &create(name));
    let reply = serve(&mut bridge, &cx, &request);
    feed(&request, &reply, &mut holds);
    nodes.push(
      bridge
        .lookup(ObjectId::new(root, 0), &cx, name)
        .unwrap()
        .ino,
    );
  }
  let request = message(Opcode::MkDir, root, &body(8, 0o755, &["e"]));
  let reply = serve(&mut bridge, &cx, &request);
  feed(&request, &reply, &mut holds);
  let e = bridge.lookup(ObjectId::new(root, 0), &cx, "e").unwrap().ino;
  let (w, x, y) = (nodes[0], nodes[1], nodes[2]);
  let names = |holds: &KernelHolds| holds.everything().1;

  let unlink = message(Opcode::Unlink, root, &body(0, 0, &["w"]));
  let reply = serve(&mut bridge, &cx, &unlink);
  feed(&unlink, &reply, &mut holds);
  assert_eq!(
    names(&holds),
    vec![
      (root, "e".to_owned()),
      (root, "x".to_owned()),
      (root, "y".to_owned())
    ],
    "the unlinked name is gone"
  );

  let mut rename_body = e.to_le_bytes().to_vec();
  rename_body.extend_from_slice(b"x\0x2\0");
  let rename = message(Opcode::Rename, root, &rename_body);
  let reply = serve(&mut bridge, &cx, &rename);
  feed(&rename, &reply, &mut holds);
  assert_eq!(
    names(&holds),
    vec![
      (root, "e".to_owned()),
      (root, "y".to_owned()),
      (e, "x2".to_owned())
    ],
    "the renamed name moved into the directory"
  );

  let mut exchange_body = root.to_le_bytes().to_vec();
  exchange_body.extend_from_slice(&RenameIn::RENAME_EXCHANGE.to_le_bytes());
  exchange_body.extend_from_slice(&0u32.to_le_bytes());
  exchange_body.extend_from_slice(b"x2\0y\0");
  let exchange = message(Opcode::Rename2, e, &exchange_body);
  let reply = serve(&mut bridge, &cx, &exchange);
  feed(&exchange, &reply, &mut holds);
  let swapped = names(&holds);
  assert_eq!(
    swapped,
    vec![
      (root, "e".to_owned()),
      (root, "y".to_owned()),
      (e, "x2".to_owned())
    ],
    "an exchange keeps both names"
  );
  assert_eq!(
    holds.everything().0,
    {
      let mut all = vec![w, x, y, e];
      all.sort_unstable();
      all
    },
    "an unlink or a rename takes no lookup away"
  );

  let forget = message(Opcode::Forget, w, &1u64.to_le_bytes());
  feed(&forget, &[], &mut holds);
  let mut batch = 2u32.to_le_bytes().to_vec();
  batch.extend_from_slice(&0u32.to_le_bytes());
  for node in [x, y] {
    batch.extend_from_slice(&node.to_le_bytes());
    batch.extend_from_slice(&1u64.to_le_bytes());
  }
  let batch_forget = message(Opcode::BatchForget, 0, &batch);
  feed(&batch_forget, &[], &mut holds);
  assert_eq!(
    holds.everything(),
    (vec![e], vec![(root, "e".to_owned())]),
    "the forgotten nodes go, and every name they had with them"
  );
}
