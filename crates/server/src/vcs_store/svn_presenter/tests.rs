//! A-645 P3: the presenter's volume against the view `slates_svn::fsfs` makes, byte for byte.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::indexing_slicing
)]

use super::*;
use slates_mem::arena::ChunkArena;
use slates_mem::region::Region;
use slates_svn::dump::{Admission, Uuid};
use slates_svn::migration::{Importer, Importing, Room, WithTexts};
use slates_vfs::NameEquivalence;
use slates_vfs::clock::StepClock;
use slates_vfs::quota::Quota;
use slates_vfs::volume::{StoreConfig, VolumeConfig};

/// The golden repository: `svnadmin dump -q` of a repository with copies, deletes, properties and text changes.
const FULL: &[u8] = include_bytes!("../../../../svn/fixtures/dump/full.dump");

/// Shape: a room no test history comes near.
const ROOMY: Room = Room {
  entries: 1 << 20,
  bytes: 1 << 30,
};

/// Shape: an admission past every line and window of the dump.
const ADMISSION: Admission = Admission {
  buffer_bytes: 1 << 20,
};

/// Shape: the bytes a read asks for at once: prime, so reads start and end inside pieces.
const READ_BYTES: usize = 1021;

/// The records `dump` makes.
fn imported(dump: &[u8]) -> SvnRecords {
  let uuid = Uuid::parse(b"00000000-0000-4000-8000-000000000738").unwrap();
  let mut records = SvnRecords::new(uuid, b"2026-11-20T00:00:00.000000Z", ROOMY).unwrap();
  let mut importer = Importer::new(ADMISSION);
  let mut rest = dump;
  loop {
    match importer.import(&mut records, rest, true).unwrap() {
      Importing::Done => return records,
      Importing::NeedMore { .. } => panic!("the whole dump was given"),
      Importing::Commit { consumed, record } => {
        rest = &rest[consumed..];
        records.commit_record(*record).unwrap();
      }
    }
  }
}

/// A vfs store and a volume for the view.
fn volume() -> (Store, Volume) {
  let page = 4096;
  let mut arena = ChunkArena::new(page);
  arena
    .add_region(Region::map(page * 4096, page, false).unwrap())
    .unwrap();
  let mut store = Store::new(
    &StoreConfig {
      page,
      cache_line: 64,
      max_dirs: 1 << 12,
      max_inodes: 1 << 12,
      max_chunks: 1 << 12,
      max_dir_blocks: 1 << 12,
      dir_cutover: 4,
    },
    arena,
    0,
  );
  let volume = Volume::create(
    &mut store,
    VolumeConfig {
      prefix: 7,
      names: NameEquivalence::Exact,
      quota: Quota::Bounded { limit: 1 << 28 },
      journal_bytes: 1 << 20,
      clock: Box::new(StepClock::new(0, 1_000)),
    },
  )
  .unwrap();
  (store, volume)
}

/// The file at `path` beneath `root`, read whole through `texts`.
fn read_path(
  volume: &Volume,
  store: &Store,
  root: InodeNo,
  path: &str,
  texts: &ViewTexts<'_>,
) -> Vec<u8> {
  let mut at = root;
  for name in path.split('/') {
    at = volume.lookup_no(store, at, name).unwrap().inode;
  }
  let mut out = Vec::new();
  let mut buf = vec![0u8; READ_BYTES];
  loop {
    let offset = u64::try_from(out.len()).unwrap();
    let read = volume
      .read_referenced(store, at, offset, &mut buf, texts)
      .unwrap();
    if read == 0 {
      return out;
    }
    out.extend_from_slice(&buf[..read]);
  }
}

/// Revision `revision`'s file as the view makes it, read whole from `source`.
fn expected_revision(view: &View, source: &SvnView<'_>, revision: Revision) -> Vec<u8> {
  let file = view
    .revision_file(source.records.index(), source, revision)
    .unwrap();
  let mut out = Vec::new();
  loop {
    let read = file
      .read(
        source,
        u64::try_from(out.len()).unwrap(),
        READ_BYTES,
        &mut out,
      )
      .unwrap();
    if read == 0 {
      return out;
    }
  }
}

/// A-645 P3. Do: present the golden repository's view into a volume a node-revision a call (the smallest budget);
/// then read every file through the view's text seam. Expect: every revision file, every revision properties file and
/// every fixed file the bytes `slates_svn::fsfs` makes for them; `db/current` naming the youngest; each revision file
/// a referenced file the volume refuses to change (`EROFS`); and the reads' counts covering every byte read.
#[test]
fn the_presented_volume_holds_the_fsfs_view_byte_for_byte() {
  let records = imported(FULL);
  let head = HeadTexts {
    green: None,
    store: None,
    records: &records,
  };
  let source = WithTexts {
    records: &records,
    texts: &head,
  };
  let (mut store, mut volume) = volume();
  let root = volume.root_inode(&store).unwrap();
  let mut presented = Presented::new();
  let mut holds = ViewHolds {
    store: None,
    records: &records,
  };
  let mut calls = 0;
  while presented
    .advance((&mut volume, &mut store), root, &source, &mut holds, 1)
    .unwrap()
    == Advance::More
  {
    calls += 1;
  }
  let youngest = records.youngest();
  assert!(calls > 0, "a budget of one took more than one call");
  assert_eq!(presented.written(), youngest + 1);
  let mut view = View::new();
  while view.advance(records.index(), &source, usize::MAX).unwrap() == Advance::More {}
  let texts = ViewTexts(&head);
  let mut read = 0u64;
  for revision in 0..=youngest {
    let path = fsfs::revision_file_path(revision);
    let bytes = read_path(&volume, &store, root, &path, &texts);
    assert_eq!(bytes, expected_revision(&view, &source, revision), "{path}");
    read += u64::try_from(bytes.len()).unwrap();
    let mut at = root;
    for name in path.split('/') {
      at = volume.lookup_no(&store, at, name).unwrap().inode;
    }
    assert_eq!(
      volume.write(&mut store, at, 0, b"x"),
      Err(VfsError::ReadOnly)
    );
    let path = fsfs::revision_properties_path(revision);
    assert_eq!(
      read_path(&volume, &store, root, &path, &texts),
      fsfs::revision_properties_file(&source, revision).unwrap(),
      "{path}"
    );
  }
  for (path, bytes) in fsfs::fixed_files(records.uuid().as_bytes(), youngest) {
    assert_eq!(
      read_path(&volume, &store, root, path, &texts),
      bytes,
      "{path}"
    );
  }
  assert_eq!(
    read_path(&volume, &store, root, "db/current", &texts),
    format!("{youngest}\n").into_bytes()
  );
  assert!(volume.referenced_counts().bytes_read >= read);
  assert_eq!(
    (store.content.chunks(), store.content.allocated_bytes()),
    (0, 0),
    "the view keeps nothing in the shard's chunk store"
  );
}

/// A-645 (a), the revprop rows. Do: present the golden repository's view; change revision 1's `svn:log` in the records,
/// as a revision property change does; tell the presenter. Expect revision 1's properties file to hold the new value,
/// as `fsfs` writes it, and every other revision's unchanged.
#[test]
fn a_revision_property_change_rewrites_that_revisions_properties_file() {
  let mut records = imported(FULL);
  let (mut store, mut volume) = volume();
  let root = volume.root_inode(&store).unwrap();
  let mut presented = Presented::new();
  {
    let head = HeadTexts {
      green: None,
      store: None,
      records: &records,
    };
    let source = WithTexts {
      records: &records,
      texts: &head,
    };
    let mut holds = ViewHolds {
      store: None,
      records: &records,
    };
    while presented
      .advance(
        (&mut volume, &mut store),
        root,
        &source,
        &mut holds,
        usize::MAX,
      )
      .unwrap()
      == Advance::More
    {}
  }
  let mut properties = records.revision_properties(1).unwrap().clone();
  properties.insert(b"svn:log".to_vec(), b"changed by a hook's leave".to_vec());
  records.set_revision_properties(1, properties).unwrap();
  let head = HeadTexts {
    green: None,
    store: None,
    records: &records,
  };
  let source = WithTexts {
    records: &records,
    texts: &head,
  };
  presented
    .revision_properties_changed((&mut volume, &mut store), root, &source, 1)
    .unwrap();
  let texts = ViewTexts(&head);
  for revision in 0..=records.youngest() {
    let path = fsfs::revision_properties_path(revision);
    let bytes = read_path(&volume, &store, root, &path, &texts);
    assert_eq!(
      bytes,
      fsfs::revision_properties_file(&source, revision).unwrap(),
      "{path}"
    );
    assert_eq!(
      revision == 1,
      bytes
        .windows(25)
        .any(|window| window == b"changed by a hook's leave"),
      "{path}"
    );
  }
}
