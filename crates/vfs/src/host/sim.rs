//! A simulated host filesystem for the base plane's oracle tests: an in-memory tree with the
//! fingerprints a real one would report, a clock the test controls, outsider edits (writes in
//! place, replacements, deletions, renames) applied between the volume's steps, and a watcher
//! that can be told to overflow. It is the "disk" leg of the (disk, overlay, witnesses) oracle
//! (Phase 1 task 12, T-1.10 to T-1.13).

use std::collections::BTreeMap;

use super::{
  BaseEntry, Hint, HostDir, HostError, HostFacts, HostFile, HostFs, HostKind, WatchState,
};
use crate::inode::Fingerprint;

/// Format: the device number every simulated file reports.
const SIM_DEV: u64 = 1;
/// Format: POSIX mode bits of a simulated directory (`rwxr-xr-x`).
const DIR_MODE: u32 = 0o755;
/// Format: POSIX mode bits of a simulated file (`rw-r--r--`).
const FILE_MODE: u32 = 0o644;
/// Format: POSIX mode bits of a symlink (`rwxrwxrwx`, unused by every filesystem).
const SYMLINK_MODE: u32 = 0o777;

/// One node of the simulated disk.
#[derive(Clone, Debug)]
struct SimNode {
  kind: HostKind,
  ino: u64,
  mode: u32,
  bytes: Vec<u8>,
  target: Box<str>,
  mtime_ns: i64,
  ctime_ns: i64,
  children: BTreeMap<Box<str>, SimNode>,
}

impl SimNode {
  fn fingerprint(&self) -> Fingerprint {
    Fingerprint {
      dev: SIM_DEV,
      ino: self.ino,
      size: match self.kind {
        HostKind::File => u64::try_from(self.bytes.len()).unwrap_or(u64::MAX),
        HostKind::Symlink => u64::try_from(self.target.len()).unwrap_or(0),
        HostKind::Dir | HostKind::Other => 0,
      },
      mtime_ns: self.mtime_ns,
      ctime_ns: self.ctime_ns,
      mode: self.mode,
    }
  }
}

/// An open handle: the path it was opened at, and for files the inode it pinned (a file
/// replaced on disk keeps serving the old bytes through the old handle, as a descriptor does).
#[derive(Clone, Debug)]
enum Open {
  Dir(Vec<Box<str>>),
  File { ino: u64, snapshot: SimNode },
}

/// The simulated host.
#[derive(Debug)]
pub struct SimHost {
  root: SimNode,
  next_ino: u64,
  now_ns: i64,
  granularity_ns: u64,
  opens: BTreeMap<u64, Open>,
  next_handle: u64,
  watched: Vec<u64>,
  hints: Vec<Hint>,
  watch_state: WatchState,
  /// Files replaced or unlinked while a handle held them: their last bytes, by inode.
  retired: BTreeMap<u64, SimNode>,
}

impl Default for SimHost {
  fn default() -> Self {
    Self::new()
  }
}

impl SimHost {
  /// An empty disk with nanosecond timestamps and a clock at zero.
  pub fn new() -> Self {
    Self {
      root: SimNode {
        kind: HostKind::Dir,
        ino: 1,
        mode: DIR_MODE,
        bytes: Vec::new(),
        target: "".into(),
        mtime_ns: 0,
        ctime_ns: 0,
        children: BTreeMap::new(),
      },
      next_ino: 2,
      now_ns: 0,
      granularity_ns: 1,
      opens: BTreeMap::new(),
      next_handle: 1,
      watched: Vec::new(),
      hints: Vec::new(),
      watch_state: WatchState::Live,
      retired: BTreeMap::new(),
    }
  }

  /// Sets the filesystem's timestamp granularity (one second for HFS+, two for FAT).
  pub fn set_granularity_ns(&mut self, ns: u64) {
    self.granularity_ns = ns.max(1);
  }

  /// Advances the simulated clock.
  pub fn advance_ns(&mut self, ns: i64) {
    self.now_ns += ns;
  }

  /// The simulated clock, monotonic ns.
  pub fn now_ns(&self) -> i64 {
    self.now_ns
  }

  /// The root directory, as the volume opens it.
  pub fn root(&mut self) -> HostDir {
    let h = self.next_handle;
    self.next_handle += 1;
    self.opens.insert(h, Open::Dir(Vec::new()));
    HostDir(h)
  }

  fn split(path: &str) -> Vec<Box<str>> {
    path
      .split('/')
      .filter(|p| !p.is_empty())
      .map(Box::from)
      .collect()
  }

  fn node(&self, parts: &[Box<str>]) -> Option<&SimNode> {
    let mut cur = &self.root;
    for p in parts {
      cur = cur.children.get(p)?;
    }
    Some(cur)
  }

  fn node_mut(&mut self, parts: &[Box<str>]) -> Option<&mut SimNode> {
    let mut cur = &mut self.root;
    for p in parts {
      cur = cur.children.get_mut(p)?;
    }
    Some(cur)
  }

  fn fresh(&mut self, kind: HostKind) -> SimNode {
    let ino = self.next_ino;
    self.next_ino += 1;
    SimNode {
      kind,
      ino,
      mode: if kind == HostKind::Dir {
        DIR_MODE
      } else {
        FILE_MODE
      },
      bytes: Vec::new(),
      target: "".into(),
      mtime_ns: self.now_ns,
      ctime_ns: self.now_ns,
      children: BTreeMap::new(),
    }
  }

  fn touch_parent(&mut self, parts: &[Box<str>]) {
    let now = self.now_ns;
    if let Some(parent) = self.node_mut(parts) {
      parent.mtime_ns = now;
      parent.ctime_ns = now;
    }
    self.hint_for(parts);
  }

  /// A hint for the watched directory at or above `parts`.
  fn hint_for(&mut self, parts: &[Box<str>]) {
    if self.watch_state != WatchState::Live {
      return;
    }
    let watched: Vec<(u64, Vec<Box<str>>)> = self
      .watched
      .iter()
      .filter_map(|h| match self.opens.get(h) {
        Some(Open::Dir(p)) => Some((*h, p.clone())),
        _ => None,
      })
      .collect();
    for (h, p) in watched {
      if p.as_slice() == parts {
        self.hints.push(Hint::Changed(HostDir(h)));
      }
    }
  }

  // ---------------------------------------------------------------- outsider edits

  /// Creates a directory (parents must exist).
  pub fn mkdir(&mut self, path: &str) {
    let parts = Self::split(path);
    let (name, parent) = parts
      .split_last()
      .map(|(n, p)| (n.clone(), p.to_vec()))
      .unwrap_or_default();
    let node = self.fresh(HostKind::Dir);
    if let Some(p) = self.node_mut(&parent) {
      p.children.insert(name, node);
    }
    self.touch_parent(&parent);
  }

  /// Creates or replaces a file with a new inode (what an editor's save-by-rename does).
  pub fn replace_file(&mut self, path: &str, bytes: &[u8]) {
    let parts = Self::split(path);
    let (name, parent) = parts
      .split_last()
      .map(|(n, p)| (n.clone(), p.to_vec()))
      .unwrap_or_default();
    let mut node = self.fresh(HostKind::File);
    node.bytes = bytes.to_vec();
    if let Some(p) = self.node_mut(&parent)
      && let Some(old) = p.children.insert(name, node)
    {
      self.retired.insert(old.ino, old);
    }
    self.touch_parent(&parent);
  }

  /// Overwrites a file in place, same inode, timestamps advanced by `dt_ns` (zero keeps them,
  /// which is the racy case of T-1.11).
  pub fn write_in_place(&mut self, path: &str, bytes: &[u8], dt_ns: i64) {
    let parts = Self::split(path);
    self.now_ns += dt_ns;
    let now = self.now_ns;
    if let Some(n) = self.node_mut(&parts) {
      n.bytes = bytes.to_vec();
      n.mtime_ns = now;
      n.ctime_ns = now;
    }
    let parent = parts[..parts.len().saturating_sub(1)].to_vec();
    self.hint_for(&parent);
  }

  /// Creates a symlink.
  pub fn symlink(&mut self, path: &str, target: &str) {
    let parts = Self::split(path);
    let (name, parent) = parts
      .split_last()
      .map(|(n, p)| (n.clone(), p.to_vec()))
      .unwrap_or_default();
    let mut node = self.fresh(HostKind::Symlink);
    node.target = target.into();
    node.mode = SYMLINK_MODE;
    if let Some(p) = self.node_mut(&parent) {
      p.children.insert(name, node);
    }
    self.touch_parent(&parent);
  }

  /// Removes an entry (a directory with everything beneath).
  pub fn remove(&mut self, path: &str) {
    let parts = Self::split(path);
    let (name, parent) = parts
      .split_last()
      .map(|(n, p)| (n.clone(), p.to_vec()))
      .unwrap_or_default();
    if let Some(p) = self.node_mut(&parent)
      && let Some(old) = p.children.remove(&name)
      && old.kind == HostKind::File
    {
      self.retired.insert(old.ino, old);
    }
    self.touch_parent(&parent);
  }

  /// Renames an entry.
  pub fn rename(&mut self, from: &str, to: &str) {
    let f = Self::split(from);
    let t = Self::split(to);
    let (fname, fparent) = f
      .split_last()
      .map(|(n, p)| (n.clone(), p.to_vec()))
      .unwrap_or_default();
    let (tname, tparent) = t
      .split_last()
      .map(|(n, p)| (n.clone(), p.to_vec()))
      .unwrap_or_default();
    let Some(node) = self
      .node_mut(&fparent)
      .and_then(|p| p.children.remove(&fname))
    else {
      return;
    };
    if let Some(p) = self.node_mut(&tparent)
      && let Some(old) = p.children.insert(tname, node)
    {
      self.retired.insert(old.ino, old);
    }
    self.touch_parent(&fparent);
    self.touch_parent(&tparent);
  }

  /// Changes a file's mode (a metadata-only change).
  pub fn chmod(&mut self, path: &str, mode: u32) {
    let parts = Self::split(path);
    let now = self.now_ns;
    if let Some(n) = self.node_mut(&parts) {
      n.mode = mode;
      n.ctime_ns = now;
    }
  }

  /// The bytes of a file as the disk holds them.
  pub fn bytes(&self, path: &str) -> Option<Vec<u8>> {
    self.node(&Self::split(path)).map(|n| n.bytes.clone())
  }

  /// The fingerprint of an entry as the disk holds it.
  pub fn fingerprint(&self, path: &str) -> Option<Fingerprint> {
    self.node(&Self::split(path)).map(SimNode::fingerprint)
  }

  /// Every path on the disk with its kind, sorted.
  pub fn paths(&self) -> Vec<(String, HostKind)> {
    let mut out = Vec::new();
    let mut stack = vec![(String::new(), &self.root)];
    while let Some((prefix, node)) = stack.pop() {
      for (name, child) in &node.children {
        let path = format!("{prefix}/{name}");
        out.push((path.clone(), child.kind));
        if child.kind == HostKind::Dir {
          stack.push((path, child));
        }
      }
    }
    out.sort();
    out
  }

  /// Makes the watcher lose events from now on until [`SimHost::watcher_recover`]; the next
  /// drain reports the overflow once.
  pub fn watcher_overflow(&mut self) {
    self.watch_state = WatchState::Overflowed;
    self.hints.push(Hint::Overflow);
  }

  /// The watcher delivers again.
  pub fn watcher_recover(&mut self) {
    self.watch_state = WatchState::Live;
  }

  /// Handles currently open (a leak check for the volume).
  pub fn open_handles(&self) -> usize {
    self.opens.len()
  }

  fn dir_parts(&self, dir: HostDir) -> Result<Vec<Box<str>>, HostError> {
    match self.opens.get(&dir.0) {
      Some(Open::Dir(p)) => Ok(p.clone()),
      _ => Err(HostError::StaleHandle),
    }
  }
}

impl HostFs for SimHost {
  fn facts(&mut self, dir: HostDir) -> Result<HostFacts, HostError> {
    self.dir_parts(dir)?;
    Ok(HostFacts {
      timestamp_granularity_ns: self.granularity_ns,
    })
  }

  fn fingerprint_dir(&mut self, dir: HostDir) -> Result<Fingerprint, HostError> {
    let parts = self.dir_parts(dir)?;
    self
      .node(&parts)
      .map(SimNode::fingerprint)
      .ok_or(HostError::NotFound)
  }

  fn list(&mut self, dir: HostDir) -> Result<Vec<BaseEntry>, HostError> {
    let parts = self.dir_parts(dir)?;
    let node = self.node(&parts).ok_or(HostError::NotFound)?;
    if node.kind != HostKind::Dir {
      return Err(HostError::NotDirectory);
    }
    Ok(
      node
        .children
        .iter()
        .map(|(name, child)| BaseEntry {
          name: name.clone(),
          kind: child.kind,
          fingerprint: child.fingerprint(),
        })
        .collect(),
    )
  }

  fn open_dir(&mut self, parent: HostDir, name: &str) -> Result<HostDir, HostError> {
    let mut parts = self.dir_parts(parent)?;
    parts.push(name.into());
    match self.node(&parts) {
      Some(n) if n.kind == HostKind::Dir => {}
      Some(_) => return Err(HostError::NotDirectory),
      None => return Err(HostError::NotFound),
    }
    let h = self.next_handle;
    self.next_handle += 1;
    self.opens.insert(h, Open::Dir(parts));
    Ok(HostDir(h))
  }

  fn open_file(&mut self, dir: HostDir, name: &str) -> Result<HostFile, HostError> {
    let mut parts = self.dir_parts(dir)?;
    parts.push(name.into());
    let node = match self.node(&parts) {
      Some(n) if n.kind == HostKind::File => n.clone(),
      Some(_) => return Err(HostError::NotFile),
      None => return Err(HostError::NotFound),
    };
    let h = self.next_handle;
    self.next_handle += 1;
    self.opens.insert(
      h,
      Open::File {
        ino: node.ino,
        snapshot: node,
      },
    );
    Ok(HostFile(h))
  }

  fn fstat(&mut self, file: HostFile) -> Result<Fingerprint, HostError> {
    let (ino, snapshot) = match self.opens.get(&file.0) {
      Some(Open::File { ino, snapshot }) => (*ino, snapshot.clone()),
      _ => return Err(HostError::StaleHandle),
    };
    // The live inode if it still sits somewhere on the disk, else the retired copy the
    // descriptor keeps alive.
    Ok(
      self
        .find_ino(ino)
        .or_else(|| self.retired.get(&ino).cloned())
        .unwrap_or(snapshot)
        .fingerprint(),
    )
  }

  fn read_at(&mut self, file: HostFile, off: u64, buf: &mut [u8]) -> Result<usize, HostError> {
    let (ino, snapshot) = match self.opens.get(&file.0) {
      Some(Open::File { ino, snapshot }) => (*ino, snapshot.clone()),
      _ => return Err(HostError::StaleHandle),
    };
    let node = self
      .find_ino(ino)
      .or_else(|| self.retired.get(&ino).cloned())
      .unwrap_or(snapshot);
    let off = usize::try_from(off).unwrap_or(usize::MAX);
    if off >= node.bytes.len() {
      return Ok(0);
    }
    let n = buf.len().min(node.bytes.len() - off);
    buf[..n].copy_from_slice(&node.bytes[off..off + n]);
    Ok(n)
  }

  fn read_link(&mut self, dir: HostDir, name: &str) -> Result<Box<str>, HostError> {
    let mut parts = self.dir_parts(dir)?;
    parts.push(name.into());
    match self.node(&parts) {
      Some(n) if n.kind == HostKind::Symlink => Ok(n.target.clone()),
      Some(_) => Err(HostError::NotFile),
      None => Err(HostError::NotFound),
    }
  }

  fn close_file(&mut self, file: HostFile) {
    self.opens.remove(&file.0);
  }

  fn close_dir(&mut self, dir: HostDir) {
    self.opens.remove(&dir.0);
    self.watched.retain(|h| *h != dir.0);
  }

  fn watch(&mut self, dir: HostDir) -> WatchState {
    if self.opens.contains_key(&dir.0) {
      self.watched.push(dir.0);
    }
    self.watch_state
  }

  fn hints(&mut self) -> Vec<Hint> {
    std::mem::take(&mut self.hints)
  }
}

impl SimHost {
  /// The live node with inode `ino`, wherever it sits now (a rename keeps the inode).
  fn find_ino(&self, ino: u64) -> Option<SimNode> {
    let mut stack = vec![&self.root];
    while let Some(node) = stack.pop() {
      if node.ino == ino {
        return Some(node.clone());
      }
      for child in node.children.values() {
        stack.push(child);
      }
    }
    None
  }
}
