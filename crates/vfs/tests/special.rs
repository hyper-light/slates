//! A-26, A-294 / §4.5: FIFO and socket names and device nodes obey namespace and copy-on-write rules without acquiring
//! file contents, and a device node keeps its number through every form a volume takes.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use slates_vfs::VfsError;
use slates_vfs::clock::StepClock;
use slates_vfs::inode::{DeviceNumber, SpecialNode};
use slates_vfs::recover::{BodyImage, VolumeImage};
use slates_vfs::volume::{Store, Volume};

/// The names every test here makes: both IPC kinds (A-26) and device nodes (A-294): a block device, a whiteout (a
/// character device numbered 0/0, as overlayfs writes one) and the widest number NFS can carry, past the Linux
/// kernel's 12-bit major and 20-bit minor.
fn every_node() -> [(&'static str, SpecialNode); 5] {
  [
    ("pipe", SpecialNode::Fifo),
    ("socket", SpecialNode::Socket),
    ("disk", SpecialNode::BlockDevice(DeviceNumber::new(8, 1))),
    ("whiteout", SpecialNode::CharDevice(DeviceNumber::new(0, 0))),
    (
      "widest",
      SpecialNode::CharDevice(DeviceNumber::new(u32::MAX, u32::MAX)),
    ),
  ]
}

/// AC-1.15 / A-26: derivation names the IPC kind, attributes and shared inode without content hunks.
#[test]
fn ipc_derivation_preserves_kind_metadata_and_link_identity() {
  let mut store = common::store();
  let mut volume = common::volume(&mut store, 1 << 24);
  let base = volume.snapshot(&mut store).unwrap();
  let root = volume.root_inode(&store).unwrap();
  let pipe = volume
    .mknod_no(&mut store, root, "pipe", 0o640, SpecialNode::Fifo)
    .unwrap();
  volume.link_no(&mut store, root, "pipe-link", pipe).unwrap();
  volume.chown(&mut store, pipe, 123, 456).unwrap();
  let document = volume.derive(&store, base).unwrap();
  assert_ipc_derived_metadata(&document, SpecialNode::Fifo);
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

/// AC-1.15 / A-294: do derive a volume that made a device node with a second name, and another that made the same
/// node with another number; expect each delta to carry its node's kind and number on both names, with no content
/// operation, and the two documents to differ, since a device's number is part of what changed.
#[test]
fn a_device_nodes_derivation_carries_its_number() {
  let derived = |number: DeviceNumber| {
    let mut store = common::store();
    let mut volume = common::volume(&mut store, 1 << 24);
    let base = volume.snapshot(&mut store).unwrap();
    let root = volume.root_inode(&store).unwrap();
    let disk = volume
      .mknod_no(
        &mut store,
        root,
        "disk",
        0o640,
        SpecialNode::BlockDevice(number),
      )
      .unwrap();
    volume.link_no(&mut store, root, "disk-link", disk).unwrap();
    volume.chown(&mut store, disk, 123, 456).unwrap();
    volume.derive(&store, base).unwrap()
  };
  let first = derived(DeviceNumber::new(8, 1));
  assert_ipc_derived_metadata(&first, SpecialNode::BlockDevice(DeviceNumber::new(8, 1)));
  let second = derived(DeviceNumber::new(8, 2));
  assert_ipc_derived_metadata(&second, SpecialNode::BlockDevice(DeviceNumber::new(8, 2)));
  assert_ne!(first.encode(), second.encode());
}

/// The oracle observes the delta's metadata and proves that no content operation was invented.
fn assert_ipc_derived_metadata(document: &slates_vfs::derive::OpsDocument, node: SpecialNode) {
  assert!(document.files.is_empty());
  assert_eq!(document.specials.len(), 2);
  for special in &document.specials {
    assert_eq!(special.node, node);
    assert_eq!(
      (
        special.attrs.uid,
        special.attrs.gid,
        special.attrs.nlink,
        special.attrs.size
      ),
      (123, 456, 2, 0)
    );
  }
}

/// AC-1.6 / A-26, A-294: links, rename, metadata and unlink preserve each frozen namespace after recovery, a device
/// node's number with it.
#[test]
fn ipc_names_survive_links_snapshots_clones_and_recovery() {
  let mut store = common::store();
  let mut volume = common::volume(&mut store, 1 << 24);
  let root = volume.root_inode(&store).unwrap();
  for (name, node) in every_node() {
    let inode = volume
      .mknod_no(&mut store, root, name, 0o640, node)
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
  // A clone shares its origin snapshot's records (A-64), so it recovers beside its recovered origin.
  let origin_image = volume.to_image(&store, None).unwrap();
  let clone_image = VolumeImage::from_content(&bytes).unwrap();
  let mut fresh = common::surviving(&store);
  let claims = common::claims(&mut fresh, &[&origin_image, &clone_image]);
  let recovered_origin = Volume::from_image(
    &mut fresh,
    &origin_image,
    &claims,
    Box::new(StepClock::new(0, 1)),
    1 << 16,
    None,
  )
  .unwrap();
  let recovered = Volume::clone_from_image(
    &mut fresh,
    &clone_image,
    &claims,
    &recovered_origin,
    snapshot,
    Box::new(StepClock::new(0, 1)),
    1 << 16,
    None,
  )
  .unwrap();
  expect_every_node_recovered_with_its_number(&recovered, &fresh);
  assert_eq!(
    recovered.to_image(&fresh, None).unwrap().to_content(),
    bytes
  );
  let (_, frozen_root) = volume.snapshot_info(snapshot).unwrap();
  assert_eq!(volume.readdir_in(&store, frozen_root).unwrap().len(), 10);
  assert!(volume.resolve_in(&store, snapshot, "/socket").is_ok());
  let disk = volume.resolve_in(&store, snapshot, "/disk").unwrap().inode;
  assert_eq!(
    volume.device_in(&store, snapshot, disk).unwrap(),
    Some(DeviceNumber::new(8, 1))
  );
}

/// The recovered clone of `ipc_names_survive_links_snapshots_clones_and_recovery`: expect every node in `recovered`
/// (over `store`) of its kind, with its device number, owner and link count (the moved socket's one link, every other
/// node's two), and empty.
fn expect_every_node_recovered_with_its_number(recovered: &Volume, store: &Store) {
  for (name, node) in every_node() {
    let (name, links) = match name {
      "socket" => ("moved", 1),
      name => (name, 2),
    };
    let inode = recovered.resolve(store, name).unwrap().inode;
    let observed = recovered.observe(store, inode).unwrap();
    assert_eq!(observed.kind, node.kind());
    assert_eq!(observed.device, node.device(), "{name} keeps its number");
    let attrs = observed.attrs;
    assert_eq!(
      (attrs.uid, attrs.gid, attrs.nlink, attrs.size),
      (123, 456, links, 0)
    );
  }
}

/// AC-3.10 / A-26, A-294: regular-file operations refuse IPC names and device nodes before changing their state, so
/// no route reads or writes a device node's number as content.
#[test]
fn ipc_names_never_acquire_regular_file_contents() {
  let mut store = common::store();
  let mut volume = common::volume(&mut store, 1 << 24);
  let root = volume.root_inode(&store).unwrap();
  for (name, node) in every_node() {
    let inode = volume
      .mknod_no(&mut store, root, name, 0o600, node)
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

/// AC-7.3 / A-26, A-294: an archive carries IPC kinds, owners and device numbers without reading or hashing a
/// stream, and every node that is no device carries the number zero.
#[test]
fn archives_preserve_ipc_metadata_without_stream_bytes() {
  use slates_archive::codec::CodecPolicy;
  use slates_vfs::export::{Progress, SnapshotArchiver, kind_of_mode};
  let mut store = common::store();
  let mut volume = common::volume(&mut store, 1 << 24);
  let root = volume.root_inode(&store).unwrap();
  for (name, node) in every_node() {
    let inode = volume
      .mknod_no(&mut store, root, name, 0o640, node)
      .unwrap();
    volume.chown(&mut store, inode, 123, 456).unwrap();
  }
  let snapshot = volume.snapshot(&mut store).unwrap();
  let mut archiver =
    SnapshotArchiver::new(&volume, &store, snapshot, 0, 0, CodecPolicy::raw_only()).unwrap();
  let mut result = None;
  // Each leaf entry and the root close, each charged one content chunk.
  for _ in 0..=every_node().len() {
    if let Progress::Done(archive) = archiver.advance(&volume, &store, 1).unwrap() {
      result = Some(archive);
      break;
    }
  }
  let archive = result.expect("bounded walk completes");
  // The archive's own encoding carries the numbers: decoded, it restores the same.
  let archive = slates_archive::Archive::decode(&archive.encode()).unwrap();
  let restored = slates_archive::restore(&archive, u64::MAX).unwrap();
  for (name, node) in every_node() {
    let meta = &restored.metadata[name];
    assert_eq!(kind_of_mode(meta.mode), Some(node.kind()));
    assert_eq!((meta.uid, meta.gid), (123, 456));
    let number = node.device().unwrap_or_default();
    assert_eq!(
      (meta.device_major, meta.device_minor),
      (number.major, number.minor)
    );
    assert_eq!(restored.files[name].len, 0);
  }
}

/// T-2.1 / A-26: a foreign recovery image cannot smuggle content into a FIFO inode.
#[test]
fn recovery_refuses_content_attached_to_an_ipc_name() {
  let mut store = common::store();
  let mut volume = common::volume(&mut store, 1 << 24);
  let root = volume.root_inode(&store).unwrap();
  let pipe = volume
    .mknod_no(&mut store, root, "pipe", 0o600, SpecialNode::Fifo)
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
  assert_eq!(recovered(&store, &image), Err(VfsError::RecoveryIncomplete));
}

/// T-2.1 / A-294, hostile input: do recover images in which a device node's body is content, a FIFO's body is a
/// device number, a device node has a size, and a file's body is a device number; expect each refused typed, never a
/// device node holding bytes or a FIFO naming a device; and the untouched image recovered, as the control.
#[test]
fn recovery_refuses_a_device_body_where_none_belongs() {
  let mut store = common::store();
  let mut volume = common::volume(&mut store, 1 << 24);
  let root = volume.root_inode(&store).unwrap();
  let disk = volume
    .mknod_no(
      &mut store,
      root,
      "disk",
      0o600,
      SpecialNode::BlockDevice(DeviceNumber::new(8, 1)),
    )
    .unwrap();
  let pipe = volume
    .mknod_no(&mut store, root, "pipe", 0o600, SpecialNode::Fifo)
    .unwrap();
  let file = volume
    .create_file_no(&mut store, root, "file", 0o600)
    .unwrap();
  let image = volume.to_image(&store, None).unwrap();
  assert!(
    recovered(&store, &image).is_ok(),
    "the untouched image recovers"
  );
  let device_body = BodyImage::Device { major: 8, minor: 1 };
  // One corruption of the image: the inode it lands on, and what it does to that inode's image.
  type Corruption<'a> = (u64, &'a dyn Fn(&mut slates_vfs::recover::InodeImage));
  let corruptions: [Corruption<'_>; 4] = [
    (disk.0, &|inode| {
      inode.body = BodyImage::Inline {
        bytes: b"sector".to_vec(),
      }
    }),
    (pipe.0, &|inode| inode.body = device_body.clone()),
    (disk.0, &|inode| inode.attrs.size = 1),
    (file.0, &|inode| inode.body = device_body.clone()),
  ];
  for (no, corrupt) in corruptions {
    let mut hostile = image.clone();
    corrupt(
      hostile
        .inodes
        .iter_mut()
        .find(|inode| inode.no == no)
        .unwrap(),
    );
    assert_eq!(
      recovered(&store, &hostile),
      Err(VfsError::RecoveryIncomplete)
    );
  }
}

/// Recovers `image` into a fresh store beside `store`'s surviving content: the volume, or its refusal.
fn recovered(store: &slates_vfs::volume::Store, image: &VolumeImage) -> Result<(), VfsError> {
  let mut fresh = common::surviving(store);
  let claims = common::claims(&mut fresh, &[image]);
  Volume::from_image(
    &mut fresh,
    image,
    &claims,
    Box::new(StepClock::new(0, 1)),
    1 << 16,
    None,
  )
  .map(|_| ())
}

/// A-294's golden vectors for the Linux kernel's device encoding (`new_encode_dev`), which a FUSE attribute and an
/// ext4 inode carry: do encode a number whose minor uses its low byte only, one using its high bits, and the widest
/// the kernel holds; expect each word the kernel's formula gives and each decoded back to its number, and a number
/// past the kernel's 12-bit major or 20-bit minor refused, never truncated.
#[test]
fn a_device_numbers_linux_encoding_is_its_golden_vector() {
  for (major, minor, word) in [
    (8, 1, 0x0000_0801u32),
    (0x123, 0x4_5678, 0x4561_2378),
    (0xfff, 0xf_ffff, 0xffff_ffff),
    (0, 0, 0),
  ] {
    let number = DeviceNumber::new(major, minor);
    assert_eq!(number.to_linux(), Some(word), "{major}:{minor}");
    assert_eq!(DeviceNumber::from_linux(word), number);
  }
  assert_eq!(DeviceNumber::new(1 << 12, 0).to_linux(), None);
  assert_eq!(DeviceNumber::new(0, 1 << 20).to_linux(), None);
}

/// Exports the snapshot `snapshot` of `volume` to completion, or its refusal.
fn export_whole(
  volume: &Volume,
  store: &slates_vfs::volume::Store,
  snapshot: slates_vfs::ids::SnapshotId,
) -> Result<slates_archive::Archive, VfsError> {
  use slates_archive::codec::CodecPolicy;
  use slates_vfs::export::{Progress, SnapshotArchiver};
  let mut archiver = SnapshotArchiver::new(volume, store, snapshot, 0, 0, CodecPolicy::raw_only())?;
  loop {
    if let Progress::Done(archive) = archiver.advance(volume, store, u64::MAX)? {
      return Ok(archive);
    }
  }
}

/// A volume holding a directory chain `depth` components deep with one file at its bottom (a path of
/// `depth + 1` components), snapshotted.
fn nested(
  depth: usize,
) -> (
  slates_vfs::volume::Store,
  Volume,
  slates_vfs::ids::SnapshotId,
) {
  let mut store = common::store();
  let mut volume = common::volume(&mut store, 1 << 24);
  let mut dir = volume.root_inode(&store).unwrap();
  for _ in 0..depth {
    dir = volume.mkdir_no(&mut store, dir, "d", 0o755).unwrap();
  }
  volume.create_file_no(&mut store, dir, "f", 0o644).unwrap();
  let snapshot = volume.snapshot(&mut store).unwrap();
  (store, volume, snapshot)
}

/// §4.11 (the manifest's depth bound, AUD-29-13): do: export a volume whose deepest path has
/// `MAX_DEPTH` components, and one whose deepest has one more; expect the first to export and decode, and
/// the second refused typed at the export (`TreeTooDeep`) — never an archive every reader refuses. Until
/// 2026-09-30 the exporter emitted it, and its placement and takeover failed at every holder.
#[test]
fn an_export_deeper_than_the_manifest_bound_is_refused_typed() {
  let bound = slates_archive::manifest::MAX_DEPTH;
  let (store, volume, snapshot) = nested(bound - 1);
  let archive = export_whole(&volume, &store, snapshot).expect("the bound exports");
  slates_archive::Archive::decode(&archive.encode()).expect("and decodes");
  let (store, volume, snapshot) = nested(bound);
  assert_eq!(
    export_whole(&volume, &store, snapshot).err(),
    Some(VfsError::TreeTooDeep { limit: bound })
  );
}

/// How much more a byte is worth than neutral in the test below. The archive crate's own cost-model
/// test uses the same shape and the same constant (`PRECIOUS_BYTES` in
/// `crates/archive/tests/archive.rs`): at neutral worth a chunk read once stays raw, which that test
/// asserts outright, so forcing the encode path means saying a byte is scarce. Derived: the measured
/// break-even on this machine is 159x neutral for the cheapest codec (2026-10-20, GAPS §8), so the
/// multiplier has to clear that with room for a slower box; 10_000 is the archive suite's choice and
/// is kept identical so the two tests cannot drift apart.
const BYTES_ARE_SCARCE: u64 = 10_000;

/// §4.11, D-17: the archiver must honour a policy that says compress, and the archive must still
/// restore byte for byte. Do: write highly compressible text into a volume, snapshot it, and export
/// the snapshot with a profile-shaped policy under which a codec pays. Expect: at least one chunk
/// comes back encoded and smaller than its raw length, and the restored file equals what was written.
///
/// Until 2026-10-20 `SnapshotArchiver` had never run with a policy that compresses. Both tests that
/// drove it passed `CodecPolicy::raw_only()`, and the daemon's own derived policy decides raw on
/// every machine measured so far — its value of a byte is anchored to memcpy bandwidth, which puts
/// the cheapest codec about 159x from paying. `crates/archive/tests/archive.rs` covers the encode
/// decision at `Archive::chunk_with`; nothing covered it through the volume walk, so the first build
/// whose policy chose a codec would have run this path in production first.
#[test]
fn the_archiver_encodes_when_the_policy_says_to_and_the_archive_still_restores() {
  use slates_archive::codec::{CodecPolicy, CodecRate, NEUTRAL_PERMILLE};
  use slates_archive::format::Encoding;
  use slates_vfs::export::{Progress, SnapshotArchiver};

  let mut store = common::store();
  let mut volume = common::volume(&mut store, 1 << 24);
  let root = volume.root_inode(&store).unwrap();
  let inode = volume
    .create_file_no(&mut store, root, "text", 0o644)
    .unwrap();
  // Repeated structured text, so the decision is about worth and not about whether the bytes
  // compress; big enough to span more than one content chunk on a 4 KiB page as well as a 16 KiB one.
  let text: Vec<u8> = b"the quick brown fox jumps over the lazy dog. ".repeat(2000);
  volume.write(&mut store, inode, 0, &text).unwrap();
  let snapshot = volume.snapshot(&mut store).unwrap();

  // The shape the boot probe measures, as one measured box's numbers, with a byte priced scarce.
  let policy = CodecPolicy {
    byte_ns_scaled: CodecPolicy::byte_worth_from_memcpy(10_000_000_000),
    lz4: Some(CodecRate::from_throughput(
      0,
      700_000_000,
      3_000_000_000,
      476,
    )),
    zstd: vec![
      CodecRate::from_throughput(1, 400_000_000, 1_200_000_000, 346),
      CodecRate::from_throughput(3, 250_000_000, 1_100_000_000, 320),
    ],
    value_of_byte_permille: NEUTRAL_PERMILLE.saturating_mul(BYTES_ARE_SCARCE),
    value_of_cpu_permille: NEUTRAL_PERMILLE,
    expected_reads: 1,
  };

  let mut archiver = SnapshotArchiver::new(&volume, &store, snapshot, 0, 0, policy).unwrap();
  let archive = loop {
    if let Progress::Done(done) = archiver.advance(&volume, &store, u64::MAX).unwrap() {
      break done;
    }
  };

  let shapes: Vec<_> = archive
    .chunks
    .iter()
    .map(|chunk| (chunk.encoding, chunk.raw_len, chunk.stored_len))
    .collect();
  let encoded = archive
    .chunks
    .iter()
    .filter(|chunk| chunk.encoding != Encoding::Raw);
  let mut encoded_count = 0u32;
  for chunk in encoded {
    encoded_count = encoded_count.saturating_add(1);
    assert!(
      chunk.stored_len < chunk.raw_len,
      "an encoded chunk is smaller than its raw bytes: {chunk:?}"
    );
  }
  assert!(
    encoded_count > 0,
    "the walk encoded at least one chunk under a policy where a codec pays; chunks: {shapes:?}"
  );

  // The whole point of compressing is that it is invisible to a reader.
  let restored = slates_archive::restore(&archive, u64::MAX).unwrap();
  let file = &restored.files["text"];
  assert_eq!(file.len, text.len() as u64, "the restored length");
  let mut rebuilt = vec![0u8; text.len()];
  for (at, bytes) in &file.pieces {
    let start = usize::try_from(*at).unwrap();
    rebuilt[start..start + bytes.len()].copy_from_slice(bytes);
  }
  assert_eq!(rebuilt, text, "the archive restores the bytes exactly");
  assert!(
    restored.chunks_decoded > 0,
    "and decoding actually ran (the decode-once counter moved)"
  );
}
