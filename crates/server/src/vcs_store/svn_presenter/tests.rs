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
use slates_svn::migration::Repository as SvnRecords;
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

/// Shape: the owner of the view's root, a user other than root, whom every entry must be owned by.
const OWNER: (u32, u32) = (501, 20);

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
      Importing::Base { text, .. } => {
        panic!("the records hold every text, yet the import asked for {text:?}")
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
  let mut volume = Volume::create(
    &mut store,
    VolumeConfig {
      prefix: 7,
      names: NameEquivalence::Exact,
      quota: Quota::Bounded { limit: 0 },
      journal_bytes: 1 << 20,
      clock: Box::new(StepClock::new(0, 1_000)),
    },
  )
  .unwrap();
  // As the daemon makes a view: a ceiling of nothing, its allowances only what the empty volume holds; the presenter
  // sizes it to the view's content as it writes (w-interop's matrix met `Volume(NoSpace)` before it did).
  let (inodes, _) = volume.inode_usage();
  volume.set_inode_allowance(inodes).unwrap();
  let (entries, _) = volume.entry_usage();
  volume.set_entry_allowance(entries.max(1)).unwrap();
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

/// Every fixed file of `records`' FSFS view read back through the view as `fsfs` makes it.
fn fixed_files_read_back(
  (volume, store, root): (&Volume, &Store, InodeNo),
  records: &SvnRecords,
  texts: &ViewTexts<'_>,
) {
  for (path, bytes) in fsfs::fixed_files(records.uuid().as_bytes(), records.youngest()) {
    assert_eq!(read_path(volume, store, root, path, texts), bytes, "{path}");
  }
}

/// Revision `revision`'s files read back through the view: its revision file the bytes `fsfs` makes for it, refused a
/// write (`EROFS`) and owned by the view root's owner, readable by all (`0444`), its properties file `fsfs`'s. The revision file's bytes read.
fn revision_reads_back(
  (volume, store, root): (&mut Volume, &mut Store, InodeNo),
  (view, source, texts): (&View, &SvnView<'_>, &ViewTexts<'_>),
  revision: Revision,
) -> u64 {
  let path = fsfs::revision_file_path(revision);
  let bytes = read_path(volume, store, root, &path, texts);
  assert_eq!(bytes, expected_revision(view, source, revision), "{path}");
  let mut at = root;
  for name in path.split('/') {
    at = volume.lookup_no(store, at, name).unwrap().inode;
  }
  assert_eq!(volume.write(store, at, 0, b"x"), Err(VfsError::ReadOnly));
  let attrs = volume.stat(store, at).unwrap();
  assert_eq!(
    (attrs.uid, attrs.gid, attrs.mode & 0o777),
    (OWNER.0, OWNER.1, 0o444),
    "{path}"
  );
  let path = fsfs::revision_properties_path(revision);
  assert_eq!(
    read_path(volume, store, root, &path, texts),
    fsfs::revision_properties_file(source, revision).unwrap(),
    "{path}"
  );
  u64::try_from(bytes.len()).unwrap()
}

/// A-645 P3. Do: present the golden repository's view into a volume a node-revision a call (the smallest budget);
/// then read every file through the view's text seam. Expect: every revision file, every revision properties file and
/// every fixed file the bytes `slates_svn::fsfs` makes for them; `db/current` naming the youngest; each revision file
/// a referenced file the volume refuses to change (`EROFS`), owned by the root's owner and readable by all (`0444`);
/// and the reads' counts covering every byte read.
#[test]
fn the_presented_volume_holds_the_fsfs_view_byte_for_byte() {
  let records = imported(FULL);
  let head = HeadTexts {
    green: None,
    store: None,
    records: &records,
    pieces: None,
  };
  let source = WithTexts {
    records: &records,
    texts: &head,
  };
  let (mut store, mut volume) = volume();
  let root = volume.root_inode(&store).unwrap();
  volume.chown(&mut store, root, OWNER.0, OWNER.1).unwrap();
  let mut presented = Presented::new();
  let mut holds = ViewHolds {
    store: None,
    stored: &BTreeSet::new(),
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
    read += revision_reads_back(
      (&mut volume, &mut store, root),
      (&view, &source, &texts),
      revision,
    );
  }
  fixed_files_read_back((&volume, &store, root), &records, &texts);
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
      pieces: None,
    };
    let source = WithTexts {
      records: &records,
      texts: &head,
    };
    let mut holds = ViewHolds {
      store: None,
      stored: &BTreeSet::new(),
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
    pieces: None,
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

/// C2 stage B part 2 (b), P2 (a hook's view brings back a stored text the content store let go before it holds it). Do:
/// on a daemon whose revision 1 adds `/trunk/a.bin`, every hold RAM has on its text let go
/// (`crate::test_store::a_text_only_the_store_holds`), show the repository's view as a hook's `REPOS-PATH` is shown
/// ([`show`]), then read revision 1's file through the view's text seam. Expect the view shown, the file carrying the
/// text whole (`PLAIN`), the text read through the store (`vcs_store.svn_texts.piece_fetched` at least once), and the
/// view holding nothing it brought beyond its file's own hold. Before, the layout's hold on the text was refused and the
/// view was never shown.
#[test]
fn a_hook_view_brings_back_a_stored_text_the_content_store_let_go() {
  let bytes = crate::test_store::noise(200 << 10);
  let (_store, daemon, id) = crate::test_store::a_text_only_the_store_holds(
    &crate::repository::svn_commit::tests::owner(),
    "viewed",
    &bytes,
  );
  let shown = crate::test_store::run_on_first(
    &daemon,
    move || async move {
      let shard = crate::state::with_state(|state| state.shard).unwrap();
      show(shard, shard, id).await
    },
    std::time::Duration::from_secs(60),
  );
  assert_eq!(shown, Ok(()));
  let (file, brought, fetched) = crate::daemon::observe_first(&daemon, move |state| {
    let fetched = state
      .refusals
      .get(crate::vcs_store::svn_texts::PIECE_FETCHED)
      .copied()
      .unwrap_or(0);
    let read = crate::vcs_store::lent_state(state, |state| {
      let slot = state.svn_views.get(&id)?;
      let handle = *state.by_id.get(&slot.volume)?;
      let held = state.volumes.get(handle).ok()?;
      let records = state.vcs_stores.subversion_records(id)?;
      let texts = HeadTexts {
        green: None,
        store: state.vcs_stores.content(),
        records,
        pieces: None,
      };
      let root = held.volume.root_inode(&state.store).ok()?;
      let path = fsfs::revision_file_path(1);
      let file = read_path(&held.volume, &state.store, root, &path, &ViewTexts(&texts));
      Some((file, slot.brought.len()))
    });
    let (file, brought) = read.unzip();
    (file, brought, fetched)
  });
  daemon.stop();
  let carried = file
    .as_ref()
    .is_some_and(|file| file.windows(bytes.len()).any(|window| window == bytes));
  assert!(
    carried,
    "revision 1's file does not carry the text: {:?} bytes",
    file.map(|file| file.len())
  );
  assert!(fetched >= 1, "no piece read through the store");
  assert_eq!(brought, Some(0), "the view kept what it brought");
}
