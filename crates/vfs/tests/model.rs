//! The executable model (Phase 1 task 8; AC-1.1, AC-1.7): a map with POSIX rules that is the
//! specification of the volume core, and a proptest state machine that drives the real volume and
//! the model through the same generated histories, comparing every refusal, every listing, every
//! byte read, every inode number and the exact accounting after every step, with shrinking.

// Test harness code: an unwrap here is a failed test, which is what it should be. proptest's
// strategy macros expand to `Arc`-carrying unions; the test harness exception of D-8 covers it.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::disallowed_types)]

use std::collections::BTreeMap;

use proptest::prelude::*;
use slates_vfs::dir::Child;
use slates_vfs::error::VfsError;
use slates_vfs::ids::{InodeNo, SnapshotId};
use slates_vfs::inode::Kind;
use slates_vfs::names::NameEquivalence;

mod common;
use common::steps::{Step, step};
use common::{clone_config, store, volume};
use slates_vfs::volume::{Store, Volume};

/// Directory listings by path and file bytes by inode counter.
type AbstractState = (Vec<(String, Vec<String>)>, BTreeMap<u64, Vec<u8>>);

// ------------------------------------------------------------------ the model

#[derive(Clone, Debug, PartialEq, Eq)]
enum Node {
  Dir(BTreeMap<String, Node>),
  File { ino: u64 },
  Symlink { ino: u64, target: String },
}

/// A file's bytes and what the chunk rule of §4.5 says is materialized: inline content is
/// charged byte for byte until a write ends past the inline threshold; after that every chunk
/// window touched by a write is charged from the window's start to the furthest write end in it,
/// and extending by truncate charges nothing (a hole). Every materialized piece carries the
/// epoch it was born in: a window is reborn when a write touches it after a snapshot (the
/// copy-on-write reopen), inline content whenever the inode record is copied.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct ModelFile {
  bytes: Vec<u8>,
  inline: bool,
  /// The inline bytes actually held (a truncate past them leaves a hole, charged nothing).
  inline_len: u64,
  /// The epoch the inode record (and so its inline bytes) was last copied in.
  inline_born: u64,
  /// Window index → (materialized bytes from the window's start, birth epoch).
  windows: BTreeMap<u64, (u64, u64)>,
}

impl ModelFile {
  fn materialized(&self) -> u64 {
    if self.inline {
      self.inline_len
    } else {
      self.windows.values().map(|(m, _)| m).sum()
    }
  }

  /// The bytes born after `last_snapshot`: what dropping the head would free.
  fn unique(&self, last_snapshot: Option<u64>) -> u64 {
    let fresh = |born: u64| last_snapshot.is_none_or(|s| born > s);
    if self.inline {
      if fresh(self.inline_born) {
        self.inline_len
      } else {
        0
      }
    } else {
      self
        .windows
        .values()
        .filter(|(_, born)| fresh(*born))
        .map(|(m, _)| m)
        .sum()
    }
  }

  /// The windows after a write of `[off, end)` under the chunk rule, or `None` when the file
  /// stays inline.
  fn windows_after(
    &self,
    off: u64,
    end: u64,
    chunk: u64,
    inline: u64,
    epoch: u64,
  ) -> Option<BTreeMap<u64, (u64, u64)>> {
    if self.inline && end <= inline {
      return None;
    }
    let mut after = self.windows.clone();
    if self.inline {
      after.insert(0, (self.inline_len, epoch));
    }
    let mut cursor = off;
    while cursor < end {
      let window = cursor / chunk;
      let write_end = end.min((window + 1) * chunk);
      let entry = after.entry(window).or_insert((0, epoch));
      *entry = (entry.0.max(write_end - window * chunk), epoch);
      cursor = write_end;
    }
    Some(after)
  }
}

#[derive(Clone, Debug, Default)]
struct Model {
  root: BTreeMap<String, Node>,
  /// Inode contents by number (hard links share).
  files: BTreeMap<u64, ModelFile>,
  links: BTreeMap<u64, u32>,
  next_ino: u64,
  quota: u64,
  chunk: u64,
  inline: u64,
  policy: NameEquivalence,
  /// The head epoch: advanced by every snapshot.
  epoch: u64,
  last_snapshot: Option<u64>,
}

impl Model {
  fn new(quota: u64, chunk: u64, inline: u64) -> Self {
    Self {
      quota,
      chunk,
      inline,
      next_ino: 2,
      policy: NameEquivalence::Fold,
      ..Default::default()
    }
  }

  fn referenced(&self) -> u64 {
    self.files.values().map(ModelFile::materialized).sum()
  }

  fn unique(&self) -> u64 {
    self
      .files
      .values()
      .map(|f| f.unique(self.last_snapshot))
      .sum()
  }

  fn snapshot(&mut self) {
    self.last_snapshot = Some(self.epoch);
    self.epoch += 1;
  }

  /// The directory at `path`: a missing component is `ENOENT`, one that is not a directory
  /// is `ENOTDIR`, as POSIX resolves paths.
  fn dir_mut(&mut self, path: &[String]) -> Result<&mut BTreeMap<String, Node>, VfsError> {
    let mut cur = &mut self.root;
    for p in path {
      let key = cur
        .keys()
        .find(|k| self.policy.same(k, p))
        .cloned()
        .ok_or(VfsError::NotFound)?;
      match cur.get_mut(&key) {
        Some(Node::Dir(d)) => cur = d,
        _ => return Err(VfsError::NotDirectory),
      }
    }
    Ok(cur)
  }

  fn dir(&self, path: &[String]) -> Result<&BTreeMap<String, Node>, VfsError> {
    let mut cur = &self.root;
    for p in path {
      let key = cur
        .keys()
        .find(|k| self.policy.same(k, p))
        .ok_or(VfsError::NotFound)?;
      match cur.get(key) {
        Some(Node::Dir(d)) => cur = d,
        _ => return Err(VfsError::NotDirectory),
      }
    }
    Ok(cur)
  }

  fn find_key(dir: &BTreeMap<String, Node>, policy: NameEquivalence, name: &str) -> Option<String> {
    dir.keys().find(|k| policy.same(k, name)).cloned()
  }

  fn create(&mut self, dir: &[String], name: &str) -> Result<u64, VfsError> {
    let policy = self.policy;
    let ino = self.next_ino;
    let d = self.dir_mut(dir)?;
    if Self::find_key(d, policy, name).is_some() {
      return Err(VfsError::AlreadyExists);
    }
    d.insert(name.to_owned(), Node::File { ino });
    self.next_ino += 1;
    self.files.insert(
      ino,
      ModelFile {
        inline: true,
        ..Default::default()
      },
    );
    self.links.insert(ino, 1);
    Ok(ino)
  }

  fn symlink(&mut self, dir: &[String], name: &str, target: &str) -> Result<(), VfsError> {
    let policy = self.policy;
    let ino = self.next_ino;
    let d = self.dir_mut(dir)?;
    if Self::find_key(d, policy, name).is_some() {
      return Err(VfsError::AlreadyExists);
    }
    d.insert(
      name.to_owned(),
      Node::Symlink {
        ino,
        target: target.to_owned(),
      },
    );
    self.next_ino += 1;
    self.links.insert(ino, 1);
    Ok(())
  }

  fn mkdir(&mut self, dir: &[String], name: &str) -> Result<(), VfsError> {
    let policy = self.policy;
    let d = self.dir_mut(dir)?;
    if Self::find_key(d, policy, name).is_some() {
      return Err(VfsError::AlreadyExists);
    }
    d.insert(name.to_owned(), Node::Dir(BTreeMap::new()));
    self.next_ino += 1;
    Ok(())
  }

  fn unlink(&mut self, dir: &[String], name: &str) -> Result<(), VfsError> {
    let policy = self.policy;
    let d = self.dir_mut(dir)?;
    let key = Self::find_key(d, policy, name).ok_or(VfsError::NotFound)?;
    let ino = match d.get(&key) {
      Some(Node::Dir(_)) => return Err(VfsError::IsDirectory),
      Some(Node::File { ino }) | Some(Node::Symlink { ino, .. }) => *ino,
      None => return Err(VfsError::NotFound),
    };
    d.remove(&key);
    let links = self.links.entry(ino).or_insert(1);
    *links -= 1;
    if *links == 0 {
      self.files.remove(&ino);
      self.links.remove(&ino);
    } else if let Some(f) = self.files.get_mut(&ino) {
      // The surviving record is copied for its link count.
      f.inline_born = self.epoch;
    }
    Ok(())
  }

  fn rmdir(&mut self, dir: &[String], name: &str) -> Result<(), VfsError> {
    let policy = self.policy;
    let d = self.dir_mut(dir)?;
    let key = Self::find_key(d, policy, name).ok_or(VfsError::NotFound)?;
    match d.get(&key) {
      Some(Node::Dir(inner)) if inner.is_empty() => {
        d.remove(&key);
        Ok(())
      }
      Some(Node::Dir(_)) => Err(VfsError::NotEmpty),
      Some(_) => Err(VfsError::NotDirectory),
      None => Err(VfsError::NotFound),
    }
  }

  fn write(&mut self, ino: u64, off: usize, bytes: &[u8]) -> Result<(), VfsError> {
    let file = self.files.get(&ino).ok_or(VfsError::NotFound)?;
    if bytes.is_empty() {
      // POSIX: a zero-length write changes nothing, not even the size.
      return Ok(());
    }
    let end = off + bytes.len();
    let (off64, end64) = (u64::try_from(off).unwrap(), u64::try_from(end).unwrap());
    let epoch = self.epoch;
    let windows = file.windows_after(off64, end64, self.chunk, self.inline, epoch);
    let after = windows
      .as_ref()
      .map_or(end64.max(file.materialized()), |w| {
        w.values().map(|(m, _)| m).sum()
      });
    let added = after.saturating_sub(file.materialized());
    if self.referenced() + added > self.quota {
      return Err(VfsError::NoSpace);
    }
    let file = self.files.get_mut(&ino).unwrap();
    match windows {
      Some(w) => {
        file.windows = w;
        file.inline = false;
      }
      None => {
        file.inline_len = file.inline_len.max(end64);
        file.inline_born = epoch;
      }
    }
    if file.bytes.len() < end {
      file.bytes.resize(end, 0);
    }
    file.bytes[off..end].copy_from_slice(bytes);
    Ok(())
  }

  /// An SDK edit: the volume truncates at `at`, writes the new bytes, then the old tail; the
  /// model composes the same three rules (T-1.18's model side).
  fn edit(&mut self, ino: u64, at: usize, delete_len: usize, bytes: &[u8]) -> Result<(), VfsError> {
    let file = self.files.get(&ino).ok_or(VfsError::NotFound)?;
    let size = file.bytes.len();
    if at > size {
      return Err(VfsError::Invalid);
    }
    let delete_len = delete_len.min(size - at);
    let tail = file.bytes[at + delete_len..].to_vec();
    // The quota is checked once, on the final length, before anything changes.
    let end = u64::try_from(at + bytes.len() + tail.len()).unwrap();
    let epoch = self.epoch;
    let probe = file.windows_after(
      u64::try_from(at).unwrap(),
      end,
      self.chunk,
      self.inline,
      epoch,
    );
    let after = probe.as_ref().map_or(end.max(file.materialized()), |w| {
      w.values().map(|(m, _)| m).sum()
    });
    if self.referenced() + after.saturating_sub(file.materialized()) > self.quota {
      return Err(VfsError::NoSpace);
    }
    self.truncate(ino, at)?;
    if let Some(f) = self.files.get_mut(&ino) {
      f.inline_born = epoch;
    }
    if !bytes.is_empty() {
      self.write(ino, at, bytes)?;
    }
    if !tail.is_empty() {
      self.write(ino, at + bytes.len(), &tail)?;
    }
    Ok(())
  }

  fn truncate(&mut self, ino: u64, len: usize) -> Result<(), VfsError> {
    let chunk = self.chunk;
    let epoch = self.epoch;
    let file = self.files.get_mut(&ino).ok_or(VfsError::NotFound)?;
    file.bytes.resize(len, 0);
    let len64 = u64::try_from(len).unwrap();
    file.inline_len = file.inline_len.min(len64);
    // The inode record is copied on every truncate, so its inline bytes are reborn.
    file.inline_born = epoch;
    file.windows.retain(|w, _| w * chunk < len64);
    if let Some((w, (m, _))) = file.windows.iter_mut().next_back() {
      *m = (*m).min(len64 - w * chunk);
    }
    Ok(())
  }

  fn rename(
    &mut self,
    from_dir: &[String],
    from: &str,
    to_dir: &[String],
    to: &str,
  ) -> Result<(), VfsError> {
    let policy = self.policy;
    // Both parent paths resolve before the source name is checked, as `renameat2` does.
    let fd = self.dir(from_dir)?;
    let td = self.dir(to_dir)?;
    let from_key = Self::find_key(fd, policy, from).ok_or(VfsError::NotFound)?;
    let node = fd.get(&from_key).cloned().unwrap();
    let to_key = Self::find_key(td, policy, to);
    if from_dir == to_dir && policy.same(from, to) {
      return Ok(());
    }
    let target = to_key.as_ref().and_then(|k| td.get(k)).cloned();
    if let (Some(Node::File { ino: a }), Node::File { ino: b }) = (&target, &node)
      && a == b
    {
      return Ok(());
    }
    if let Node::Dir(_) = node {
      // Moving into its own subtree: the target path starts with the source path.
      let mut source_path = from_dir.to_vec();
      source_path.push(from_key.clone());
      if to_dir.len() >= source_path.len()
        && to_dir
          .iter()
          .zip(&source_path)
          .all(|(a, b)| policy.same(a, b))
      {
        return Err(VfsError::Invalid);
      }
      match &target {
        None => {}
        Some(Node::Dir(inner)) if inner.is_empty() => {}
        Some(Node::Dir(_)) => return Err(VfsError::NotEmpty),
        Some(_) => return Err(VfsError::NotDirectory),
      }
    } else if matches!(target, Some(Node::Dir(_))) {
      return Err(VfsError::IsDirectory);
    }
    if let Some(Node::File { ino }) | Some(Node::Symlink { ino, .. }) = &target {
      let ino = *ino;
      let links = self.links.entry(ino).or_insert(1);
      *links -= 1;
      if *links == 0 {
        self.files.remove(&ino);
        self.links.remove(&ino);
      }
    }
    let fd = self.dir_mut(from_dir).unwrap();
    let node = fd.remove(&from_key).unwrap();
    let moved_ino = match &node {
      Node::File { ino } | Node::Symlink { ino, .. } => Some(*ino),
      Node::Dir(_) => None,
    };
    let td = self.dir_mut(to_dir).unwrap();
    if let Some(k) = to_key {
      td.remove(&k);
    }
    td.insert(to.to_owned(), node);
    // The moved file's record is copied (its home follows it), so its inline bytes are reborn.
    if let Some(ino) = moved_ino
      && let Some(f) = self.files.get_mut(&ino)
    {
      f.inline_born = self.epoch;
    }
    Ok(())
  }

  fn link(&mut self, dir: &[String], name: &str, ino: u64) -> Result<(), VfsError> {
    let policy = self.policy;
    if !self.files.contains_key(&ino) {
      return Err(VfsError::NotFound);
    }
    let d = self.dir_mut(dir)?;
    if Self::find_key(d, policy, name).is_some() {
      return Err(VfsError::AlreadyExists);
    }
    d.insert(name.to_owned(), Node::File { ino });
    *self.links.entry(ino).or_insert(1) += 1;
    if let Some(f) = self.files.get_mut(&ino) {
      f.inline_born = self.epoch;
    }
    Ok(())
  }

  /// The listing of every directory as (path, sorted names) and every file's bytes.
  fn abstract_state(&self) -> AbstractState {
    let mut dirs = Vec::new();
    fn walk(prefix: &str, d: &BTreeMap<String, Node>, out: &mut Vec<(String, Vec<String>)>) {
      let mut names: Vec<String> = d.keys().cloned().collect();
      names.sort();
      out.push((prefix.to_owned(), names));
      for (k, v) in d {
        if let Node::Dir(inner) = v {
          walk(&format!("{prefix}/{k}"), inner, out);
        }
      }
    }
    walk("", &self.root, &mut dirs);
    dirs.sort();
    (
      dirs,
      self
        .files
        .iter()
        .map(|(k, f)| (*k, f.bytes.clone()))
        .collect(),
    )
  }
}

// ------------------------------------------------------------------ the driver

/// The real volume's view: directory listings and file bytes by inode counter.
fn real_state(vol: &Volume, store: &Store) -> AbstractState {
  let mut dirs = Vec::new();
  let mut files = BTreeMap::new();
  let mut stack = vec![(String::new(), vol.root())];
  while let Some((prefix, dir)) = stack.pop() {
    let rows = vol.readdir(store, dir).unwrap();
    let mut names: Vec<String> = rows.iter().map(|r| r.name.to_string()).collect();
    names.sort();
    dirs.push((prefix.clone(), names));
    for row in rows {
      match row.kind {
        Kind::Dir => {
          let located = vol.lookup(store, dir, row.name).unwrap();
          if let Child::Dir(h) = located.child {
            stack.push((format!("{prefix}/{}", row.name), h));
          }
        }
        Kind::File => {
          files.insert(row.inode.counter(), read_all(vol, store, row.inode));
        }
        Kind::Symlink => {}
      }
    }
  }
  dirs.sort();
  (dirs, files)
}

fn read_all(vol: &Volume, store: &Store, ino: InodeNo) -> Vec<u8> {
  let size = vol.stat(store, ino).unwrap().size;
  let mut buf = vec![0u8; usize::try_from(size).unwrap()];
  let n = vol.read(store, ino, 0, &mut buf).unwrap();
  buf.truncate(n);
  buf
}

fn ino_of(model: &Model, pick: u8) -> Option<u64> {
  let inos: Vec<u64> = model.files.keys().copied().collect();
  if inos.is_empty() {
    None
  } else {
    Some(inos[usize::from(pick) % inos.len()])
  }
}

/// The directory at `path`, or the refusal path resolution gives: `ENOENT` for a missing
/// component, `ENOTDIR` for one that is not a directory.
fn dir_handle(
  vol: &Volume,
  store: &Store,
  path: &[String],
) -> Result<slates_mem::Handle<slates_vfs::dir::DirNode>, VfsError> {
  let mut dir = vol.root();
  for p in path {
    match vol.lookup(store, dir, p)?.child {
      Child::Dir(h) => dir = h,
      _ => return Err(VfsError::NotDirectory),
    }
  }
  Ok(dir)
}

/// The real and the expected outcome of one step.
type Outcomes = (Result<(), VfsError>, Result<(), VfsError>);

/// One step against both the volume and the model.
fn apply(step: &Step, vol: &mut Volume, store: &mut Store, model: &mut Model) -> Option<Outcomes> {
  match step {
    Step::Link(..) | Step::Write(..) | Step::Truncate(..) | Step::Edit(..) | Step::Snapshot => {
      apply_content(step, vol, store, model)
    }
    _ => Some(apply_names(step, vol, store, model)),
  }
}

fn apply_names(step: &Step, vol: &mut Volume, store: &mut Store, model: &mut Model) -> Outcomes {
  match step {
    Step::Create(p, n) => (
      with_dir(vol, store, p, |v, s, d| {
        v.create_file(s, d, n, 0o644).map(|_| ())
      }),
      model.create(p, n).map(|_| ()),
    ),
    Step::Mkdir(p, n) => (
      with_dir(vol, store, p, |v, s, d| v.mkdir(s, d, n, 0o755).map(|_| ())),
      model.mkdir(p, n),
    ),
    Step::Symlink(p, n) => (
      with_dir(vol, store, p, |v, s, d| {
        v.symlink(s, d, n, "target").map(|_| ())
      }),
      model.symlink(p, n, "target"),
    ),
    Step::Unlink(p, n) => (
      with_dir(vol, store, p, |v, s, d| v.unlink(s, d, n)),
      model.unlink(p, n),
    ),
    Step::Rmdir(p, n) => (
      with_dir(vol, store, p, |v, s, d| v.rmdir(s, d, n)),
      model.rmdir(p, n),
    ),
    Step::Rename(fp, fnm, tp, tn) => (
      match (dir_handle(vol, store, fp), dir_handle(vol, store, tp)) {
        (Ok(f), Ok(t)) => vol.rename(store, f, fnm, t, tn),
        (Err(e), _) | (_, Err(e)) => Err(e),
      },
      model.rename(fp, fnm, tp, tn),
    ),
    _ => (Ok(()), Ok(())),
  }
}

fn apply_content(
  step: &Step,
  vol: &mut Volume,
  store: &mut Store,
  model: &mut Model,
) -> Option<Outcomes> {
  Some(match step {
    Step::Link(p, n, pick) => {
      let ino = ino_of(model, *pick)?;
      (
        with_dir(vol, store, p, |v, s, d| {
          v.link(s, d, n, InodeNo::compose(7, ino))
        }),
        model.link(p, n, ino),
      )
    }
    Step::Write(pick, off, bytes) => {
      let ino = ino_of(model, *pick)?;
      (
        vol
          .write(store, InodeNo::compose(7, ino), u64::from(*off), bytes)
          .map(|_| ()),
        model.write(ino, usize::from(*off), bytes),
      )
    }
    Step::Truncate(pick, len) => {
      let ino = ino_of(model, *pick)?;
      (
        vol.truncate(store, InodeNo::compose(7, ino), u64::from(*len)),
        model.truncate(ino, usize::from(*len)),
      )
    }
    Step::Edit(pick, at, del, bytes) => {
      let ino = ino_of(model, *pick)?;
      // The offset is clamped to the size: an edit past the end is a refusal both sides agree
      // on, and the interesting histories are the ones inside the file.
      let size = model.files.get(&ino).map_or(0, |f| f.bytes.len());
      let at = usize::from(*at) % (size + 1);
      (
        vol.edit(
          store,
          InodeNo::compose(7, ino),
          u64::try_from(at).unwrap(),
          u64::from(*del),
          bytes,
        ),
        model.edit(ino, at, usize::from(*del), bytes),
      )
    }
    Step::Snapshot => {
      vol.snapshot(store).unwrap();
      model.snapshot();
      (Ok(()), Ok(()))
    }
    _ => (Ok(()), Ok(())),
  })
}

fn with_dir(
  vol: &mut Volume,
  store: &mut Store,
  path: &[String],
  op: impl FnOnce(
    &mut Volume,
    &mut Store,
    slates_mem::Handle<slates_vfs::dir::DirNode>,
  ) -> Result<(), VfsError>,
) -> Result<(), VfsError> {
  let dir = dir_handle(vol, store, path)?;
  op(vol, store, dir)
}

fn run(steps: Vec<Step>, quota: u64) {
  let mut store = store();
  let mut vol = volume(&mut store, quota);
  let mut model = Model::new(
    quota,
    u64::try_from(store.content.chunk_bytes()).unwrap(),
    u64::try_from(store.inline_bytes).unwrap(),
  );
  for step in steps {
    let Some((real, expected)) = apply(&step, &mut vol, &mut store, &mut model) else {
      continue;
    };
    assert_eq!(real, expected, "step {step:?}");
    let (real_dirs, real_files) = real_state(&vol, &store);
    let (model_dirs, model_files) = model.abstract_state();
    assert_eq!(real_dirs, model_dirs, "listings after {step:?}");
    assert_eq!(real_files, model_files, "bytes after {step:?}");
    assert_eq!(
      vol.accounting().referenced_bytes,
      model.referenced(),
      "referenced after {step:?}"
    );
    assert_eq!(
      vol.accounting().unique_bytes,
      model.unique(),
      "unique after {step:?}"
    );
  }
}

proptest! {
  #![proptest_config(ProptestConfig { cases: 400, max_shrink_iters: 4000, failure_persistence: None, .. ProptestConfig::default() })]

  #[test]
  fn the_volume_equals_the_model_on_every_history(steps in prop::collection::vec(step(), 1..40)) {
    run(steps, 1 << 20);
  }

  #[test]
  fn a_tight_quota_refuses_the_same_writes_as_the_model(steps in prop::collection::vec(step(), 1..30)) {
    run(steps, 150);
  }
}

/// AC-1.1: the model-based suite over 10^6 generated operations with shrinking on. CI runs it
/// as `cargo test -p slates-vfs --release --test model -- --ignored ac_1_1`; the count is
/// exact because every applied step is counted, not estimated from the case count.
#[test]
#[ignore = "AC-1.1: 10^6 operations; CI runs it with --ignored"]
fn ac_1_1_one_million_generated_operations_agree_with_the_model() {
  use proptest::test_runner::{Config, RngAlgorithm, TestRng, TestRunner};
  /// Format: the design's operation count for AC-1.1.
  const OPERATIONS: u64 = 1_000_000;
  /// Shape: cases per runner; a runner keeps its success count, so each batch gets a fresh
  /// one, seeded from the batch number so a failing batch can be rerun.
  const CASES_PER_BATCH: u32 = 1000;
  let applied = std::cell::Cell::new(0u64);
  let strategy = prop::collection::vec(step(), 1..40);
  let mut batch = 0u64;
  while applied.get() < OPERATIONS {
    let mut seed = [0u8; 32];
    seed[..8].copy_from_slice(&batch.to_le_bytes());
    let mut runner = TestRunner::new_with_rng(
      Config {
        cases: CASES_PER_BATCH,
        max_shrink_iters: 4000,
        // Never write a regression file into the tree (CLAUDE.md §4).
        failure_persistence: None,
        ..Config::default()
      },
      TestRng::from_seed(RngAlgorithm::ChaCha, &seed),
    );
    let result = runner.run(&strategy, |steps| {
      applied.set(applied.get() + u64::try_from(steps.len()).unwrap());
      run(steps, 1 << 20);
      Ok(())
    });
    if let Err(e) = result {
      panic!(
        "AC-1.1 failed in batch {batch} after {} operations: {e}",
        applied.get()
      );
    }
    batch += 1;
  }
  println!(
    "AC-1.1: {} generated operations agreed with the model",
    applied.get()
  );
}

/// A file with two versions: "version one" under snapshot `s1`, "VERSION TWO" at the head.
fn two_versions() -> (Store, Volume, InodeNo, SnapshotId) {
  let mut store = store();
  let mut vol = volume(&mut store, 1 << 24);
  let root = vol.root();
  let f = vol.create_file(&mut store, root, "f", 0o644).unwrap();
  vol.write(&mut store, f, 0, b"version one").unwrap();
  let s1 = vol.snapshot(&mut store).unwrap();
  vol.write(&mut store, f, 0, b"VERSION TWO").unwrap();
  (store, vol, f, s1)
}

/// AC-1.2: a snapshot is O(1) and keeps the old bytes readable while the head moves on.
#[test]
fn a_snapshot_keeps_the_old_bytes_readable_while_the_head_moves_on() {
  let (store, vol, f, s1) = two_versions();
  let mut buf = [0u8; 512];
  let n = vol.read_in(&store, s1, f, 0, &mut buf).unwrap();
  assert_eq!(&buf[..n], b"version one");
  let n = vol.read(&store, f, 0, &mut buf).unwrap();
  assert_eq!(&buf[..n], b"VERSION TWO");
  assert_eq!(vol.accounting().referenced_bytes, 11);
}

/// A clone of `vol` at `snapshot`, folding names, with a 16 MiB bounded quota.
fn clone_at(store: &Store, vol: &mut Volume, snapshot: SnapshotId) -> Volume {
  Volume::clone_of(store, vol, snapshot, clone_config(8)).unwrap()
}

/// Destroys a volume in slices of eight releases until it reports done.
fn destroy_fully(vol: &mut Volume, store: &mut Store) {
  vol.destroy(store).unwrap();
  while vol.destroy_step(store, 8).unwrap() != slates_vfs::volume::DestroyProgress::Done {}
}

/// AC-1.2: a clone reads its snapshot and diverges without touching the origin.
#[test]
fn a_clone_reads_its_snapshot_and_diverges_without_touching_the_origin() {
  let (mut store, mut vol, f, s1) = two_versions();
  let mut buf = [0u8; 512];
  let mut clone = clone_at(&store, &mut vol, s1);
  assert_eq!(clone.accounting().referenced_bytes, 11);
  assert_eq!(clone.accounting().unique_bytes, 0);
  let cf = clone.resolve(&store, "/f").unwrap().inode;
  assert_eq!(cf, f, "inode numbers survive the clone");
  let n = clone.read(&store, cf, 0, &mut buf).unwrap();
  assert_eq!(&buf[..n], b"version one");
  clone.write(&mut store, cf, 0, b"clone").unwrap();
  let n = clone.read(&store, cf, 0, &mut buf).unwrap();
  assert_eq!(&buf[..n], b"cloneon one");
  let n = vol.read(&store, f, 0, &mut buf).unwrap();
  assert_eq!(&buf[..n], b"VERSION TWO", "the origin is untouched");
  let n = vol.read_in(&store, s1, f, 0, &mut buf).unwrap();
  assert_eq!(&buf[..n], b"version one", "the snapshot is untouched");
}

/// AC-1.5: destroying a clone releases only the bytes it wrote; the origin's snapshot still
/// reads and can be destroyed afterwards.
#[test]
fn destroying_a_clone_releases_only_its_own_bytes() {
  let (mut store, mut vol, f, s1) = two_versions();
  let mut buf = [0u8; 512];
  let mut clone = clone_at(&store, &mut vol, s1);
  let cf = clone.resolve(&store, "/f").unwrap().inode;
  // A write past the inline threshold takes content memory of the clone's own.
  let before = store.content.allocated_bytes();
  let big = vec![b'c'; 300];
  clone.write(&mut store, cf, 0, &big).unwrap();
  assert!(
    store.content.allocated_bytes() > before,
    "the clone's write took content memory"
  );
  let n = clone.read(&store, cf, 0, &mut buf).unwrap();
  assert_eq!(&buf[..n], &big[..]);
  destroy_fully(&mut clone, &mut store);
  assert_eq!(
    store.content.allocated_bytes(),
    before,
    "only the clone's bytes went"
  );
  let n = vol.read_in(&store, s1, f, 0, &mut buf).unwrap();
  assert_eq!(
    &buf[..n],
    b"version one",
    "the origin's snapshot still reads"
  );
  assert_eq!(
    vol.destroy_snapshot(&mut store, s1),
    Err(VfsError::Pinned),
    "the clone's pin outlives the clone until its owner releases it"
  );
  vol.unpin(s1).unwrap();
  assert_eq!(vol.destroy_snapshot(&mut store, s1), Ok(()));
}

#[test]
fn posix_rename_rules_and_no_change_on_refusal() {
  let mut store = store();
  let mut vol = volume(&mut store, 1 << 20);
  let root = vol.root();
  let d = vol.mkdir(&mut store, root, "d", 0o755).unwrap();
  let e = vol.mkdir(&mut store, d, "e", 0o755).unwrap();
  vol.create_file(&mut store, root, "f", 0o644).unwrap();
  assert_eq!(
    vol.rename(&mut store, root, "d", e, "x"),
    Err(VfsError::Invalid),
    "into its own subtree"
  );
  assert_eq!(
    vol.rename(&mut store, root, "f", root, "d"),
    Err(VfsError::IsDirectory)
  );
  assert_eq!(
    vol.rename(&mut store, root, "d", root, "f"),
    Err(VfsError::NotDirectory)
  );
  assert_eq!(vol.rename(&mut store, root, "d", root, "missing"), Ok(()));
  assert!(vol.resolve(&store, "/missing/e").is_ok());
  assert!(vol.resolve(&store, "/d").is_err());
  let moved = vol.resolve(&store, "/missing").unwrap().inode;
  assert_eq!(
    vol.link(&mut store, root, "dl", moved),
    Err(VfsError::NotPermitted)
  );
  assert_eq!(
    vol.create_file(&mut store, root, "F", 0o644),
    Err(VfsError::AlreadyExists),
    "folds equal"
  );
  assert_eq!(vol.readdir(&store, root).unwrap().len(), 2);
}

#[test]
fn truncate_to_a_non_page_boundary_then_extend_reads_zeros_in_the_tail() {
  let mut store = store();
  let mut vol = volume(&mut store, 1 << 24);
  let root = vol.root();
  let f = vol.create_file(&mut store, root, "t", 0o644).unwrap();
  let data: Vec<u8> = (0..u8::MAX).cycle().take(9000).collect();
  vol.write(&mut store, f, 0, &data).unwrap();
  vol.truncate(&mut store, f, 5000).unwrap();
  vol.truncate(&mut store, f, 7000).unwrap();
  let mut buf = vec![1u8; 2000];
  assert_eq!(vol.read(&store, f, 5000, &mut buf).unwrap(), 2000);
  assert!(buf.iter().all(|b| *b == 0));
  vol.write(&mut store, f, 6000, b"z").unwrap();
  let mut buf = vec![1u8; 1001];
  assert_eq!(vol.read(&store, f, 5000, &mut buf).unwrap(), 1001);
  assert!(
    buf[..1000].iter().all(|b| *b == 0),
    "the reopened chunk's tail stays zero"
  );
  assert_eq!(buf[1000], b'z');
}
