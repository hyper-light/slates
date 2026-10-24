//! A-160 by use (§4.6): `image build` and `image merge` through a daemon in this process. A volume's snapshot is built
//! as an image in a new volume, read back through the client and parsed as ext2; a guest's edits — a rename, a
//! rewrite, a deletion, a new file at the freed inode number — are written into the image, and the merge makes a new
//! clone of the snapshot hold them. The refusals are typed, and the image volume pins its snapshot until it goes.
// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::indexing_slicing,
  clippy::unwrap_in_result,
  clippy::panic_in_result_fn
)]

use std::collections::BTreeMap;
use std::convert::Infallible;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use slates_client::{
  Client, ClientError, CreateSpec, Deadlines, IMAGE_FILE, Intent, NamePolicy, ReadAt, Refusal,
  SizeClass, VolumeId,
};
use slates_image::format::{BUILT_GENERATION, inode as inode_field};
use slates_image::layout::BLOCK_BYTES;
use slates_image::{
  Builder, Identity, Image, ImageIo, ImageSource, Kind, NodeId, Progress, Timestamp, Tree,
  TreeBuilder,
};
use slates_machine::{MachineError, MachineProfile, ProfileOptions};
use slates_server::daemon::LIVENESS_BUDGET_NS;
use slates_server::{Daemon, DaemonConfig, SegmentSource};

/// Shape: each probe's wall budget for the tests' machine profile (milliseconds).
const PROBE_MS: u64 = 5;
/// Shape: shards per test daemon: two, so a client's shard and a volume's owner may differ.
const TEST_SHARDS: u16 = 2;
/// Shape: the source volume's bounded quota.
const QUOTA: u64 = 8 << 20;
/// Shape: the image's capacity.
const CAPACITY: u64 = 4 << 20;
/// Shape: how long a client retries the rendezvous while its daemon starts, and how long a destroy may take.
const WAIT: Duration = Duration::from_secs(20);
/// Shape: the entries an image read back may hold.
const MOST: usize = 1 << 16;
/// Shape: the generation a guest's kernel gives the file it makes (any but the build's).
const GUEST_GENERATION: u32 = 0x5eed;
/// Shape: the bytes `/docs/sub/b.bin` runs past its third chunk, so its last chunk is partial.
const LARGE_TAIL: usize = 1_234;
/// Shape: where in its second chunk the guest rewrites `/docs/sub/b.bin`, and how many bytes.
const REWRITE: (usize, usize) = (100, 2_000);

/// The machine profile, measured once per process.
fn profile() -> MachineProfile {
  static PROFILE: OnceLock<Result<MachineProfile, MachineError>> = OnceLock::new();
  PROFILE
    .get_or_init(|| {
      MachineProfile::measure_patiently(&ProfileOptions {
        budget_per_probe: Duration::from_millis(PROBE_MS),
        codecs: false,
        core_matrix: false,
      })
    })
    .clone()
    .expect("the machine profile measures")
}

/// A daemon in this process named `tag`, with its consensus group made, a client of it, and the daemon's chunk: the
/// copy-on-write unit a merge compares and writes in (sixteen host pages, `slates_vfs::content::chunk_bytes`).
fn start(tag: &str) -> (Daemon, Client, usize) {
  let profile = profile();
  let instance = format!("img-{tag}-{}", std::process::id());
  let config = DaemonConfig::derive(&profile, &instance, Some(TEST_SHARDS));
  let chunk = slates_vfs::content::chunk_bytes(config.page).get();
  let daemon = Daemon::start(
    &profile,
    config,
    SegmentSource::Create {
      name: format!("slates-seg-img-{tag}-{}", std::process::id()),
    },
  )
  .unwrap();
  daemon
    .bootstrap(true)
    .expect("the fixture creates its local consensus group");
  let deadlines =
    Deadlines::derive(LIVENESS_BUDGET_NS, slates_db::replay::RECOVERY_BUDGET_NS).get();
  let started = Instant::now();
  let client = loop {
    match Client::connect(&instance, deadlines) {
      Ok(client) => break client,
      Err(ClientError::Ipc(slates_ipc::IpcError::DaemonUnavailable { .. }))
        if started.elapsed() < WAIT =>
      {
        std::thread::yield_now();
      }
      Err(e) => panic!("{e}"),
    }
  };
  (daemon, client, chunk)
}

/// The source volume's files: `/docs/a.txt`, `/docs/sub/b.bin` (a pattern three chunks and more long), `/c.txt`, and
/// `/z-last/delete-me`, which the guest deletes.
fn files(chunk: usize) -> Vec<(&'static str, Vec<u8>)> {
  vec![
    ("/docs/a.txt", b"the first file".to_vec()),
    (
      "/docs/sub/b.bin",
      (0..3 * chunk + LARGE_TAIL)
        .map(|at| u8::try_from(at % 251).unwrap())
        .collect(),
    ),
    ("/c.txt", b"see".to_vec()),
    ("/z-last/delete-me", b"gone soon".to_vec()),
  ]
}

/// Makes the source volume with [`files`] and snapshots it.
fn source(client: &mut Client, chunk: usize) -> (VolumeId, slates_client::SnapshotId) {
  let id = client
    .create(&CreateSpec {
      name: "source".to_owned(),
      size: SizeClass::Bounded { limit: QUOTA },
      names: NamePolicy::Exact,
      require_locked: false,
      base: None,
    })
    .unwrap();
  let attachment = client.attach(id, None, Intent::Write).unwrap().attachment;
  for directory in ["/docs", "/docs/sub", "/z-last"] {
    client.fs_mkdir((id, attachment), directory, 0o755).unwrap();
  }
  for (path, bytes) in files(chunk) {
    client
      .fs_write((id, attachment), path, &bytes, 0o644)
      .unwrap();
  }
  client.detach(attachment).unwrap();
  (id, client.snapshot(id).unwrap())
}

/// An image held in memory.
struct Memory {
  bytes: Vec<u8>,
}

impl ImageSource<Infallible> for Memory {
  fn image_len(&self) -> u64 {
    u64::try_from(self.bytes.len()).unwrap()
  }

  fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<(), Infallible> {
    let start = usize::try_from(offset).unwrap();
    buf.copy_from_slice(&self.bytes[start..start + buf.len()]);
    Ok(())
  }
}

/// A tree's files by path, with their bytes read from `image`.
fn files_of(
  tree: &Tree,
  inodes: &[u32],
  image: &mut Image,
  memory: &mut Memory,
) -> BTreeMap<String, Vec<u8>> {
  let mut found = BTreeMap::new();
  let mut stack = vec![(String::new(), NodeId::ROOT)];
  while let Some((prefix, directory)) = stack.pop() {
    for (name, child) in tree.children(directory) {
      let path = format!("{prefix}/{}", String::from_utf8_lossy(name));
      match tree.node(*child).unwrap().kind {
        Kind::Directory => stack.push((path, *child)),
        Kind::File { size } => {
          let mut bytes = vec![0u8; usize::try_from(size).unwrap()];
          image
            .read(memory, inodes[child.index()], 0, &mut bytes)
            .unwrap();
          found.insert(path, bytes);
        }
        _ => {}
      }
    }
  }
  found
}

/// A volume's files by path, read through the client.
fn volume_files(client: &mut Client, volume: VolumeId) -> BTreeMap<String, Vec<u8>> {
  let mut found = BTreeMap::new();
  let mut stack = vec![String::new()];
  while let Some(directory) = stack.pop() {
    let listed = client
      .list_dir(
        volume,
        if directory.is_empty() {
          "/"
        } else {
          &directory
        },
        ReadAt::Head,
      )
      .unwrap();
    for entry in listed {
      let path = format!("{directory}/{}", entry.name);
      match entry.kind {
        slates_client::EntryKind::Dir => stack.push(path),
        slates_client::EntryKind::File => {
          let bytes = client.read(volume, &path, ReadAt::Head).unwrap();
          found.insert(path, bytes);
        }
        _ => {}
      }
    }
  }
  found
}

/// The node `/z-last/delete-me` names in `read`.
fn deleted_node(read: &Tree) -> NodeId {
  read
    .ids()
    .flat_map(|directory| read.children(directory).iter())
    .find(|(name, _)| name == b"delete-me")
    .map(|(_, child)| *child)
    .unwrap()
}

/// The guest's tree: `read` with `/docs/a.txt` renamed `/docs/renamed.txt`, `/docs/sub/b.bin` rewritten in its second
/// chunk, and the deleted file's node replaced by `/new.txt`, so every other node keeps its index (and so its inode
/// number) and the new file takes the deleted one's. Its files' bytes by node.
fn guest_tree(
  read: &Tree,
  contents: &BTreeMap<usize, Vec<u8>>,
  chunk: usize,
) -> (Tree, BTreeMap<usize, Vec<u8>>) {
  let deleted = deleted_node(read);
  let mut builder = TreeBuilder::new(read.node(NodeId::ROOT).unwrap().clone());
  let mut ids = BTreeMap::from([(0usize, NodeId::ROOT)]);
  let mut by_node: BTreeMap<usize, Vec<u8>> = BTreeMap::new();
  for id in read.ids().skip(1) {
    let (mut node, mut bytes) = (
      read.node(id).unwrap().clone(),
      contents.get(&id.index()).cloned(),
    );
    if id == deleted {
      bytes = Some(b"brand new".to_vec());
    }
    if let (Kind::File { size }, Some(held)) = (&mut node.kind, bytes.as_mut()) {
      if held.len() == 3 * chunk + LARGE_TAIL {
        rewrite(held, chunk);
      }
      *size = u64::try_from(held.len()).unwrap();
    }
    let new = builder.add_node(node);
    ids.insert(id.index(), new);
    if let Some(bytes) = bytes {
      by_node.insert(new.index(), bytes);
    }
  }
  for id in read.ids() {
    for (name, child) in read.children(id) {
      let (parent, name) = match name.as_slice() {
        _ if *child == deleted => (NodeId::ROOT, b"new.txt".to_vec()),
        b"a.txt" => (ids[&id.index()], b"renamed.txt".to_vec()),
        _ => (ids[&id.index()], name.clone()),
      };
      builder.add_entry(parent, name, ids[&child.index()]);
    }
  }
  (builder.build().unwrap(), by_node)
}

/// A writer's input and output over files by node and an image in memory.
struct Writing<'a> {
  files: &'a BTreeMap<usize, Vec<u8>>,
  image: &'a mut Memory,
}

impl ImageIo<Infallible> for Writing<'_> {
  fn read_content(
    &mut self,
    node: NodeId,
    offset: u64,
    buf: &mut [u8],
  ) -> Result<usize, Infallible> {
    let bytes = self.files.get(&node.index()).map_or(&[][..], Vec::as_slice);
    let start = usize::try_from(offset).unwrap().min(bytes.len());
    let take = buf.len().min(bytes.len() - start);
    buf[..take].copy_from_slice(&bytes[start..start + take]);
    Ok(take)
  }

  fn write_image(&mut self, offset: u64, bytes: &[u8]) -> Result<(), Infallible> {
    let start = usize::try_from(offset).unwrap();
    self.image.bytes[start..start + bytes.len()].copy_from_slice(bytes);
    Ok(())
  }
}

/// `tree` written as an image of [`CAPACITY`] with file system identity `uuid`.
fn written(tree: Tree, files: &BTreeMap<usize, Vec<u8>>, uuid: [u8; 16]) -> Memory {
  let created = Timestamp {
    secs: 1_700_000_000,
    nanos: 0,
  };
  let mut builder = Builder::new(tree, CAPACITY, Identity { uuid, created }).unwrap();
  let mut memory = Memory {
    bytes: vec![0; usize::try_from(builder.image_bytes()).unwrap()],
  };
  while builder
    .step(
      &mut Writing {
        files,
        image: &mut memory,
      },
      u64::MAX,
    )
    .unwrap()
    == Progress::More
  {}
  memory
}

/// A guest's edits written as an image ([`guest_tree`]), the new file's inode carrying a generation of the guest's
/// own, as a kernel gives the inode it allocates.
fn edited_image(
  read: &Tree,
  inodes: &[u32],
  contents: &BTreeMap<usize, Vec<u8>>,
  (uuid, chunk): ([u8; 16], usize),
) -> Vec<u8> {
  // Every node the build wrote is at inode 10 + its index, so a tree that keeps each node at its index keeps each at
  // its written number; the new file takes the deleted one's index, and so its number.
  for id in read.ids().skip(1) {
    assert_eq!(
      usize::try_from(inodes[id.index()]).unwrap(),
      id.index() + 10
    );
  }
  let (tree, files) = guest_tree(read, contents, chunk);
  let mut memory = written(tree, &files, uuid);
  let new_ino = inodes[deleted_node(read).index()];
  let descriptor = usize::try_from(BLOCK_BYTES).unwrap();
  let table = u32::from_le_bytes(
    memory.bytes[descriptor + 8..descriptor + 12]
      .try_into()
      .unwrap(),
  );
  let at = usize::try_from(table).unwrap() * descriptor
    + (usize::try_from(new_ino).unwrap() - 1) * 256
    + inode_field::GENERATION;
  assert_eq!(
    u32::from_le_bytes(memory.bytes[at..at + 4].try_into().unwrap()),
    BUILT_GENERATION
  );
  memory.bytes[at..at + 4].copy_from_slice(&GUEST_GENERATION.to_le_bytes());
  memory.bytes
}

/// Each file's bytes in an image read back, by node.
fn contents_by_node(
  read: &slates_image::ImageTree,
  image: &mut Image,
  memory: &mut Memory,
) -> BTreeMap<usize, Vec<u8>> {
  let mut contents = BTreeMap::new();
  for id in read.tree.ids() {
    if let Kind::File { size } = read.tree.node(id).unwrap().kind {
      let mut bytes = vec![0u8; usize::try_from(size).unwrap()];
      image
        .read(memory, read.inodes[id.index()], 0, &mut bytes)
        .unwrap();
      contents.insert(id.index(), bytes);
    }
  }
  contents
}

/// The guest's rewrite of `/docs/sub/b.bin`: [`REWRITE`]'s bytes in its second chunk.
fn rewrite(bytes: &mut [u8], chunk: usize) {
  let (at, len) = REWRITE;
  bytes[chunk + at..chunk + at + len].fill(b'#');
}

/// A-160 by use. Do: build a volume's snapshot as an image, read the image file back through the client and parse
/// it; write a guest's edits into the image (a rename, a rewrite of one chunk, a deletion, a new file at the freed
/// inode number) and merge it. Expect the image to hold the volume's files byte for byte; the merge to keep every
/// build node but the deleted one, make one, remove one, move one and write only the rewritten chunk's bytes; and the
/// clone to hold exactly the edited files.
#[test]
fn an_image_is_built_read_back_edited_and_merged_into_a_clone() {
  let (daemon, mut client, chunk) = start("merge");
  let (volume, snapshot) = source(&mut client, chunk);
  let built = client
    .image_build(volume, snapshot, "image", CAPACITY)
    .unwrap();
  assert_eq!(built.image_bytes, CAPACITY);
  let mut memory = Memory {
    bytes: client
      .read(built.id, &format!("/{IMAGE_FILE}"), ReadAt::Head)
      .unwrap(),
  };
  assert_eq!(
    u64::try_from(memory.bytes.len()).unwrap(),
    built.image_bytes
  );
  let mut image = Image::open(&mut memory).unwrap();
  assert_eq!(
    image.uuid(),
    built.id.bytes,
    "the image's identity is its volume's id"
  );
  let read = image.tree(&mut memory, MOST).unwrap();
  let written: BTreeMap<String, Vec<u8>> = files(chunk)
    .into_iter()
    .map(|(path, bytes)| (path.to_owned(), bytes))
    .collect();
  assert_eq!(
    files_of(&read.tree, &read.inodes, &mut image, &mut memory),
    written
  );
  let contents = contents_by_node(&read, &mut image, &mut memory);
  let edited = edited_image(&read.tree, &read.inodes, &contents, (built.id.bytes, chunk));
  let attachment = client
    .attach(built.id, None, Intent::Write)
    .unwrap()
    .attachment;
  client
    .fs_write(
      (built.id, attachment),
      &format!("/{IMAGE_FILE}"),
      &edited,
      0o600,
    )
    .unwrap();
  client.detach(attachment).unwrap();
  let (clone, report) = client.image_merge(built.id, "merged").unwrap();
  let nodes = u64::try_from(read.tree.nodes().len()).unwrap();
  assert_eq!(
    (report.kept, report.created, report.removed, report.moved),
    (nodes - 1, 1, 1, 1),
    "{report:?}"
  );
  // The new file's nine bytes, and the one chunk of the rewritten file whose bytes differ: the rest stays shared.
  assert_eq!(
    report.bytes_written,
    9 + u64::try_from(chunk).unwrap(),
    "{report:?}"
  );
  let mut expected = written;
  expected.remove("/z-last/delete-me");
  let renamed = expected.remove("/docs/a.txt").unwrap();
  expected.insert("/docs/renamed.txt".to_owned(), renamed);
  rewrite(expected.get_mut("/docs/sub/b.bin").unwrap(), chunk);
  expected.insert("/new.txt".to_owned(), b"brand new".to_vec());
  assert_eq!(volume_files(&mut client, clone), expected);
  // The source volume is untouched.
  assert_eq!(volume_files(&mut client, volume).len(), files(chunk).len());
  daemon.stop();
}

/// A-160 (typed refusals). Do: build with a capacity too small for the tree, of a snapshot that does not exist, under
/// a name taken; merge a volume that is not an image, and an image whose bytes are not ext2. Expect each refused by
/// kind, and no volume made by any of them.
#[test]
fn image_verbs_refuse_by_kind_and_make_nothing() {
  let (daemon, mut client, chunk) = start("refuse");
  let (volume, snapshot) = source(&mut client, chunk);
  let image_refused = |result: Result<_, ClientError>| {
    matches!(
      result,
      Err(ClientError::Refused(Refusal::ImageRefused { .. }))
    )
  };
  assert!(image_refused(
    client
      .image_build(volume, snapshot, "tiny", 64 << 10)
      .map(|_| ())
  ));
  let gone = slates_client::SnapshotId {
    value: snapshot.value ^ 1,
  };
  assert_eq!(
    client
      .image_build(volume, gone, "nowhere", CAPACITY)
      .map(|_| ()),
    Err(ClientError::Refused(Refusal::NotFound))
  );
  assert_eq!(
    client
      .image_build(volume, snapshot, "source", CAPACITY)
      .map(|_| ()),
    Err(ClientError::Refused(Refusal::AlreadyExists {
      existing: volume
    }))
  );
  assert!(image_refused(
    client.image_merge(volume, "not-an-image").map(|_| ())
  ));
  let built = client
    .image_build(volume, snapshot, "image", CAPACITY)
    .unwrap();
  let attachment = client
    .attach(built.id, None, Intent::Write)
    .unwrap()
    .attachment;
  client
    .fs_write(
      (built.id, attachment),
      &format!("/{IMAGE_FILE}"),
      &vec![0x5a; 1 << 16],
      0o600,
    )
    .unwrap();
  client.detach(attachment).unwrap();
  assert!(image_refused(
    client.image_merge(built.id, "garbage").map(|_| ())
  ));
  let names: Vec<String> = client.list().unwrap().into_iter().map(|v| v.name).collect();
  assert_eq!(names.len(), 2, "only the source and the image: {names:?}");
  daemon.stop();
}

/// A-160 (the pin). Do: build an image of a snapshot, try to destroy the snapshot, destroy the image volume, and try
/// again. Expect the first destroy refused while the image volume holds its pin, and the second to succeed once it
/// is gone.
#[test]
fn an_image_volume_pins_its_snapshot_until_it_goes() {
  let (daemon, mut client, chunk) = start("pin");
  let (volume, snapshot) = source(&mut client, chunk);
  let built = client
    .image_build(volume, snapshot, "image", CAPACITY)
    .unwrap();
  assert!(matches!(
    client.destroy_snapshot(volume, snapshot),
    Err(ClientError::Refused(_))
  ));
  client.destroy(built.id).unwrap();
  let started = Instant::now();
  loop {
    match client.destroy_snapshot(volume, snapshot) {
      Ok(()) => break,
      Err(_) if started.elapsed() < WAIT => std::thread::yield_now(),
      Err(error) => panic!("the snapshot stayed pinned: {error}"),
    }
  }
  daemon.stop();
}
