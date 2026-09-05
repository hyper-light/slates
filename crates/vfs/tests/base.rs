//! The base plane's tests (Phase 1 task 10; §4.5, §4.15, D-25) over the simulated host: the
//! worked example of Phase 1, AC-1.9 (create costs one open, memory independent of the tree),
//! AC-1.10 (the overlay holds exactly the diverged entries, witnesses equal the disk at
//! copy-up), AC-1.11 (drift never absorbed; `BaseDrift` instead of torn bytes), T-1.11 (the racy
//! rule), T-1.12 (`rm -r` of a large base directory then two files inside), T-1.13 (watcher
//! overflow), and T-1.10's oracle over generated histories with outsider edits.

// Test harness code: an unwrap here is a failed test, which is what it should be. proptest's
// strategy types carry `Arc` (D-8's harness exception).
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::disallowed_types,
  clippy::panic
)]

use std::collections::{BTreeMap, BTreeSet};

use proptest::prelude::*;
use slates_vfs::base::{BaseConfig, Divergence, DriftKind};
use slates_vfs::clock::StepClock;
use slates_vfs::dir::Child;
use slates_vfs::error::VfsError;
use slates_vfs::host::sim::SimHost;
use slates_vfs::host::{HostFs, HostKind, WatchState};
use slates_vfs::inode::{Fingerprint, Kind};
use slates_vfs::names::NameEquivalence;
use slates_vfs::quota::Quota;
use slates_vfs::volume::{Store, Volume, VolumeConfig};

mod common;
use common::store;

/// Shape: the large-file class boundary these tests use, one chunk window, so a file of two
/// windows is large class and pins one window at a time.
const LARGE: u64 = 65_536;

fn overlay(host: &mut SimHost, store: &mut Store) -> Volume {
  let root = host.root();
  let facts = host.facts(root).unwrap();
  Volume::create_overlay(
    store,
    VolumeConfig {
      prefix: 7,
      names: NameEquivalence::Exact,
      quota: Quota::Bounded { limit: 1 << 30 },
      journal_bytes: 1 << 20,
      clock: Box::new(StepClock::new(1_000_000, 1_000)),
    },
    BaseConfig {
      root,
      facts,
      large_class_bytes: LARGE,
    },
  )
  .unwrap()
}

fn read_all(
  vol: &mut Volume,
  host: &mut SimHost,
  store: &mut Store,
  path: &str,
) -> Result<Vec<u8>, VfsError> {
  let mut o = vol.with_host(host);
  let no = o.resolve(store, path)?.inode;
  let size = o.stat(store, no)?.size;
  let mut buf = vec![0u8; usize::try_from(size).unwrap()];
  let n = o.read(store, no, 0, &mut buf)?;
  buf.truncate(n);
  Ok(buf)
}

/// A base tree of `files` files in `dirs` directories.
fn populate(host: &mut SimHost, dirs: usize, files: usize) {
  for d in 0..dirs {
    host.mkdir(&format!("/d{d}"));
  }
  for f in 0..files {
    let d = f % dirs.max(1);
    host.replace_file(&format!("/d{d}/f{f}"), format!("file {f}").as_bytes());
  }
}

/// AC-1.9: create over a base directory costs one directory open regardless of tree size, and
/// memory after create is independent of the tree.
#[test]
fn create_over_a_base_costs_one_open_and_no_memory_whatever_the_tree_size() {
  for (dirs, files) in [(4usize, 1_000usize), (64, 100_000), (256, 1_000_000)] {
    let mut host = SimHost::new();
    populate(&mut host, dirs, files);
    let mut store = store();
    let vol = overlay(&mut host, &mut store);
    assert_eq!(
      host.open_handles(),
      1,
      "one directory open at {files} files"
    );
    assert_eq!(store.dirs.iter().count(), 1, "the root node only");
    assert_eq!(store.inodes.iter().count(), 1, "the root inode only");
    assert!(vol.diverged(&store).is_empty());
  }
}

/// The base of the worked example: `src/lib.rs` and `src/main.rs`, with `lib.rs` read once
/// and then edited by the agent (copied up).
fn worked_example() -> (SimHost, Store, Volume, slates_vfs::ids::InodeNo) {
  let mut host = SimHost::new();
  host.mkdir("/src");
  host.replace_file("/src/lib.rs", b"pub fn lib() {}");
  host.replace_file("/src/main.rs", b"fn main() {}");
  let mut store = store();
  let mut vol = overlay(&mut host, &mut store);
  assert_eq!(
    read_all(&mut vol, &mut host, &mut store, "/src/lib.rs").unwrap(),
    b"pub fn lib() {}"
  );
  assert_eq!(
    host.open_handles(),
    3,
    "the root, src, and the file's descriptor"
  );
  let lib = vol
    .with_host(&mut host)
    .resolve(&mut store, "/src/lib.rs")
    .unwrap()
    .inode;
  assert!(vol.diverged(&store).is_empty(), "a lookup does not diverge");
  vol
    .with_host(&mut host)
    .write(&mut store, lib, 0, b"pub")
    .unwrap();
  (host, store, vol, lib)
}

/// The overlay worked example of §4.4 and Phase 1, first half: a lookup loads one listing; a
/// write copies up with a witness equal to the disk.
#[test]
fn the_overlay_worked_example_copies_up_with_a_witness_equal_to_the_disk() {
  let (mut host, mut store, mut vol, lib) = worked_example();
  let witness = vol
    .base_plane()
    .unwrap()
    .witness(lib)
    .expect("witnessed at copy-up");
  assert_eq!(
    witness.fingerprint,
    host.fingerprint("/src/lib.rs").unwrap()
  );
  assert_eq!(
    witness.identity,
    *blake3::hash(b"pub fn lib() {}").as_bytes()
  );
  assert!(!witness.racy);
  assert_eq!(
    read_all(&mut vol, &mut host, &mut store, "/src/lib.rs").unwrap(),
    b"pub fn lib() {}"
  );
  let diverged = vol.diverged(&store);
  assert_eq!(diverged.len(), 1);
  assert_eq!(diverged[0].path, "/src/lib.rs");
  assert_eq!(diverged[0].kind, Divergence::Witnessed);
}

/// The worked example, second half: `git pull` replaces both files on the host; drift is
/// reported on the witnessed entry and nothing else; `read_base` shows the disk; `rewitness`
/// clears the drift and leaves the agent's content alone.
#[test]
fn the_overlay_worked_example_reports_drift_and_rewitnesses() {
  let (mut host, mut store, mut vol, _) = worked_example();
  host.advance_ns(1_000_000_000);
  host.replace_file("/src/lib.rs", b"pulled lib");
  host.replace_file("/src/main.rs", b"pulled main");
  let status = vol.with_host(&mut host).status(&mut store).unwrap();
  assert_eq!(
    status.drift,
    vec![("/src/lib.rs".to_owned(), DriftKind::Replaced)]
  );
  assert_eq!(
    read_all(&mut vol, &mut host, &mut store, "/src/lib.rs").unwrap(),
    b"pub fn lib() {}",
    "the agent's bytes"
  );
  assert_eq!(
    read_all(&mut vol, &mut host, &mut store, "/src/main.rs").unwrap(),
    b"pulled main",
    "untouched shows the live disk"
  );
  assert_eq!(
    vol.with_host(&mut host).read_base("/src/lib.rs").unwrap(),
    b"pulled lib"
  );
  let redone = vol
    .with_host(&mut host)
    .rewitness(&mut store, None)
    .unwrap();
  assert_eq!(redone, vec!["/src/lib.rs".to_owned()]);
  assert!(
    vol
      .with_host(&mut host)
      .status(&mut store)
      .unwrap()
      .drift
      .is_empty()
  );
  assert_eq!(
    read_all(&mut vol, &mut host, &mut store, "/src/lib.rs").unwrap(),
    b"pub fn lib() {}",
    "content untouched by rewitness"
  );
}

/// AC-1.11: a large-class base file whose remaining extents are served from disk is truncated
/// in place by the user; a read of an unpinned range returns `BaseDrift`, never torn bytes, and
/// the agent's own written window is intact.
#[test]
fn an_in_place_overwrite_beneath_a_large_file_is_base_drift_never_torn_bytes() {
  let mut host = SimHost::new();
  let big: Vec<u8> = (0..(3 * LARGE))
    .map(|i| u8::try_from(i % 251).unwrap())
    .collect();
  host.replace_file("/data.bin", &big);
  let mut store = store();
  let mut vol = overlay(&mut host, &mut store);
  let f = vol
    .with_host(&mut host)
    .resolve(&mut store, "/data.bin")
    .unwrap()
    .inode;
  vol
    .with_host(&mut host)
    .write(&mut store, f, 10, b"agent")
    .unwrap();
  let mut buf = [0u8; 16];
  assert_eq!(
    vol
      .with_host(&mut host)
      .read(&mut store, f, 8, &mut buf)
      .unwrap(),
    16
  );
  assert_eq!(&buf[2..7], b"agent");
  // Only the first window is pinned; the rest still reads through the descriptor.
  assert_eq!(
    vol
      .with_host(&mut host)
      .read(&mut store, f, 2 * LARGE, &mut buf)
      .unwrap(),
    16
  );
  assert_eq!(buf[0], u8::try_from((2 * LARGE) % 251).unwrap());

  host.write_in_place(
    "/data.bin",
    &big[..usize::try_from(LARGE).unwrap()],
    5_000_000_000,
  );
  assert_eq!(
    vol
      .with_host(&mut host)
      .read(&mut store, f, 2 * LARGE, &mut buf),
    Err(VfsError::BaseDrift)
  );
  assert_eq!(
    vol
      .with_host(&mut host)
      .read(&mut store, f, 8, &mut buf)
      .unwrap(),
    16,
    "the pinned window still reads"
  );
  assert_eq!(&buf[2..7], b"agent");
  assert_eq!(
    vol.with_host(&mut host).status(&mut store).unwrap().drift,
    vec![("/data.bin".to_owned(), DriftKind::Modified)]
  );
}

/// T-1.11: a base file modified in place within the timestamp granularity without a size
/// change; the racy rule re-hashes and the drift is detected.
#[test]
fn a_racy_in_place_edit_is_caught_by_rehashing() {
  let mut host = SimHost::new();
  host.set_granularity_ns(1_000_000_000);
  host.replace_file("/notes.txt", b"version one");
  let mut store = store();
  let mut vol = overlay(&mut host, &mut store);
  let f = vol
    .with_host(&mut host)
    .resolve(&mut store, "/notes.txt")
    .unwrap()
    .inode;
  vol
    .with_host(&mut host)
    .chmod(&mut store, f, 0o600)
    .unwrap();
  let witness = vol.base_plane().unwrap().witness(f).unwrap();
  assert!(
    witness.racy,
    "the file's mtime is within one second of the listing"
  );
  // Same size, same timestamps: only the bytes differ.
  host.write_in_place("/notes.txt", b"version two", 0);
  assert_eq!(host.fingerprint("/notes.txt").unwrap(), witness.fingerprint);
  assert_eq!(
    vol.with_host(&mut host).status(&mut store).unwrap().drift,
    vec![("/notes.txt".to_owned(), DriftKind::Modified)]
  );
}

/// T-1.12: `rm -r` of a 40k-entry base directory then recreate two files inside; expect one
/// opaque directory over the whiteout, two overlay entries, and a merged listing of exactly two.
#[test]
fn removing_a_large_base_directory_and_recreating_two_files_is_one_opaque_directory() {
  /// Format: the design's entry count for this case.
  const ENTRIES: usize = 40_000;
  let mut host = SimHost::new();
  host.mkdir("/vendor");
  for i in 0..ENTRIES {
    host.replace_file(&format!("/vendor/pkg-{i:05}.txt"), b"x");
  }
  let mut store = store();
  let mut vol = overlay(&mut host, &mut store);
  let vendor = match vol
    .with_host(&mut host)
    .resolve(&mut store, "/vendor")
    .unwrap()
    .child
  {
    Child::Dir(h) => h,
    other => panic!("{other:?}"),
  };
  let names: Vec<String> = vol
    .with_host(&mut host)
    .readdir(&mut store, vendor)
    .unwrap()
    .iter()
    .map(|r| r.name.to_owned())
    .collect();
  assert_eq!(names.len(), ENTRIES);
  for n in &names {
    vol
      .with_host(&mut host)
      .unlink(&mut store, vendor, n)
      .unwrap();
  }
  let root = vol.root();
  vol
    .with_host(&mut host)
    .rmdir(&mut store, root, "vendor")
    .unwrap();
  let vendor = vol
    .with_host(&mut host)
    .mkdir(&mut store, root, "vendor", 0o755)
    .unwrap();
  vol
    .with_host(&mut host)
    .create_file(&mut store, vendor, "a.txt", 0o644)
    .unwrap();
  vol
    .with_host(&mut host)
    .create_file(&mut store, vendor, "b.txt", 0o644)
    .unwrap();
  let mut listed: Vec<String> = vol
    .with_host(&mut host)
    .readdir(&mut store, vendor)
    .unwrap()
    .iter()
    .map(|r| r.name.to_owned())
    .collect();
  listed.sort();
  assert_eq!(listed, vec!["a.txt".to_owned(), "b.txt".to_owned()]);
  let diverged = vol.diverged(&store);
  let kinds: Vec<(&str, Divergence)> = diverged.iter().map(|d| (d.path.as_str(), d.kind)).collect();
  assert_eq!(
    kinds,
    vec![
      ("/vendor", Divergence::Created),
      ("/vendor/a.txt", Divergence::Created),
      ("/vendor/b.txt", Divergence::Created),
    ]
  );
}

/// T-1.13: a watcher overflow is followed by a full re-check: no drift is missed and `status`
/// reports the watcher overflowed.
#[test]
fn a_watcher_overflow_rechecks_everything() {
  let mut host = SimHost::new();
  host.replace_file("/a.txt", b"a");
  host.replace_file("/b.txt", b"b");
  let mut store = store();
  let mut vol = overlay(&mut host, &mut store);
  let a = vol
    .with_host(&mut host)
    .resolve(&mut store, "/a.txt")
    .unwrap()
    .inode;
  let b = vol
    .with_host(&mut host)
    .resolve(&mut store, "/b.txt")
    .unwrap()
    .inode;
  vol
    .with_host(&mut host)
    .write(&mut store, a, 0, b"A")
    .unwrap();
  vol
    .with_host(&mut host)
    .write(&mut store, b, 0, b"B")
    .unwrap();
  assert_eq!(
    vol.with_host(&mut host).status(&mut store).unwrap().watcher,
    WatchState::Live
  );
  host.watcher_overflow();
  host.advance_ns(1);
  host.replace_file("/b.txt", b"changed while the watcher was deaf");
  let status = vol.with_host(&mut host).status(&mut store).unwrap();
  assert_eq!(status.watcher, WatchState::Overflowed);
  assert_eq!(
    status.drift,
    vec![("/b.txt".to_owned(), DriftKind::Replaced)]
  );
}

// ------------------------------------------------------------------ the oracle (T-1.10, AC-1.10)

/// One step of a generated history: the agent's or an outsider's.
#[derive(Clone, Debug)]
enum Move {
  /// The agent writes a base or created file (creating it if absent).
  Write(u8, Vec<u8>),
  /// The agent unlinks a file.
  Unlink(u8),
  /// The agent renames a file.
  Rename(u8, u8),
  /// An outsider replaces a disk file with a new inode.
  OutsiderReplace(u8, Vec<u8>),
  /// An outsider writes a disk file in place.
  OutsiderWrite(u8, Vec<u8>),
  /// An outsider deletes a disk file.
  OutsiderRemove(u8),
}

/// Format: the file names the histories use.
const NAMES: [&str; 6] = ["a", "b", "c", "d", "e", "f"];

fn name(i: u8) -> &'static str {
  NAMES[usize::from(i) % NAMES.len()]
}

fn moves() -> impl Strategy<Value = Move> {
  prop_oneof![
    (any::<u8>(), prop::collection::vec(any::<u8>(), 1..8)).prop_map(|(i, b)| Move::Write(i, b)),
    any::<u8>().prop_map(Move::Unlink),
    (any::<u8>(), any::<u8>()).prop_map(|(a, b)| Move::Rename(a, b)),
    (any::<u8>(), prop::collection::vec(any::<u8>(), 1..8))
      .prop_map(|(i, b)| Move::OutsiderReplace(i, b)),
    (any::<u8>(), prop::collection::vec(any::<u8>(), 1..8))
      .prop_map(|(i, b)| Move::OutsiderWrite(i, b)),
    any::<u8>().prop_map(Move::OutsiderRemove),
  ]
}

/// A witness as the oracle keeps it: the disk path it was taken at, the fingerprint there,
/// and the bytes there.
#[derive(Clone, Debug)]
struct OracleWitness {
  disk_path: String,
  fingerprint: Fingerprint,
  bytes: Vec<u8>,
}

/// What the agent holds for a name.
#[derive(Clone, Debug)]
struct AgentFile {
  /// The bytes the volume holds itself (content copied up, or created); `None` while the
  /// bytes still sit on the disk behind a metadata-only witness (a renamed base file).
  bytes: Option<Vec<u8>>,
  /// The witness, when the file came from the base.
  witness: Option<OracleWitness>,
  /// Set once the volume has seen the descriptor's inode change in place: sticky until a
  /// rewitness, which these histories never do.
  lost: bool,
}

/// The oracle of (disk, overlay, witnesses) at the root directory.
#[derive(Default, Debug)]
struct Oracle {
  agent: BTreeMap<String, AgentFile>,
  /// Names the agent deleted that the disk held at the time.
  whiteouts: BTreeSet<String>,
}

impl Oracle {
  fn diverged(&self) -> Vec<(String, Divergence)> {
    let mut out: Vec<(String, Divergence)> = self
      .agent
      .iter()
      .map(|(n, f)| {
        (
          format!("/{n}"),
          if f.witness.is_some() {
            Divergence::Witnessed
          } else {
            Divergence::Created
          },
        )
      })
      .chain(
        self
          .whiteouts
          .iter()
          .map(|n| (format!("/{n}"), Divergence::Whiteout)),
      )
      .collect();
    out.sort();
    out
  }

  /// Drift: a witnessed name whose disk fingerprint, at the path the witness was taken, no
  /// longer matches the witness (a replacement with the same bytes is drift: fingerprints are
  /// the truth; identical bytes are the landing verdict's business).
  fn drift(&self, host: &SimHost) -> Vec<String> {
    self
      .agent
      .iter()
      .filter_map(|(n, f)| {
        let w = f.witness.as_ref()?;
        (host.fingerprint(&w.disk_path) != Some(w.fingerprint)).then(|| format!("/{n}"))
      })
      .collect()
  }

  /// What a read of an agent file returns: its own bytes; for bytes still on the disk, the
  /// witnessed bytes through the held descriptor, unless the descriptor's inode was changed
  /// in place, which is `BaseDrift`.
  fn expected_read(&self, host: &SimHost, n: &str) -> Option<Result<Vec<u8>, VfsError>> {
    let f = self.agent.get(n)?;
    if let Some(bytes) = &f.bytes {
      return Some(Ok(bytes.clone()));
    }
    let w = f.witness.as_ref()?;
    Some(if f.lost || Self::modified_in_place(host, w) {
      Err(VfsError::BaseDrift)
    } else {
      Ok(w.bytes.clone())
    })
  }

  /// After the volume's own check (every `status`): a file whose descriptor inode changed in
  /// place is lost from now on.
  fn observe(&mut self, host: &SimHost) {
    for f in self.agent.values_mut() {
      if f.bytes.is_none()
        && let Some(w) = &f.witness
        && Self::modified_in_place(host, w)
      {
        f.lost = true;
      }
    }
  }

  fn modified_in_place(host: &SimHost, w: &OracleWitness) -> bool {
    match host.fingerprint(&w.disk_path) {
      Some(now) => now.ino == w.fingerprint.ino && now != w.fingerprint,
      None => false,
    }
  }
}

proptest! {
  #![proptest_config(ProptestConfig { cases: 150, failure_persistence: None, .. ProptestConfig::default() })]

  /// T-1.10 and AC-1.10: after every step the overlay's diverged set, its witnesses and its
  /// drift list equal the oracle's, and every readable file reads what the oracle says.
  #[test]
  fn the_overlay_matches_the_oracle_under_outsider_edits(
    base in prop::collection::vec((any::<u8>(), prop::collection::vec(any::<u8>(), 1..8)), 0..6),
    history in prop::collection::vec(moves(), 1..30),
  ) {
    let mut host = SimHost::new();
    let mut base_names = BTreeSet::new();
    for (i, bytes) in &base {
      host.replace_file(&format!("/{}", name(*i)), bytes);
      base_names.insert(name(*i).to_owned());
    }
    let mut store = store();
    let mut vol = overlay(&mut host, &mut store);
    let mut oracle = Oracle::default();
    for step in &history {
      host.advance_ns(2);
      apply_move(step, &mut host, &mut store, &mut vol, &mut oracle);
      // The diverged set.
      let real: Vec<(String, Divergence)> = vol.diverged(&store).into_iter().map(|d| (d.path, d.kind)).collect();
      prop_assert_eq!(&real, &oracle.diverged(), "diverged after {:?}", step);
      // Drift.
      let status = vol.with_host(&mut host).status(&mut store).unwrap();
      oracle.observe(&host);
      let mut drift: Vec<String> = status.drift.iter().map(|(p, _)| p.clone()).collect();
      drift.sort();
      let mut expected = oracle.drift(&host);
      expected.sort();
      prop_assert_eq!(&drift, &expected, "drift after {:?}", step);
      // Bytes: the agent's files read what the oracle says; untouched base files read the disk.
      let names: Vec<String> = oracle.agent.keys().cloned().collect();
      for n in names {
        let got = read_all(&mut vol, &mut host, &mut store, &format!("/{n}"));
        prop_assert_eq!(Some(got), oracle.expected_read(&host, &n), "read of {} after {:?}", n, step);
      }
      let disk_files: Vec<String> = host
        .paths()
        .iter()
        .filter(|(_, k)| *k == HostKind::File)
        .map(|(p, _)| p.trim_start_matches('/').to_owned())
        .collect();
      for n in disk_files {
        if oracle.agent.contains_key(&n) || oracle.whiteouts.contains(&n) {
          continue;
        }
        let disk = host.bytes(&format!("/{n}"));
        let got = read_all(&mut vol, &mut host, &mut store, &format!("/{n}")).ok();
        prop_assert_eq!(got, disk, "untouched {} shows the disk after {:?}", n, step);
      }
    }
  }
}

fn apply_move(
  step: &Move,
  host: &mut SimHost,
  store: &mut Store,
  vol: &mut Volume,
  oracle: &mut Oracle,
) {
  match step {
    Move::Write(i, bytes) => agent_write(name(*i), bytes, host, store, vol, oracle),
    Move::Unlink(i) => {
      let n = name(*i);
      let root = vol.root();
      let on_disk = host.bytes(&format!("/{n}")).is_some();
      let r = vol.with_host(host).unlink(store, root, n);
      if r.is_ok() {
        oracle.agent.remove(n);
        if on_disk {
          oracle.whiteouts.insert(n.to_owned());
        }
      }
    }
    Move::Rename(a, b) => agent_rename(name(*a), name(*b), host, store, vol, oracle),
    Move::OutsiderReplace(i, bytes) => host.replace_file(&format!("/{}", name(*i)), bytes),
    Move::OutsiderWrite(i, bytes) => {
      if host.bytes(&format!("/{}", name(*i))).is_some() {
        host.write_in_place(&format!("/{}", name(*i)), bytes, 3);
      }
    }
    Move::OutsiderRemove(i) => host.remove(&format!("/{}", name(*i))),
  }
}

/// The agent writes `bytes` at offset zero of `n`, creating it when neither side has it.
fn agent_write(
  n: &str,
  bytes: &[u8],
  host: &mut SimHost,
  store: &mut Store,
  vol: &mut Volume,
  oracle: &mut Oracle,
) {
  let root = vol.root();
  let disk_now = host.bytes(&format!("/{n}"));
  let fp_now = host.fingerprint(&format!("/{n}"));
  // A file whose bytes are still on the disk behind a metadata witness cannot be pinned once
  // its inode changed in place: the write refuses with BaseDrift and nothing moves.
  let lost = oracle.agent.get(n).is_some_and(|f| {
    f.bytes.is_none()
      && (f.lost
        || f
          .witness
          .as_ref()
          .is_some_and(|w| Oracle::modified_in_place(host, w)))
  });
  let mut o = vol.with_host(host);
  let no = match o.lookup(store, root, n) {
    Ok(l) => l.inode,
    Err(VfsError::NotFound) => o.create_file(store, root, n, 0o644).unwrap(),
    Err(e) => panic!("{e:?}"),
  };
  let r = o.write(store, no, 0, bytes);
  if lost {
    assert_eq!(
      r,
      Err(VfsError::BaseDrift),
      "a lost entry refuses the write"
    );
    if let Some(f) = oracle.agent.get_mut(n) {
      f.lost = true;
    }
    return;
  }
  r.unwrap();
  let entry = oracle.agent.entry(n.to_owned()).or_insert_with(|| {
    // A base file the disk holds (and no whiteout hides) is witnessed at copy-up and starts
    // from the disk's bytes; anything else starts empty.
    let witnessed = disk_now.is_some() && !oracle.whiteouts.contains(n);
    AgentFile {
      bytes: Some(if witnessed {
        disk_now.clone().unwrap_or_default()
      } else {
        Vec::new()
      }),
      witness: witnessed.then(|| OracleWitness {
        disk_path: format!("/{n}"),
        fingerprint: fp_now.unwrap_or_default(),
        bytes: disk_now.clone().unwrap_or_default(),
      }),
      lost: false,
    }
  });
  oracle.whiteouts.remove(n);
  // Bytes still on the disk are pinned at the write: they become the volume's own.
  let current = entry
    .bytes
    .clone()
    .or_else(|| entry.witness.as_ref().map(|w| w.bytes.clone()))
    .unwrap_or_default();
  let mut next = current;
  if next.len() < bytes.len() {
    next.resize(bytes.len(), 0);
  }
  next[..bytes.len()].copy_from_slice(bytes);
  entry.bytes = Some(next);
}

/// The agent renames `from` to `to`: a base file moves with a metadata-only witness at its
/// old disk path and leaves a whiteout there.
fn agent_rename(
  from: &str,
  to: &str,
  host: &mut SimHost,
  store: &mut Store,
  vol: &mut Volume,
  oracle: &mut Oracle,
) {
  if from == to {
    return;
  }
  let root = vol.root();
  let disk_from = host.bytes(&format!("/{from}"));
  let fp_from = host.fingerprint(&format!("/{from}"));
  let r = vol.with_host(host).rename(store, root, from, root, to);
  if r.is_err() {
    return;
  }
  let moved = oracle.agent.remove(from).unwrap_or_else(|| AgentFile {
    bytes: None,
    witness: Some(OracleWitness {
      disk_path: format!("/{from}"),
      fingerprint: fp_from.unwrap_or_default(),
      bytes: disk_from.clone().unwrap_or_default(),
    }),
    lost: false,
  });
  oracle.agent.insert(to.to_owned(), moved);
  oracle.whiteouts.remove(to);
  if disk_from.is_some() {
    oracle.whiteouts.insert(from.to_owned());
  }
}

/// A clone of an overlay snapshot shares the host and the origin's witnesses.
#[test]
fn a_clone_of_an_overlay_snapshot_reads_the_same_base() {
  let mut host = SimHost::new();
  host.replace_file("/f", b"base");
  let mut store = store();
  let mut vol = overlay(&mut host, &mut store);
  let f = vol
    .with_host(&mut host)
    .resolve(&mut store, "/f")
    .unwrap()
    .inode;
  vol
    .with_host(&mut host)
    .write(&mut store, f, 0, b"B")
    .unwrap();
  let s = vol.snapshot(&mut store).unwrap();
  let mut clone = Volume::clone_of(
    &store,
    &mut vol,
    s,
    VolumeConfig {
      prefix: 8,
      names: NameEquivalence::Exact,
      quota: Quota::Bounded { limit: 1 << 30 },
      journal_bytes: 1 << 20,
      clock: Box::new(StepClock::new(0, 1)),
    },
  )
  .unwrap();
  assert!(clone.is_overlay());
  assert!(clone.base_plane().unwrap().is_witnessed(f));
  assert_eq!(
    read_all(&mut clone, &mut host, &mut store, "/f").unwrap(),
    b"Base"
  );
  assert_eq!(clone.diverged(&store).len(), 1);
  let _ = Kind::File;
  let _ = HostKind::File;
}
