//! Protocol v2 `object-info` visibility immediately after a push before any fetch derives reach
//! sets, including when landed pack windows have been let go from RAM and must be brought back on
//! demand (preserving hidden-ref protection).

use core::convert::Infallible;
use std::io::Write as _;

use flate2::Compression;
use flate2::write::ZlibEncoder;
use slates_vcs::{ContentId, ContentStore};

use crate::Admission;
use crate::entry::{self, Header, Representation};
use crate::http::Version;
use crate::object_name::{Kind, ObjectFormat, ObjectId, name_of};
use crate::pack;
use crate::pktline::{self, Packet};
use crate::store::{GitNaming, Incoming, NamedRef, Push, Repository};
use crate::upload::{
  Asked, PackPlan, PackRefused, PackSource, Refusal, Served, Written, serve_in_turns,
};

/// Minimal `PackSource` for `object-info` requests (which never stream a pack).
struct NoPacks;

impl PackSource for NoPacks {
  type Pack = ();

  fn begin(
    &mut self,
    _served: &Served<'_>,
    _plan: &PackPlan<'_>,
    _admission: &mut Admission,
  ) -> Result<(), PackRefused> {
    panic!("object-info must not begin a pack")
  }

  fn write(
    &mut self,
    _served: &Served<'_>,
    _pack: &mut (),
    _admission: &mut Admission,
    _out: &mut Vec<u8>,
    _room: usize,
  ) -> Result<bool, PackRefused> {
    panic!("object-info must not write a pack")
  }
}

/// Shape: 4 KiB content-store window matching `tests/store/packed.rs`'s `landed_create`.
const STORE_WINDOW: usize = 1 << 12;

/// Format: 29-byte blob payload matching the live red-team repro (`object-info-visible-blob.py`).
const VISIBLE_BLOB: &[u8] = b"visible-blob-after-fresh-push";

/// Format: 31-byte blob reachable only from a hidden ref.
const HIDDEN_BLOB: &[u8] = b"hidden-only-blob-must-stay-dark";

/// Deflates `data` as one zlib stream.
fn zlib(data: &[u8]) -> Vec<u8> {
  let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
  encoder.write_all(data).unwrap();
  encoder.finish().unwrap()
}

/// Builds a pack holding `objects` whole, returning `(pack_bytes, offsets)` in order.
fn pack_with_offsets(objects: &[(Kind, &[u8])]) -> (Vec<u8>, Vec<u64>) {
  let mut bytes = pack::SIGNATURE.to_vec();
  bytes.extend_from_slice(&2u32.to_be_bytes());
  bytes.extend_from_slice(&u32::try_from(objects.len()).unwrap().to_be_bytes());
  let mut offsets = Vec::with_capacity(objects.len());
  for (kind, content) in objects {
    let offset = u64::try_from(bytes.len()).unwrap();
    offsets.push(offset);
    let header = Header {
      representation: Representation::Whole(*kind),
      size: u64::try_from(content.len()).unwrap(),
    };
    entry::write(header, offset, &mut bytes).unwrap();
    bytes.extend_from_slice(&zlib(content));
  }
  let checksum = pack::checksum_of(ObjectFormat::Sha1, &bytes).unwrap();
  bytes.extend_from_slice(checksum.as_bytes());
  (bytes, offsets)
}

/// Encodes one tree entry (`mode name\0<raw-id>`).
fn tree_entry(mode: &str, name: &str, id: ObjectId) -> Vec<u8> {
  [format!("{mode} {name}\0").as_bytes(), id.as_bytes()].concat()
}

/// Encodes one root commit pointing to `tree`.
fn commit_of(tree: ObjectId, message: &str) -> Vec<u8> {
  format!(
    "tree {tree}\nauthor A <a@example.com> 1 +0000\ncommitter A <a@example.com> 1 +0000\n\n{message}\n"
  )
  .into_bytes()
}

/// Encodes a protocol v2 `object-info` command request asking for `size` and each oid in `oids`.
fn object_info_request(oids: &[ObjectId]) -> Vec<u8> {
  let mut out = Vec::new();
  pktline::write(Packet::Data(b"command=object-info\n"), &mut out).unwrap();
  pktline::write(Packet::Data(b"agent=git/2.56.1\n"), &mut out).unwrap();
  pktline::write(Packet::Data(b"object-format=sha1\n"), &mut out).unwrap();
  pktline::write(Packet::Delim, &mut out).unwrap();
  pktline::write(Packet::Data(b"size\n"), &mut out).unwrap();
  for id in oids {
    let line = format!("oid {id}\n");
    pktline::write(Packet::Data(line.as_bytes()), &mut out).unwrap();
  }
  pktline::write(Packet::Flush, &mut out).unwrap();
  out
}

/// Commits a fresh push holding a visible branch (green 1 -> `VISIBLE_BLOB`) and a hidden branch
/// (green 2 -> `HIDDEN_BLOB`) WITHOUT deriving reach sets, returning `(repo, store, pack, vis_id, hid_id)`.
fn fresh_pushed_repo() -> (Repository, ContentStore, Vec<u8>, ObjectId, ObjectId) {
  assert_eq!(VISIBLE_BLOB.len(), 29);
  let vis_blob_id = name_of(ObjectFormat::Sha1, Kind::Blob, VISIBLE_BLOB).unwrap();
  let vis_tree = tree_entry("100644", "visible.txt", vis_blob_id);
  let vis_tree_id = name_of(ObjectFormat::Sha1, Kind::Tree, &vis_tree).unwrap();
  let vis_commit = commit_of(vis_tree_id, "visible");
  let vis_commit_id = name_of(ObjectFormat::Sha1, Kind::Commit, &vis_commit).unwrap();

  let hid_blob_id = name_of(ObjectFormat::Sha1, Kind::Blob, HIDDEN_BLOB).unwrap();
  let hid_tree = tree_entry("100644", "secret.txt", hid_blob_id);
  let hid_tree_id = name_of(ObjectFormat::Sha1, Kind::Tree, &hid_tree).unwrap();
  let hid_commit = commit_of(hid_tree_id, "hidden");
  let hid_commit_id = name_of(ObjectFormat::Sha1, Kind::Commit, &hid_commit).unwrap();

  let objects: [(Kind, &[u8]); 6] = [
    (Kind::Blob, VISIBLE_BLOB),
    (Kind::Tree, &vis_tree),
    (Kind::Commit, &vis_commit),
    (Kind::Blob, HIDDEN_BLOB),
    (Kind::Tree, &hid_tree),
    (Kind::Commit, &hid_commit),
  ];
  let (pack, offsets) = pack_with_offsets(&objects);
  let mut store = ContentStore::new(1 << 24, STORE_WINDOW).unwrap();
  let mut repository = Repository::new(ObjectFormat::Sha1);
  let naming = GitNaming {
    format: ObjectFormat::Sha1,
  };
  let mut push = Push::new(ObjectFormat::Sha1);
  push.extend_pack(&pack);
  for ((kind, content), offset) in objects.iter().zip(offsets) {
    let incoming = Incoming {
      kind: *kind,
      content,
      claimed: None,
      depth: 0,
    };
    push
      .admit_placed::<_, Infallible>(&store, &naming, (incoming, offset))
      .unwrap();
  }
  repository.seal(&store, &mut push).unwrap();
  repository.stage(&mut store, &mut push).unwrap();
  let vis_move = repository
    .admit(&store, &push, 1, Some(vis_commit_id))
    .unwrap();
  let hid_move = repository
    .admit(&store, &push, 2, Some(hid_commit_id))
    .unwrap();
  repository
    .commit(&mut store, &mut push, &[vis_move, hid_move])
    .unwrap();
  push.abandon(&mut store).unwrap();
  // Deliberately DO NOT derive reach sets: both greens stay deferred right after push.
  (repository, store, pack, vis_blob_id, hid_blob_id)
}

/// Drains `answer` whole into bytes.
fn drain(
  served: &Served<'_>,
  answer: &mut crate::upload::Answer<()>,
  packs: &mut NoPacks,
) -> Vec<u8> {
  let mut out = Vec::new();
  let mut admission = Admission::new(u64::MAX);
  while answer.write(served, packs, &mut admission, &mut out, 65_536) == Written::More {}
  out
}

/// §4.17.2 "A fetch" & "Access" (protocol v2 `object-info` immediately after push, before any
/// fetch warms derived reach sets, including after landed pack windows are let go from RAM):
/// do issue `object-info` for a visible 29-byte blob and a hidden-only 31-byte blob right after
/// push, once with pack windows resident and once after letting every pack window go (driving the
/// exact `made.refusal().and(served.repository.waiting())` bring-back retry loop that HTTP and SSH
/// edges run). Expect: when pack windows are let go, the first call refuses `Refusal::Store(_)`
/// with `repository.waiting().is_some()` (never silently reporting the visible blob unknown!), and
/// after bringing back the waited window the retry reports `<visible> 29` while keeping `<hidden> `
/// unknown.
#[test]
fn object_info_reports_freshly_pushed_visible_blob_even_when_reach_sets_deferred_and_pack_let_go() {
  let (repository, mut store, pack, vis_blob_id, hid_blob_id) = fresh_pushed_repo();
  let visible_refs = [NamedRef {
    name: b"refs/heads/main",
    green: 1,
  }];
  let request = object_info_request(&[vis_blob_id, hid_blob_id]);

  // 1. Resident windows, deferred reach sets: succeeds immediately on first call.
  let served = Served {
    repository: &repository,
    store: &store,
    refs: &visible_refs,
    head: b"refs/heads/main",
  };
  let mut packs = NoPacks;
  let mut admission = Admission::new(u64::MAX);
  let mut answer = serve_in_turns(
    &served,
    Version::V2,
    Asked::Request(&request),
    &mut packs,
    &mut admission,
  );
  assert_eq!(answer.refusal(), None);
  let resident_bytes = drain(&served, &mut answer, &mut packs);
  let resident_text = String::from_utf8_lossy(&resident_bytes);
  assert!(
    resident_text.contains(&format!("{vis_blob_id} 29")),
    "visible blob should report 29 bytes: {resident_text}"
  );
  assert!(
    resident_text.contains(&format!("002d{hid_blob_id} ")),
    "hidden-only blob must stay unknown: {resident_text}"
  );

  // 2. Evict all pack windows + recent cache (matching `let_go_owed_as_push_ends` after a push).
  for window in pack.chunks(STORE_WINDOW).map(ContentId::of) {
    assert!(store.mark_landed(&window));
    store.let_go_landed(&window, None).unwrap();
  }
  repository.let_go_recent(&mut store);

  // Drive the exact HTTP/SSH bring-back retry loop (`made.refusal().and(repository.waiting())`).
  let mut brought = 0usize;
  let mut last = None;
  let bytes = loop {
    let served = Served {
      repository: &repository,
      store: &store,
      refs: &visible_refs,
      head: b"refs/heads/main",
    };
    let mut packs = NoPacks;
    let mut admission = Admission::new(u64::MAX);
    let mut answer = serve_in_turns(
      &served,
      Version::V2,
      Asked::Request(&request),
      &mut packs,
      &mut admission,
    )
    .holding();
    let away = answer.refusal().and(served.repository.waiting());
    if let Some(waited) = away {
      assert!(
        matches!(answer.refusal(), Some(Refusal::Store(_))),
        "expected Refusal::Store when waiting on let-go pack window, got {:?}",
        answer.refusal()
      );
      assert_ne!(last, Some(waited), "bring-back must make progress");
      last = Some(waited);
      let (window, start, len) = store
        .away_window(&waited.pack, waited.len, waited.offset)
        .expect("waited window is let go");
      let from = usize::try_from(start).unwrap();
      let to = from + usize::try_from(len).unwrap();
      store.bring_back(&window, &pack[from..to]).unwrap();
      brought += 1;
      continue;
    }
    assert_eq!(
      answer.refusal(),
      None,
      "unexpected refusal: {:?}",
      answer.refusal()
    );
    break drain(&served, &mut answer, &mut packs);
  };

  assert!(
    brought > 0,
    "non-vacuity: let-go pack window must trigger Refusal::Store + waiting() bring-back"
  );
  let text = String::from_utf8_lossy(&bytes);
  assert!(
    text.contains(&format!("{vis_blob_id} 29")),
    "visible blob must report 29 bytes after bring-back: {text}"
  );
  assert!(
    text.contains(&format!("002d{hid_blob_id} ")),
    "hidden-only blob must stay unknown after bring-back: {text}"
  );
}

/// §4.17.2 "A fetch" & "Access" (protocol v2 `object-info` over annotated tag -> commit -> parent
/// commit -> tree -> blob with deferred reach sets and let-go pack windows, plus refusal on a
/// genuine store fault):
/// do push a two-commit history tagged by an annotated tag on visible `refs/tags/v1` and a hidden
/// branch `refs/heads/secret`, evict every pack window, and issue `object-info` for the ancestor
/// blob, ancestor tree, ancestor commit, annotated tag, and hidden blob.
/// Expect: without bring-back, `serve_in_turns` refuses with `Refusal::Store(_)` instead of
/// silently returning unknown; after bring-back, every visible object reports its exact byte size
/// while the hidden-only blob remains unknown.
#[test]
fn object_info_covers_tree_tag_ancestor_and_refuses_unreadable_store_fault() {
  let base_blob = b"ancestor-blob-bytes";
  let base_blob_id = name_of(ObjectFormat::Sha1, Kind::Blob, base_blob).unwrap();
  let base_tree = tree_entry("100644", "base.txt", base_blob_id);
  let base_tree_id = name_of(ObjectFormat::Sha1, Kind::Tree, &base_tree).unwrap();
  let base_commit = commit_of(base_tree_id, "base");
  let base_commit_id = name_of(ObjectFormat::Sha1, Kind::Commit, &base_commit).unwrap();

  let tip_blob = b"tip-blob-bytes!";
  let tip_blob_id = name_of(ObjectFormat::Sha1, Kind::Blob, tip_blob).unwrap();
  let tip_tree = tree_entry("100644", "tip.txt", tip_blob_id);
  let tip_tree_id = name_of(ObjectFormat::Sha1, Kind::Tree, &tip_tree).unwrap();
  let tip_commit = format!(
    "tree {tip_tree_id}\nparent {base_commit_id}\nauthor A <a@example.com> 2 +0000\ncommitter A <a@example.com> 2 +0000\n\ntip\n"
  )
  .into_bytes();
  let tip_commit_id = name_of(ObjectFormat::Sha1, Kind::Commit, &tip_commit).unwrap();
  let tag_bytes = format!(
    "object {tip_commit_id}\ntype commit\ntag v1\ntagger A <a@example.com> 3 +0000\n\nrelease v1\n"
  )
  .into_bytes();
  let tag_id = name_of(ObjectFormat::Sha1, Kind::Tag, &tag_bytes).unwrap();

  let hid_blob_id = name_of(ObjectFormat::Sha1, Kind::Blob, HIDDEN_BLOB).unwrap();
  let hid_tree = tree_entry("100644", "secret.txt", hid_blob_id);
  let hid_tree_id = name_of(ObjectFormat::Sha1, Kind::Tree, &hid_tree).unwrap();
  let hid_commit = commit_of(hid_tree_id, "hidden");
  let hid_commit_id = name_of(ObjectFormat::Sha1, Kind::Commit, &hid_commit).unwrap();

  let objects: [(Kind, &[u8]); 10] = [
    (Kind::Blob, base_blob),
    (Kind::Tree, &base_tree),
    (Kind::Commit, &base_commit),
    (Kind::Blob, tip_blob),
    (Kind::Tree, &tip_tree),
    (Kind::Commit, &tip_commit),
    (Kind::Tag, &tag_bytes),
    (Kind::Blob, HIDDEN_BLOB),
    (Kind::Tree, &hid_tree),
    (Kind::Commit, &hid_commit),
  ];
  let (pack, offsets) = pack_with_offsets(&objects);
  let mut store = ContentStore::new(1 << 24, STORE_WINDOW).unwrap();
  let mut repository = Repository::new(ObjectFormat::Sha1);
  let naming = GitNaming {
    format: ObjectFormat::Sha1,
  };
  let mut push = Push::new(ObjectFormat::Sha1);
  push.extend_pack(&pack);
  for ((kind, content), offset) in objects.iter().zip(offsets) {
    let incoming = Incoming {
      kind: *kind,
      content,
      claimed: None,
      depth: 0,
    };
    push
      .admit_placed::<_, Infallible>(&store, &naming, (incoming, offset))
      .unwrap();
  }
  repository.seal(&store, &mut push).unwrap();
  repository.stage(&mut store, &mut push).unwrap();
  let vis_move = repository.admit(&store, &push, 1, Some(tag_id)).unwrap();
  let hid_move = repository
    .admit(&store, &push, 2, Some(hid_commit_id))
    .unwrap();
  repository
    .commit(&mut store, &mut push, &[vis_move, hid_move])
    .unwrap();
  push.abandon(&mut store).unwrap();

  for window in pack.chunks(STORE_WINDOW).map(ContentId::of) {
    assert!(store.mark_landed(&window));
    store.let_go_landed(&window, None).unwrap();
  }
  repository.let_go_recent(&mut store);

  let visible_refs = [NamedRef {
    name: b"refs/tags/v1",
    green: 1,
  }];
  let request = object_info_request(&[
    base_blob_id,
    base_tree_id,
    base_commit_id,
    tag_id,
    hid_blob_id,
  ]);

  // First call without bring-back MUST fail with Refusal::Store (never silently return unknown).
  {
    let served = Served {
      repository: &repository,
      store: &store,
      refs: &visible_refs,
      head: b"refs/tags/v1",
    };
    let mut packs = NoPacks;
    let mut admission = Admission::new(u64::MAX);
    let answer = serve_in_turns(
      &served,
      Version::V2,
      Asked::Request(&request),
      &mut packs,
      &mut admission,
    )
    .holding();
    assert!(
      matches!(answer.refusal(), Some(Refusal::Store(_))),
      "unreadable let-go window must refuse with Refusal::Store, got {:?}",
      answer.refusal()
    );
  }

  // Now drive the bring-back retry loop to completion.
  let mut brought = 0usize;
  let bytes = loop {
    let served = Served {
      repository: &repository,
      store: &store,
      refs: &visible_refs,
      head: b"refs/tags/v1",
    };
    let mut packs = NoPacks;
    let mut admission = Admission::new(u64::MAX);
    let mut answer = serve_in_turns(
      &served,
      Version::V2,
      Asked::Request(&request),
      &mut packs,
      &mut admission,
    )
    .holding();
    if let Some(waited) = answer.refusal().and(served.repository.waiting()) {
      let (window, start, len) = store
        .away_window(&waited.pack, waited.len, waited.offset)
        .expect("waited window is let go");
      let from = usize::try_from(start).unwrap();
      let to = from + usize::try_from(len).unwrap();
      store.bring_back(&window, &pack[from..to]).unwrap();
      brought += 1;
      continue;
    }
    assert_eq!(answer.refusal(), None);
    break drain(&served, &mut answer, &mut packs);
  };
  assert!(brought > 0);
  let text = String::from_utf8_lossy(&bytes);
  assert!(text.contains(&format!("{base_blob_id} {}", base_blob.len())));
  assert!(text.contains(&format!("{base_tree_id} {}", base_tree.len())));
  assert!(text.contains(&format!("{base_commit_id} {}", base_commit.len())));
  assert!(text.contains(&format!("{tag_id} {}", tag_bytes.len())));
  assert!(text.contains(&format!("002d{hid_blob_id} ")));
}
