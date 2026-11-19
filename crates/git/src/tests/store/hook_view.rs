//! A-665: the read-only bare repository a git hook runs in, held to what stock git 2.50.1 makes
//! (`fixtures/view`, `make-view-fixtures.sh`: `git init --bare`, `index-pack`, `pack-refs`, and
//! what receive-pack hands its hooks) and to the store it shows: every object of the store and of
//! the push read back through the view's packs, each pack checked as `git verify-pack` checks one,
//! counted; git's own packs laid out and indexed byte for byte as `index-pack` indexes them; any
//! window of a file the same bytes; and every refusal by kind, with nothing left held.

use std::collections::{BTreeMap, BTreeSet};

use slates_vcs::ContentStore;

use crate::Admission;
use crate::entry::PACK_HEADER_LEN;
use crate::object_name::{Kind, ObjectFormat, ObjectId};
use crate::pack::{self, idx::Idx};
use crate::served::{self, MAIN, SIDE, V1, served};
use crate::store::hook_view::seal::{PartEntries, Sealing};
use crate::store::hook_view::units::{Part, Source, content_range, lay_out};
use crate::store::hook_view::{
  HookView, Listed, Node, PackKind, Pushed, Quantum, ReadRefused, Seal, SealRefused, ShownRef,
  Snapshot, ViewCounters, ViewRefused,
};
use crate::store::repository::Kept;
use crate::store::{Placed, Positions, Push, Repository, Stored};
use crate::unpack::{NoIndex, check_index, read_whole};

use super::fragments::{commit_over, text};
use super::{EXCHANGE, Made, Ticks, blob, made, push_and_move, sealed, store};

/// What `git init --bare -b main` makes (`make-view-fixtures.sh` 1): `HEAD`.
const INIT_HEAD: &[u8] = include_bytes!("../../../fixtures/view/init.HEAD");
/// Its `config`, SHA-1.
const INIT_CONFIG: &[u8] = include_bytes!("../../../fixtures/view/init.config");
/// Its `config`, SHA-256.
const INIT_SHA256_CONFIG: &[u8] = include_bytes!("../../../fixtures/view/init-sha256.config");
/// A-582's repository made bare by `init`, `index-pack` and `pack-refs` (2): every path.
const BARE_LISTING: &str = include_str!("../../../fixtures/view/bare.listing.txt");
/// Its `packed-refs`.
const BARE_PACKED_REFS: &[u8] = include_bytes!("../../../fixtures/view/bare.packed-refs");
/// The pushing process's own `GIT_*` variables (3), which every hook inherits.
const PUSHER_ENV: &str = include_str!("../../../fixtures/view/pusher.env.txt");
/// `pre-receive` of a push unpacked loose into the quarantine.
const LOOSE_PRE_RECEIVE: &str = include_str!("../../../fixtures/view/hook.loose.pre-receive.txt");
/// `pre-receive` of a push indexed into the quarantine as a pack.
const PACK_PRE_RECEIVE: &str = include_str!("../../../fixtures/view/hook.pack.pre-receive.txt");
/// `update`, after receive-pack moved the objects.
const LOOSE_UPDATE: &str = include_str!("../../../fixtures/view/hook.loose.update.txt");
/// `post-receive`.
const LOOSE_POST_RECEIVE: &str = include_str!("../../../fixtures/view/hook.loose.post-receive.txt");
/// A-582's pack with offset deltas, and its index, which `index-pack` makes again byte for byte.
const OFS_PACK: &[u8] = include_bytes!("../../../fixtures/ofs.pack");
/// Its index.
const OFS_IDX: &[u8] = include_bytes!("../../../fixtures/ofs.idx");
/// A-591's SHA-256 pack.
const SHA256_PACK: &[u8] = include_bytes!("../../../fixtures/sha256.pack");
/// Its index.
const SHA256_IDX: &[u8] = include_bytes!("../../../fixtures/sha256.idx");

/// Shape: the room the tests' views and seals may use, room to spare.
const ROOM: u64 = 1 << 30;

/// Shape: the turns a seal may take before a test calls it stuck.
const TURNS: u64 = 1 << 24;

/// The root the fixtures write a repository's path as.
const ROOT: &[u8] = b"$ROOT";

/// What the fixtures write in place of `mkdtemp`'s six letters, and what comes before them.
const QUARANTINE: (&[u8], &[u8]) = (b"tmp_objdir-incoming-", b"XXXXXX");

/// `HEAD`'s target in every test.
const DEFAULT_BRANCH: &[u8] = b"refs/heads/main";

/// The served repository's refs at their newest versions.
const SERVED_REFS: [ShownRef<'static>; 3] = [
  ShownRef {
    name: b"refs/heads/main",
    green: MAIN,
    version: 2,
  },
  ShownRef {
    name: b"refs/heads/side",
    green: SIDE,
    version: 0,
  },
  ShownRef {
    name: b"refs/tags/v1",
    green: V1,
    version: 0,
  },
];

/// `value` as a `u64`.
fn to_u64(value: usize) -> u64 {
  u64::try_from(value).unwrap()
}

/// A line of a listing's path: what follows its `d` or `f`.
fn path_of(line: &str) -> &str {
  line.split_once(' ').map_or(line, |(_, path)| path)
}

/// A turn of one unit of work: no quantum on the tests' clock.
fn one_unit(clock: &mut Ticks) -> Quantum<'_> {
  Quantum { clock, ns: 0 }
}

/// The seal of `view`'s pack `pack`, a unit of work a turn.
fn seal_one(
  view: &HookView,
  repository: &Repository,
  store: &mut ContentStore,
  pack: &ObjectId,
) -> Result<Seal, SealRefused> {
  let mut sealing = view.sealing(pack).ok_or(SealRefused::NoPack)?;
  let mut clock = Ticks(0);
  for _ in 0..TURNS {
    if view.seal_step(&mut sealing, store, one_unit(&mut clock))? {
      return view.seal(sealing, repository, store, &mut Admission::new(ROOM));
    }
  }
  Err(SealRefused::Unfinished)
}

/// Every pack of `view` sealed.
fn seal_all(
  view: &HookView,
  repository: &Repository,
  store: &mut ContentStore,
) -> BTreeMap<ObjectId, Seal> {
  let packs: Vec<ObjectId> = view.packs().map(|(name, _)| name).collect();
  packs
    .into_iter()
    .map(|pack| (pack, seal_one(view, repository, store, &pack).unwrap()))
    .collect()
}

/// Gives back the seals and the view's holds.
fn close(view: HookView, seals: BTreeMap<ObjectId, Seal>, store: &mut ContentStore) {
  for seal in seals.into_values() {
    seal.release(store).unwrap();
  }
  view.close(store).unwrap();
}

/// The file `path` read whole, `window` bytes a read.
fn read_file(
  view: &HookView,
  store: &ContentStore,
  seals: &BTreeMap<ObjectId, Seal>,
  path: &[u8],
  window: u64,
) -> Vec<u8> {
  let Some(Node::File { len }) = view.node(path) else {
    panic!("{} is no file", String::from_utf8_lossy(path));
  };
  let mut bytes = Vec::new();
  let mut at = 0;
  while at < len {
    view.read(store, seals, path, (at, window), &mut bytes).unwrap();
    at += window;
  }
  assert_eq!(to_u64(bytes.len()), len);
  bytes
}

/// Every path of `view` as `make-view-fixtures.sh` lists one: `d` or `f`, a space, the path,
/// sorted by path.
fn walk(view: &HookView) -> Vec<String> {
  let mut lines = Vec::new();
  let mut pending: Vec<Vec<u8>> = vec![Vec::new()];
  while let Some(directory) = pending.pop() {
    for Listed { name, node } in view.list(&directory).unwrap() {
      let path = if directory.is_empty() {
        name
      } else {
        [directory.as_slice(), b"/".as_slice(), name.as_slice()].concat()
      };
      let shown = String::from_utf8(path.clone()).unwrap();
      if node == Node::Directory {
        lines.push(format!("d {shown}"));
        pending.push(path);
      } else {
        lines.push(format!("f {shown}"));
      }
    }
  }
  lines.sort_by(|left, right| path_of(left).cmp(path_of(right)));
  lines
}

/// What git made that the view shows: every path but `description`, the hook samples,
/// `info/exclude` and the `.rev` (the view's module docs say why each is left out).
fn shown_by_the_view(listing: &str) -> Vec<String> {
  listing
    .lines()
    .filter(|line| {
      let path = path_of(line);
      let left_out = ["description", "hooks", "info"].contains(&path)
        || path.starts_with("hooks/")
        || path.starts_with("info/")
        || path.ends_with(".rev");
      !left_out
    })
    .map(str::to_owned)
    .collect()
}

/// A listing's lines set apart: the rest, and those naming a file in `objects/pack`.
fn split_packs(lines: &[String]) -> (Vec<String>, Vec<String>) {
  let (packs, rest): (Vec<String>, Vec<String>) = lines
    .iter()
    .cloned()
    .partition(|line| line.starts_with("f objects/pack/"));
  (rest, packs)
}

/// Whether `line` names a pack's file as `index-pack` names one: `pack-`, the format's digits in
/// lower case, `.pack` or `.idx`.
fn named_as_git_names(line: &str, format: ObjectFormat) -> bool {
  let name = line.trim_start_matches("f objects/pack/");
  let digits = name
    .strip_prefix("pack-")
    .and_then(|named| {
      named
        .strip_suffix(".pack")
        .or_else(|| named.strip_suffix(".idx"))
    })
    .unwrap_or_default();
  digits.len() == format.hex_len()
    && digits
      .bytes()
      .all(|digit| digit.is_ascii_digit() || (b'a'..=b'f').contains(&digit))
}

/// A config's bytes less the two lines macOS's probes of its own filesystem add.
fn without_probes(config: &[u8]) -> Vec<u8> {
  let text = String::from_utf8(config.to_vec()).unwrap();
  text
    .lines()
    .filter(|line| !line.contains("ignorecase") && !line.contains("precomposeunicode"))
    .flat_map(|line| format!("{line}\n").into_bytes())
    .collect()
}

/// A-582's repository (`served`), every version's fragments encoded and `main`'s base pack made.
fn served_encoded() -> (Repository, ContentStore) {
  let served::Served {
    mut repository,
    mut store,
    ..
  } = served();
  let mut clock = Ticks(0);
  for (green, versions) in [(MAIN, 3), (SIDE, 1), (V1, 1)] {
    for version in 0..versions {
      repository
        .encode_fragments(&mut store, (green, version), &mut clock, EXCHANGE)
        .unwrap();
    }
  }
  repository
    .rebuild_base(&mut store, MAIN, &mut clock, EXCHANGE)
    .unwrap();
  (repository, store)
}

/// The view of `repository` at `refs`, with `pushed`.
fn open(
  repository: &Repository,
  store: &mut ContentStore,
  refs: &[ShownRef<'_>],
  pushed: Option<Pushed<'_>>,
) -> HookView {
  let snapshot = Snapshot {
    refs,
    head: DEFAULT_BRANCH,
  };
  HookView::open(
    repository,
    store,
    &snapshot,
    pushed,
    &mut Admission::new(ROOM),
  )
  .unwrap()
}

/// A-665 (§4.17.2 piece 10; `make-view-fixtures.sh` 1 and 2). Do: open the view of the
/// repository stock git served A-582's vectors from, its fragments encoded and `main`'s base pack
/// made; walk its tree. Expect: every path `git init --bare`, `index-pack` and `pack-refs` made
/// that git reads, by git's names; each pack an index and a pack named as `index-pack` names
/// them; `HEAD` git's bytes, and `config` git's less macOS's two probes.
#[test]
fn the_layout_is_what_git_init_and_index_pack_make() {
  let (repository, mut store) = served_encoded();
  let view = open(&repository, &mut store, &SERVED_REFS, None);
  let (ours, our_packs) = split_packs(&walk(&view));
  let (theirs, their_packs) = split_packs(&shown_by_the_view(BARE_LISTING));
  assert_eq!(ours, theirs);
  let all_named = our_packs
    .iter()
    .chain(&their_packs)
    .all(|line| named_as_git_names(line, ObjectFormat::Sha1));
  assert!(all_named, "{our_packs:?} {their_packs:?}");
  assert_eq!(our_packs.len(), 2 * view.packs().count());
  let seals = BTreeMap::new();
  assert_eq!(read_file(&view, &store, &seals, b"HEAD", 1), INIT_HEAD);
  let config = read_file(&view, &store, &seals, b"config", 1);
  assert_eq!(config, without_probes(INIT_CONFIG));
  close(view, seals, &mut store);
}

/// A-665 (`make-view-fixtures.sh` 1, `--object-format=sha256`). Do: open the view of an empty
/// SHA-256 repository and read its `config`. Expect: git's bytes less macOS's two probes,
/// `[extensions] objectformat` before `[core]` and version 1.
#[test]
fn a_sha256_repositorys_config_is_what_git_init_writes() {
  let empty = Repository::new(ObjectFormat::Sha256);
  let mut store = store();
  let view = open(&empty, &mut store, &[], None);
  let config = read_file(&view, &store, &BTreeMap::new(), b"config", 3);
  assert_eq!(config, without_probes(INIT_SHA256_CONFIG));
  view.close(&mut store).unwrap();
}

/// A-665 (`make-view-fixtures.sh` 2: `git pack-refs --all`). Do: read the served repository's
/// `packed-refs` whole and in windows of every size up to its length. Expect: git's bytes each
/// time, `v1` an annotated tag followed by its peeled line.
#[test]
fn packed_refs_are_what_git_pack_refs_writes() {
  let (repository, mut store) = served_encoded();
  let view = open(&repository, &mut store, &SERVED_REFS, None);
  let seals = BTreeMap::new();
  for window in 1..=to_u64(BARE_PACKED_REFS.len()) {
    let read = read_file(&view, &store, &seals, b"packed-refs", window);
    assert_eq!(read, BARE_PACKED_REFS, "window {window}");
  }
  close(view, seals, &mut store);
}

/// A part's entries as given, whatever the store keeps.
struct Given<'k>(&'k Kept);

impl PartEntries for Given<'_> {
  fn placed(&self, _part: &Part) -> Option<&[Placed]> {
    Some(&self.0.placed)
  }
}

/// `pack`'s objects as a kept part, its entries read from git's own index `idx`.
fn git_part(store: &mut ContentStore, pack: &[u8], idx: &[u8], format: ObjectFormat) -> Kept {
  let index = Idx::decode(idx, format).unwrap();
  let body = &pack[12..pack.len() - format.raw_len()];
  let mut placed: Vec<Placed> = index
    .entries()
    .map(|entry| Placed {
      id: entry.id,
      position: 0,
      kind: Kind::Blob,
      offset: entry.offset - PACK_HEADER_LEN,
      crc: entry.crc32,
      depth: 0,
      stored: Stored::Whole,
      base: None,
    })
    .collect();
  placed.sort_by_key(|entry| entry.offset);
  Kept {
    content: store.put_object(body).unwrap(),
    len: to_u64(body.len()),
    placed,
    positions: Positions::new(),
    references: Positions::new(),
    offsets: 0,
  }
}

/// A-665 (the view's packs; `make-view-fixtures.sh` 2 holds `index-pack`'s index of `ofs.pack`
/// to `ofs.idx`). Do: lay git's own packs out as the view lays a kept part out, SHA-1 and
/// SHA-256, seal each and read it whole. Expect: the pack git wrote, header, trailer and every
/// byte, and the index `index-pack` writes for it, byte for byte; the lengths and the count the
/// layout gave before a byte was read.
#[test]
fn a_pack_git_wrote_laid_out_and_sealed_reads_back_with_index_packs_index() {
  let cases = [
    (OFS_PACK, OFS_IDX, ObjectFormat::Sha1),
    (SHA256_PACK, SHA256_IDX, ObjectFormat::Sha256),
  ];
  for (pack, idx, format) in cases {
    let mut store = store();
    let kept = git_part(&mut store, pack, idx, format);
    let source = Source::Version {
      green: 0,
      version: 0,
    };
    let mut room = Admission::new(ROOM);
    let unit = lay_out(format, PackKind::Fragments, &[(source, false, &kept)], Vec::new(), &mut room)
      .unwrap();
    let count = u32::from_be_bytes(pack[8..12].try_into().unwrap());
    let lens = (unit.len, unit.idx_len, unit.count);
    assert_eq!(lens, (to_u64(pack.len()), to_u64(idx.len()), count));
    let mut sealing = Sealing::of(&unit);
    let mut clock = Ticks(0);
    while !sealing.step_on(&unit, &store, one_unit(&mut clock)).unwrap() {}
    let seal = sealing
      .finish_with(&unit, &Given(&kept), &mut store, &mut room)
      .unwrap();
    let mut read = Vec::new();
    unit.fill(&store, seal.known(), (0, unit.len), &mut read).unwrap();
    assert_eq!(read, pack);
    let (content, len) = seal.index();
    let mut index = Vec::new();
    content_range(&store, content, len, (0, len), &mut index).unwrap();
    assert_eq!(index, idx);
  }
}

/// The greens of [`built`]'s repository: `main`.
const MAIN_GREEN: u64 = 1;
/// A branch made at `main`'s second version.
const SIDE_GREEN: u64 = 2;
/// An annotated tag of `main`'s second version.
const TAG_GREEN: u64 = 3;

/// A repository with every case of the view, its refs, and a push pending on it with its head.
struct Built {
  repository: Repository,
  store: ContentStore,
  refs: Vec<(Vec<u8>, u64, usize)>,
  push: Push,
  next: ObjectId,
}

/// An annotated tag of the commit `object`.
fn tag_of(format: ObjectFormat, object: ObjectId) -> Made {
  let text = format!(
    "object {object}\ntype commit\ntag v1\ntagger T Agger <tagger@example.com> 5 +0000\n\nversion one\n"
  );
  made(format, Kind::Tag, text.into_bytes())
}

/// Pushes `objects` and moves `green` to `head`.
fn moved(built: &mut Built, objects: Vec<Made>, (green, head): (u64, ObjectId)) {
  push_and_move(
    &mut built.repository,
    &mut built.store,
    objects,
    &[(green, Some(head))],
  )
  .unwrap();
}

/// Encodes `green`'s version `version`.
fn encoded(built: &mut Built, green: u64, version: usize) {
  built
    .repository
    .encode_fragments(&mut built.store, (green, version), &mut Ticks(0), EXCHANGE)
    .unwrap();
}

/// The repository of every case: `main` at a commit over a 200-line text, then with a line of it
/// edited (the text a reference delta against the one before), both encoded; `side` made at
/// `main`'s head and encoded; both greens' base packs made (`side`'s holding nothing `main`'s does
/// not); `main` again, a line more edited and with a blob nothing reaches, encoded (its text's
/// base held only by the base pack, so held whole beside it); `main` once more, not encoded; an
/// annotated tag of `main`'s second commit, encoded; and a push of `main`'s next commit pending.
fn built(format: ObjectFormat) -> Built {
  let mut built = Built {
    repository: Repository::new(format),
    store: store(),
    refs: Vec::new(),
    push: Push::new(format),
    next: ObjectId::null(format),
  };
  let first = commit_over(format, &text(200, &[]), &[], 1);
  let second = commit_over(format, &text(200, &[120]), &[first[0].id], 2);
  let (first_head, second_head) = (first[0].id, second[0].id);
  moved(&mut built, first, (MAIN_GREEN, first_head));
  moved(&mut built, second, (MAIN_GREEN, second_head));
  moved(&mut built, Vec::new(), (SIDE_GREEN, second_head));
  for (green, version) in [(MAIN_GREEN, 0), (MAIN_GREEN, 1), (SIDE_GREEN, 0)] {
    encoded(&mut built, green, version);
  }
  for green in [MAIN_GREEN, SIDE_GREEN] {
    built
      .repository
      .rebuild_base(&mut built.store, green, &mut Ticks(0), EXCHANGE)
      .unwrap();
  }
  later_versions(&mut built, format, second_head);
  built
}

/// [`built`]'s versions after the base packs.
fn later_versions(built: &mut Built, format: ObjectFormat, second: ObjectId) {
  let mut third = commit_over(format, &text(200, &[120, 7]), &[second], 3);
  let third_head = third[0].id;
  third.push(blob(format, b"a blob nothing reaches\n"));
  moved(built, third, (MAIN_GREEN, third_head));
  encoded(built, MAIN_GREEN, 2);
  let fourth = commit_over(format, &text(200, &[120, 7, 50]), &[third_head], 4);
  let fourth_head = fourth[0].id;
  moved(built, fourth, (MAIN_GREEN, fourth_head));
  let tag = tag_of(format, second);
  let tag_id = tag.id;
  moved(built, vec![tag], (TAG_GREEN, tag_id));
  encoded(built, TAG_GREEN, 0);
  let next = commit_over(format, &text(200, &[120, 7, 50, 9]), &[fourth_head], 5);
  built.next = next[0].id;
  built.push = sealed(&built.repository, &mut built.store, next);
  built.refs = vec![
    (b"refs/heads/main".to_vec(), MAIN_GREEN, 3),
    (b"refs/heads/side".to_vec(), SIDE_GREEN, 0),
    (b"refs/tags/v1".to_vec(), TAG_GREEN, 0),
  ];
}

/// [`Built`]'s refs as a snapshot's.
fn shown(refs: &[(Vec<u8>, u64, usize)]) -> Vec<ShownRef<'_>> {
  refs
    .iter()
    .map(|(name, green, version)| ShownRef {
      name,
      green: *green,
      version: *version,
    })
    .collect()
}

/// Every pack file's path in `view`: the repository's and the quarantine's.
fn pack_paths(view: &HookView) -> Vec<Vec<u8>> {
  walk(view)
    .into_iter()
    .filter(|line| line.starts_with("f ") && line.ends_with(".pack"))
    .map(|line| path_of(&line).as_bytes().to_vec())
    .collect()
}

/// A pack's path with its index's suffix.
fn index_path(path: &[u8]) -> Vec<u8> {
  [&path[..path.len() - b".pack".len()], b".idx".as_slice()].concat()
}

/// The objects of the pack at `path`, checked with its index as `git verify-pack` checks them:
/// the trailer the checksum of every byte before it, every object inflated and every delta
/// resolved within the pack, the index holding exactly the pack's objects at their offsets with
/// their CRC32s.
fn verified(
  view: &HookView,
  store: &ContentStore,
  seals: &BTreeMap<ObjectId, Seal>,
  (path, window): (&[u8], u64),
  format: ObjectFormat,
) -> BTreeSet<ObjectId> {
  let bytes = read_file(view, store, seals, path, window);
  let checked = pack::check(&bytes, format).unwrap();
  let unpacked = read_whole(&bytes, format, &NoIndex, &mut Admission::new(ROOM)).unwrap();
  let idx_bytes = read_file(view, store, seals, &index_path(path), window);
  let index = Idx::decode(&idx_bytes, format).unwrap();
  index
    .check_pack(&checked, &mut Admission::new(ROOM))
    .unwrap();
  check_index(&unpacked, &index).unwrap();
  unpacked.objects.iter().map(|object| object.id).collect()
}

/// Every object the store holds, by position, and the push's.
fn every_object(built: &Built) -> BTreeSet<ObjectId> {
  let format = built.repository.format();
  let mut ids: BTreeSet<ObjectId> = (0..built.repository.next_position())
    .map(|position| built.repository.id_at(&built.store, position).unwrap())
    .collect();
  ids.extend(
    built
      .push
      .held()
      .map(|(id, ..)| ObjectId::from_bytes(format, id).unwrap()),
  );
  ids
}

/// A-665 (the oracle: every object reachable through the view's packs, counted). Do: open the
/// view of [`built`]'s repository with its push quarantined, SHA-1 and SHA-256; seal every pack;
/// read each pack and its index from the view (in windows of 7 bytes, and of 1 KiB) and check
/// them as `git verify-pack` does. Expect: every pack whole by itself, its index git's for it;
/// together every object the store holds and every object of the push; and each way the view takes
/// a pack counted: a base pack shown and one passed over, two versions' fragments, a reference
/// delta's base held whole beside them, the version not encoded and the blob nothing reaches held
/// whole, and the push's four objects.
#[test]
fn every_object_of_the_store_and_the_push_is_in_the_views_packs_counted() {
  for (format, window) in [(ObjectFormat::Sha1, 7), (ObjectFormat::Sha256, 1 << 10)] {
    let mut built = built(format);
    let refs = shown(&built.refs);
    let pushed = Some(Pushed::Quarantined(&built.push));
    let view = open(&built.repository, &mut built.store, &refs, pushed);
    let seals = seal_all(&view, &built.repository, &mut built.store);
    let mut seen = BTreeSet::new();
    for path in pack_paths(&view) {
      seen.extend(verified(&view, &built.store, &seals, (&path, window), format));
    }
    assert_eq!(seen, every_object(&built));
    assert_counted(view.counters(), &built);
    close(view, seals, &mut built.store);
  }
}

/// The counters [`built`]'s view shows (the oracle's non-vacuity): every count exact but the bases
/// held whole beside a version's fragments, at least the edited text's (its tree's delta wins or
/// not by the learned rules).
fn assert_counted(counters: ViewCounters, built: &Built) {
  assert!(counters.completed >= 1, "{counters:?}");
  let expected = ViewCounters {
    base_packs: 1,
    fragments: 2,
    skipped: 1,
    completed: counters.completed,
    rest: 4,
    pushed: to_u64(built.push.len()),
  };
  assert_eq!(counters, expected);
}

/// A-665 (lazy, a window a read). Do: before any seal, read each pack's trailer and its index, and
/// a base pack up to its trailer; seal; read every file in windows of 1, 13, a content window and
/// the whole, and from its end. Expect: unsealed, the trailers and indexes refused naming their
/// packs and the base pack's bytes before its trailer read; sealed, every file the same bytes in
/// every window, and nothing from an end.
#[test]
fn every_window_reads_the_same_bytes_and_only_what_the_seal_knows_waits_for_it() {
  let mut built = built(ObjectFormat::Sha1);
  let refs = shown(&built.refs);
  let pushed = Some(Pushed::Quarantined(&built.push));
  let view = open(&built.repository, &mut built.store, &refs, pushed);
  for path in pack_paths(&view) {
    refused_before_the_seal(&view, &built.store, &path);
  }
  let seals = seal_all(&view, &built.repository, &mut built.store);
  for line in walk(&view).iter().filter(|line| line.starts_with("f ")) {
    same_in_every_window(&view, &built.store, &seals, path_of(line).as_bytes());
  }
  close(view, seals, &mut built.store);
}

/// A pack's reads before its seal (the test above).
fn refused_before_the_seal(view: &HookView, store: &ContentStore, path: &[u8]) {
  let Some(Node::File { len }) = view.node(path) else {
    panic!("no pack");
  };
  let shown = String::from_utf8_lossy(path).into_owned();
  let (pack, kind) = view
    .packs()
    .find(|(name, _)| shown.contains(&name.to_string()))
    .unwrap();
  let trailer = len - to_u64(pack.format().raw_len());
  let unsealed: BTreeMap<ObjectId, Seal> = BTreeMap::new();
  let refused = Err(ReadRefused::Unsealed { pack });
  let mut out = Vec::new();
  assert_eq!(view.read(store, &unsealed, path, (trailer, 1), &mut out), refused);
  let index = index_path(path);
  assert_eq!(view.read(store, &unsealed, &index, (0, 1), &mut out), refused);
  if kind == PackKind::BasePack {
    view.read(store, &unsealed, path, (0, trailer), &mut out).unwrap();
    assert_eq!(to_u64(out.len()), trailer);
  }
}

/// A file's bytes in every window (the test above).
fn same_in_every_window(
  view: &HookView,
  store: &ContentStore,
  seals: &BTreeMap<ObjectId, Seal>,
  path: &[u8],
) {
  let Some(Node::File { len }) = view.node(path) else {
    panic!("no file");
  };
  let whole = read_file(view, store, seals, path, len.max(1));
  for window in [1, 13, to_u64(store.window())] {
    assert_eq!(read_file(view, store, seals, path, window), whole);
  }
  let mut past = Vec::new();
  view.read(store, seals, path, (len, 64), &mut past).unwrap();
  assert!(past.is_empty());
}

/// The `GIT_*` variables a fixture's hook saw beyond the pushing process's own, less
/// `GIT_EXEC_PATH` and the push's options, which the view leaves to the hook's `git` and to the
/// hook driver.
fn hooks_own(fixture: &str) -> Vec<(String, Vec<u8>)> {
  let inherited: BTreeSet<&str> = PUSHER_ENV.lines().collect();
  fixture
    .lines()
    .filter_map(|line| line.strip_prefix("env "))
    .filter(|variable| !inherited.contains(variable))
    .filter_map(|variable| variable.split_once('='))
    .filter(|(name, _)| *name != "GIT_EXEC_PATH" && !name.starts_with("GIT_PUSH_OPTION_"))
    .map(|(name, value)| (name.to_owned(), value.as_bytes().to_vec()))
    .collect()
}

/// `value` with the six letters after the quarantine's prefix written `XXXXXX`, as the fixtures
/// write `mkdtemp`'s letters.
fn letters_hidden(mut value: Vec<u8>) -> Vec<u8> {
  let (prefix, hidden) = QUARANTINE;
  if let Some(at) = value
    .windows(prefix.len())
    .position(|window| window == prefix)
  {
    let letters = at + prefix.len();
    value[letters..letters + hidden.len()].copy_from_slice(hidden);
  }
  value
}

/// `view`'s environment rooted at `$ROOT`, the quarantine's letters hidden.
fn ours(view: &HookView) -> Vec<(String, Vec<u8>)> {
  view
    .environment(ROOT)
    .into_iter()
    .map(|(name, value)| (name.to_owned(), letters_hidden(value)))
    .collect()
}

/// A-665 (`make-view-fixtures.sh` 3). Do: open the served repository's view with a push
/// quarantined, migrated, and with none; ask each for a hook's environment at `$ROOT`, and list
/// the quarantine. Expect: `pre-receive`'s quarantine variables and `GIT_DIR` as stock
/// receive-pack set them in both of its pushes (loose and packed), `update`'s and
/// `post-receive`'s `GIT_DIR` alone; the quarantine a `pack` directory of one pack and its index,
/// as receive-pack's is less the `.keep` and `.rev`.
#[test]
fn the_hook_environment_is_what_receive_pack_hands_its_hooks() {
  let (repository, mut store) = served_encoded();
  let next = commit_over(ObjectFormat::Sha1, b"pushed\n", &[served::id(served::THIRD)], 9);
  let push = sealed(&repository, &mut store, next);
  let quarantined = open(
    &repository,
    &mut store,
    &SERVED_REFS,
    Some(Pushed::Quarantined(&push)),
  );
  assert_eq!(ours(&quarantined), hooks_own(LOOSE_PRE_RECEIVE));
  assert_eq!(ours(&quarantined), hooks_own(PACK_PRE_RECEIVE));
  quarantine_listed(&quarantined);
  quarantined.close(&mut store).unwrap();
  let migrated = open(
    &repository,
    &mut store,
    &SERVED_REFS,
    Some(Pushed::Migrated(&push)),
  );
  assert_eq!(ours(&migrated), hooks_own(LOOSE_UPDATE));
  migrated.close(&mut store).unwrap();
  let after = open(&repository, &mut store, &SERVED_REFS, None);
  assert_eq!(ours(&after), hooks_own(LOOSE_POST_RECEIVE));
  after.close(&mut store).unwrap();
  push.abandon(&mut store).unwrap();
}

/// The names `view` lists at `path`.
fn names(view: &HookView, path: &[u8]) -> Vec<String> {
  view
    .list(path)
    .unwrap()
    .into_iter()
    .map(|listed| String::from_utf8(listed.name).unwrap())
    .collect()
}

/// The quarantine's listing (the test above): `objects` holds it after `info` and `pack`, and it
/// holds `pack`, which holds the push's index and its pack.
fn quarantine_listed(view: &HookView) {
  let objects = names(view, b"objects");
  assert_eq!(objects[..2], ["info".to_owned(), "pack".to_owned()]);
  let (prefix, letters) = QUARANTINE;
  let quarantine = &objects[2];
  assert!(quarantine.starts_with(std::str::from_utf8(prefix).unwrap()));
  assert_eq!(quarantine.len(), prefix.len() + letters.len());
  let directory = format!("objects/{quarantine}");
  assert_eq!(names(view, directory.as_bytes()), ["pack".to_owned()]);
  let files = names(view, format!("{directory}/pack").as_bytes());
  let suffixes: Vec<&str> = files
    .iter()
    .map(|file| file.rsplit('.').next().unwrap())
    .collect();
  assert_eq!(suffixes, ["idx", "pack"]);
}

/// The value `view` gives `name` at `root`.
fn value_at(view: &HookView, root: &[u8], name: &str) -> Vec<u8> {
  view
    .environment(root)
    .into_iter()
    .find(|(variable, _)| *variable == name)
    .unwrap()
    .1
}

/// A-665 (`env_append`'s quoting). Do: ask a quarantined view for its environment at a root
/// holding `:`, then at one holding `:`, a double quote, a byte past ASCII and a tab. Expect: the
/// alternates' value quoted as `quote_c_style` quotes it, the quarantine's own as it is.
#[test]
fn an_alternates_path_holding_the_separator_is_quoted_as_git_quotes_it() {
  let (repository, mut store) = served_encoded();
  let push = sealed(&repository, &mut store, Vec::new());
  let view = open(
    &repository,
    &mut store,
    &SERVED_REFS,
    Some(Pushed::Quarantined(&push)),
  );
  let alternates = "GIT_ALTERNATE_OBJECT_DIRECTORIES";
  let plain = value_at(&view, b"/srv/a:b", alternates);
  assert_eq!(plain, b"\"/srv/a:b/./objects\"");
  let odd = b"/srv/a:\"\xc3\xa9\tb";
  let quoted = value_at(&view, odd, alternates);
  assert_eq!(quoted, b"\"/srv/a:\\\"\\303\\251\\tb/./objects\"");
  assert!(value_at(&view, odd, "GIT_QUARANTINE_PATH").starts_with(odd));
  view.close(&mut store).unwrap();
  push.abandon(&mut store).unwrap();
}

/// A-665 (typed refusals). Do: open the served repository's view with `HEAD` outside `refs/`, a
/// ref name git refuses, a name twice, and a version the store lacks. Expect: each refused by its
/// kind, with nothing held.
#[test]
fn each_snapshot_refusal_is_typed_and_holds_nothing() {
  let (repository, mut store) = served_encoded();
  let kept = repository.base_part(MAIN, false).unwrap().1.content;
  let before = store.holders(&kept);
  let bad_name = [ShownRef {
    name: b"refs/heads/a b",
    ..SERVED_REFS[0]
  }];
  let twice = [SERVED_REFS[0], SERVED_REFS[0]];
  let missing = [ShownRef {
    version: 7,
    ..SERVED_REFS[0]
  }];
  let no_version = ViewRefused::NoVersion {
    green: MAIN,
    version: 7,
  };
  let cases: [(&[ShownRef<'_>], &[u8], ViewRefused); 4] = [
    (&SERVED_REFS, b"HEAD", ViewRefused::HeadName),
    (&bad_name, DEFAULT_BRANCH, ViewRefused::RefName { at: 0 }),
    (&twice, DEFAULT_BRANCH, ViewRefused::Duplicate { at: 1 }),
    (&missing, DEFAULT_BRANCH, no_version),
  ];
  for (refs, head, refused) in cases {
    let snapshot = Snapshot { refs, head };
    let opened = HookView::open(&repository, &mut store, &snapshot, None, &mut Admission::new(ROOM));
    assert_eq!(opened.unwrap_err(), refused);
  }
  assert_eq!(store.holders(&kept), before);
}

/// A-665 (typed refusals; holds given back). Do: open the served repository's view with another
/// format's push and with an admission too small; then open one and close it. Expect: each refused
/// by its kind with nothing held; the open view holding `main`'s base pack once more, and nothing
/// once closed.
#[test]
fn a_foreign_push_and_too_little_room_are_refused_and_a_closed_view_holds_nothing() {
  let (repository, mut store) = served_encoded();
  let kept = repository.base_part(MAIN, false).unwrap().1.content;
  let before = store.holders(&kept);
  let snapshot = Snapshot {
    refs: &SERVED_REFS,
    head: DEFAULT_BRANCH,
  };
  let foreign = Push::new(ObjectFormat::Sha256);
  let pushed = Some(Pushed::Quarantined(&foreign));
  let opened = HookView::open(&repository, &mut store, &snapshot, pushed, &mut Admission::new(ROOM));
  assert_eq!(opened.unwrap_err(), ViewRefused::ForeignPush);
  let opened = HookView::open(&repository, &mut store, &snapshot, None, &mut Admission::new(64));
  assert!(matches!(opened, Err(ViewRefused::PastAdmission(_))));
  assert_eq!(store.holders(&kept), before);
  let view = open(&repository, &mut store, &SERVED_REFS, None);
  assert_eq!(store.holders(&kept), before + 1);
  view.close(&mut store).unwrap();
  assert_eq!(store.holders(&kept), before);
}

/// A-665 (holds; a base pack made again). Do: open [`built`]'s view; commit its push and make
/// `main`'s base pack again, so the store lets the old one go; read the old base pack, then seal
/// it. Expect: the old base pack still read whole up to its trailer, as the view holds it; its
/// seal refused `Changed`, the part kept there now not the one the view shows.
#[test]
fn a_base_pack_made_again_stays_readable_and_its_seal_is_refused_by_kind() {
  let mut built = built(ObjectFormat::Sha1);
  let refs = shown(&built.refs);
  let view = open(&built.repository, &mut built.store, &refs, None);
  let (base, _) = view
    .packs()
    .find(|(_, kind)| *kind == PackKind::BasePack)
    .unwrap();
  let mut push = std::mem::replace(&mut built.push, Push::new(ObjectFormat::Sha1));
  let admitted = built
    .repository
    .admit(&built.store, &push, MAIN_GREEN, Some(built.next))
    .unwrap();
  built
    .repository
    .commit(&mut built.store, &mut push, &[admitted])
    .unwrap();
  built
    .repository
    .rebuild_base(&mut built.store, MAIN_GREEN, &mut Ticks(0), EXCHANGE)
    .unwrap();
  let path = format!("objects/pack/pack-{base}.pack");
  let Some(Node::File { len }) = view.node(path.as_bytes()) else {
    panic!("no base pack");
  };
  let before_trailer = (0, len - to_u64(base.format().raw_len()));
  let mut bytes = Vec::new();
  let unsealed: BTreeMap<ObjectId, Seal> = BTreeMap::new();
  view
    .read(&built.store, &unsealed, path.as_bytes(), before_trailer, &mut bytes)
    .unwrap();
  let sealed = seal_one(&view, &built.repository, &mut built.store, &base);
  assert_eq!(sealed, Err(SealRefused::Changed { pack: base }));
  view.close(&mut built.store).unwrap();
}

/// Format: the tag before each line [`a_view_for_stock_git`] prints, so its reader finds them
/// among libtest's own lines, which share standard output and may begin a line it ends.
const TAG: &str = "A665-VIEW";

/// Main's run (stock git over the view, before part 2 mounts one): [`built`]'s view with its push
/// quarantined, every pack sealed, on standard output a tagged line a path, `d <path>` or
/// `f <path> <bytes in hexadecimal>`, then `env <name>=<value in hexadecimal>` for each variable
/// `pre-receive` is handed at the root `SLATES_HOOK_VIEW_ROOT` names (`$ROOT` when unset). The run
/// request (g293's progress file and the run-request queue) makes the tree again in a RAM disk at
/// that root and runs stock `git fsck`, `git verify-pack -v` on every index, and `git log` and
/// `git cat-file --batch-check --batch-all-objects` in the hook's environment.
#[test]
#[ignore = "main's run: stock git over the view"]
fn a_view_for_stock_git() {
  let mut built = built(ObjectFormat::Sha1);
  let refs = shown(&built.refs);
  let pushed = Some(Pushed::Quarantined(&built.push));
  let view = open(&built.repository, &mut built.store, &refs, pushed);
  let seals = seal_all(&view, &built.repository, &mut built.store);
  let hex = |bytes: &[u8]| -> String { bytes.iter().map(|byte| format!("{byte:02x}")).collect() };
  let mut out = String::new();
  for line in walk(&view) {
    let path = path_of(&line);
    if line.starts_with("d ") {
      out.push_str(&format!("{TAG} d {path}\n"));
    } else {
      let bytes = read_file(&view, &built.store, &seals, path.as_bytes(), 1 << 16);
      out.push_str(&format!("{TAG} f {path} {}\n", hex(&bytes)));
    }
  }
  let root = std::env::var("SLATES_HOOK_VIEW_ROOT").unwrap_or_else(|_| "$ROOT".to_owned());
  for (name, value) in view.environment(root.as_bytes()) {
    out.push_str(&format!("{TAG} env {name}={}\n", hex(&value)));
  }
  std::io::Write::write_all(&mut std::io::stdout(), out.as_bytes()).unwrap();
  close(view, seals, &mut built.store);
}
