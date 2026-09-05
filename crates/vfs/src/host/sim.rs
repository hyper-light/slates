//! A simulated host filesystem for the base plane's oracle tests: an in-memory tree with the
//! fingerprints a real one would report, a clock the test controls, outsider edits (writes in
//! place, replacements, deletions, renames) applied between the volume's steps, and a watcher
//! that can be told to overflow. It is the "disk" leg of the (disk, overlay, witnesses) oracle
//! (Phase 1 task 12, T-1.10 to T-1.13).

use std::collections::BTreeMap;

use super::{
  BaseEntry, Hint, HostDir, HostError, HostFacts, HostFile, HostFs, HostKind, LandCapabilities,
  LandFs, WatchState,
};
use crate::inode::Fingerprint;

/// Format: the device number every simulated file reports.
const SIM_DEV: u64 = 1;
/// Format: the errno a crashed simulated host reports (`EIO`).
const SIM_EIO: i32 = 5;
/// Format: the errno for a missing exchange (`EINVAL`).
const SIM_EINVAL: i32 = 22;
/// Format: the errno the simulated host reports for a name already taken (`EEXIST`, 17 on
/// Linux and macOS).
const SIM_EEXIST: i32 = 17;
/// Format: the `st_mode` of a simulated directory: the `S_IFDIR` type bits over `rwxr-xr-x`,
/// as `stat` reports it (the landing tells a removed directory from a removed file by them).
const DIR_MODE: u32 = 0o040_755;
/// Format: the `st_mode` of a simulated file: `S_IFREG` over `rw-r--r--`.
const FILE_MODE: u32 = 0o100_644;
/// Format: the `st_mode` of a symlink: `S_IFLNK` over `rwxrwxrwx` (unused by every filesystem).
const SYMLINK_MODE: u32 = 0o120_777;

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
/// A temporary made by the landing is a file with no name until it is placed.
#[derive(Clone, Debug)]
enum Open {
  Dir(Vec<Box<str>>),
  File {
    ino: u64,
    snapshot: SimNode,
    /// Where the file was opened: the first place to look for its inode (a rename or a
    /// replacement moves it, and the tree walk then finds it wherever it sits).
    path: Vec<Box<str>>,
  },
  /// An unnamed writable temporary: its bytes live here until `place` links it.
  Temp {
    node: SimNode,
    placed: Option<Vec<Box<str>>>,
  },
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
  /// Write verbs performed so far, for crash injection: the landing crashes when the count
  /// reaches `crash_at`, and every write verb after that refuses.
  write_steps: u64,
  crash_at: Option<u64>,
  crashed: bool,
  /// Whether the simulated filesystem offers an atomic exchange (a mocked `EINVAL` when not).
  exchange_supported: bool,
  /// Whether it offers unnamed temporaries.
  unnamed_temporaries: bool,
  /// Directories synced since the last drain, by handle.
  synced_dirs: Vec<u64>,
  /// Files synced since the last drain.
  synced_files: Vec<u64>,
  /// Every seam call so far, read or write, for proportionality checks (AC-1.14).
  calls: u64,
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
      write_steps: 0,
      crash_at: None,
      crashed: false,
      exchange_supported: true,
      unnamed_temporaries: true,
      synced_dirs: Vec::new(),
      synced_files: Vec::new(),
      calls: 0,
    }
  }

  /// Seam calls so far.
  pub fn calls(&self) -> u64 {
    self.calls
  }

  // ---------------------------------------------------------------- landing controls

  /// Crashes the host at the `n`-th write verb from now (0 is the very next): that verb and
  /// every later one refuse with `EIO`, and the disk keeps whatever the earlier ones did.
  pub fn crash_at_write(&mut self, n: u64) {
    self.write_steps = 0;
    self.crash_at = Some(n);
    self.crashed = false;
  }

  /// Clears a crash: the host serves again (a restart).
  pub fn recover(&mut self) {
    self.crash_at = None;
    self.crashed = false;
    self.write_steps = 0;
  }

  /// Whether the crash point was reached.
  pub fn crashed(&self) -> bool {
    self.crashed
  }

  /// Write verbs performed since the last crash point was set (the writer's instruction count
  /// for T-1.15's "every instruction").
  pub fn write_steps(&self) -> u64 {
    self.write_steps
  }

  /// Takes the atomic exchange away (the mocked `EINVAL` of T-1.16).
  pub fn set_exchange_supported(&mut self, supported: bool) {
    self.exchange_supported = supported;
  }

  /// Takes unnamed temporaries away (a filesystem without `O_TMPFILE`).
  pub fn set_unnamed_temporaries(&mut self, supported: bool) {
    self.unnamed_temporaries = supported;
  }

  /// The directory handles synced since the last call.
  pub fn take_synced_dirs(&mut self) -> Vec<HostDir> {
    std::mem::take(&mut self.synced_dirs)
      .into_iter()
      .map(HostDir)
      .collect()
  }

  /// The file handles synced since the last call.
  pub fn take_synced_files(&mut self) -> Vec<HostFile> {
    std::mem::take(&mut self.synced_files)
      .into_iter()
      .map(HostFile)
      .collect()
  }

  /// One write verb: refuses once the crash point is reached.
  fn write_step(&mut self) -> Result<(), HostError> {
    self.calls += 1;
    if self.crashed {
      return Err(HostError::Unavailable(SIM_EIO));
    }
    if let Some(at) = self.crash_at
      && self.write_steps >= at
    {
      self.crashed = true;
      return Err(HostError::Unavailable(SIM_EIO));
    }
    self.write_steps += 1;
    Ok(())
  }

  fn parent_and_name(dir_parts: &[Box<str>], name: &str) -> (Vec<Box<str>>, Box<str>) {
    (dir_parts.to_vec(), name.into())
  }

  /// The entry `name` under the directory handle, if it exists.
  fn entry_parts(&self, dir: HostDir, name: &str) -> Result<Vec<Box<str>>, HostError> {
    let mut parts = self.dir_parts(dir)?;
    parts.push(name.into());
    Ok(parts)
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
    self.calls += 1;
    self.dir_parts(dir)?;
    Ok(HostFacts {
      timestamp_granularity_ns: self.granularity_ns,
    })
  }

  fn fingerprint_dir(&mut self, dir: HostDir) -> Result<Fingerprint, HostError> {
    self.calls += 1;
    let parts = self.dir_parts(dir)?;
    self
      .node(&parts)
      .map(SimNode::fingerprint)
      .ok_or(HostError::NotFound)
  }

  fn list(&mut self, dir: HostDir) -> Result<Vec<BaseEntry>, HostError> {
    self.calls += 1;
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
    self.calls += 1;
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
    self.calls += 1;
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
        path: parts,
      },
    );
    Ok(HostFile(h))
  }

  fn fstat(&mut self, file: HostFile) -> Result<Fingerprint, HostError> {
    self.calls += 1;
    Ok(self.live_node(file)?.fingerprint())
  }

  fn read_at(&mut self, file: HostFile, off: u64, buf: &mut [u8]) -> Result<usize, HostError> {
    self.calls += 1;
    let node = self.live_node(file)?;
    let off = usize::try_from(off).unwrap_or(usize::MAX);
    if off >= node.bytes.len() {
      return Ok(0);
    }
    let n = buf.len().min(node.bytes.len() - off);
    buf[..n].copy_from_slice(&node.bytes[off..off + n]);
    Ok(n)
  }

  fn read_link(&mut self, dir: HostDir, name: &str) -> Result<Box<str>, HostError> {
    self.calls += 1;
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
  /// What an open descriptor serves: the inode where it was opened if it still sits there,
  /// else wherever the inode moved to, else the retired copy the descriptor keeps alive, else
  /// the snapshot taken at the open.
  fn live_node(&self, file: HostFile) -> Result<SimNode, HostError> {
    let (ino, snapshot, path) = match self.opens.get(&file.0) {
      Some(Open::File {
        ino,
        snapshot,
        path,
      }) => (*ino, snapshot, path),
      _ => return Err(HostError::StaleHandle),
    };
    Ok(
      self
        .node(path)
        .filter(|n| n.ino == ino)
        .cloned()
        .or_else(|| self.sibling_with_ino(path, ino))
        .or_else(|| self.find_ino(ino))
        .or_else(|| self.retired.get(&ino).cloned())
        .unwrap_or_else(|| snapshot.clone()),
    )
  }

  /// The inode under another name in the same directory (an exchange or a rename within the
  /// directory moved it there): the directory's entries, not the whole tree.
  fn sibling_with_ino(&self, path: &[Box<str>], ino: u64) -> Option<SimNode> {
    let parent = path.split_last().map(|(_, p)| p)?;
    self
      .node(parent)?
      .children
      .values()
      .find(|n| n.ino == ino)
      .cloned()
  }

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

impl LandFs for SimHost {
  fn capabilities(&mut self, dir: HostDir) -> Result<LandCapabilities, HostError> {
    self.dir_parts(dir)?;
    Ok(LandCapabilities {
      exchange: self.exchange_supported,
      reflink: false,
      unnamed_temporaries: self.unnamed_temporaries,
    })
  }

  fn create_temp(&mut self, dir: HostDir, name: &str) -> Result<HostFile, HostError> {
    self.write_step()?;
    let parts = self.dir_parts(dir)?;
    let node = self.fresh(HostKind::File);
    let h = self.next_handle;
    self.next_handle += 1;
    if self.unnamed_temporaries {
      self.opens.insert(h, Open::Temp { node, placed: None });
    } else {
      // A hidden sibling name: visible in listings from now on, like a real one.
      let (parent, name) = Self::parent_and_name(&parts, name);
      let ino = node.ino;
      if let Some(p) = self.node_mut(&parent) {
        p.children.insert(name.clone(), node.clone());
      }
      let mut placed = parent;
      placed.push(name);
      self.opens.insert(
        h,
        Open::Temp {
          node: SimNode { ino, ..node },
          placed: Some(placed),
        },
      );
      self.touch_parent(&parts);
    }
    Ok(HostFile(h))
  }

  fn write_at(&mut self, file: HostFile, off: u64, bytes: &[u8]) -> Result<(), HostError> {
    self.write_step()?;
    let now = self.now_ns;
    let off = usize::try_from(off).map_err(|_| HostError::Unavailable(SIM_EINVAL))?;
    let (node_ino, placed) = match self.opens.get_mut(&file.0) {
      Some(Open::Temp { node, placed }) => {
        if node.bytes.len() < off + bytes.len() {
          node.bytes.resize(off + bytes.len(), 0);
        }
        node.bytes[off..off + bytes.len()].copy_from_slice(bytes);
        node.mtime_ns = now;
        node.ctime_ns = now;
        (node.ino, placed.clone())
      }
      Some(Open::File { .. }) => return Err(HostError::Unavailable(SIM_EINVAL)),
      _ => return Err(HostError::StaleHandle),
    };
    // A named temporary's bytes live in the tree too.
    if let Some(parts) = placed
      && let Some(n) = self.node_mut(&parts)
      && n.ino == node_ino
    {
      if n.bytes.len() < off + bytes.len() {
        n.bytes.resize(off + bytes.len(), 0);
      }
      n.bytes[off..off + bytes.len()].copy_from_slice(bytes);
      n.mtime_ns = now;
    }
    Ok(())
  }

  fn sync_file(&mut self, file: HostFile) -> Result<(), HostError> {
    self.write_step()?;
    if !self.opens.contains_key(&file.0) {
      return Err(HostError::StaleHandle);
    }
    self.synced_files.push(file.0);
    Ok(())
  }

  fn set_mode(&mut self, file: HostFile, mode: u32) -> Result<(), HostError> {
    self.write_step()?;
    let now = self.now_ns;
    match self.opens.get_mut(&file.0) {
      Some(Open::Temp { node, placed }) => {
        node.mode = mode;
        node.ctime_ns = now;
        let placed = placed.clone();
        let ino = node.ino;
        if let Some(parts) = placed
          && let Some(n) = self.node_mut(&parts)
          && n.ino == ino
        {
          n.mode = mode;
        }
        Ok(())
      }
      Some(Open::File { ino, .. }) => {
        let ino = *ino;
        if let Some(n) = self.node_by_ino_mut(ino) {
          n.mode = mode;
          n.ctime_ns = now;
        }
        Ok(())
      }
      _ => Err(HostError::StaleHandle),
    }
  }

  fn set_mtime(&mut self, file: HostFile, mtime_ns: i64) -> Result<(), HostError> {
    self.write_step()?;
    match self.opens.get_mut(&file.0) {
      Some(Open::Temp { node, placed }) => {
        node.mtime_ns = mtime_ns;
        let placed = placed.clone();
        let ino = node.ino;
        if let Some(parts) = placed
          && let Some(n) = self.node_mut(&parts)
          && n.ino == ino
        {
          n.mtime_ns = mtime_ns;
        }
        Ok(())
      }
      Some(Open::File { ino, .. }) => {
        let ino = *ino;
        if let Some(n) = self.node_by_ino_mut(ino) {
          n.mtime_ns = mtime_ns;
        }
        Ok(())
      }
      _ => Err(HostError::StaleHandle),
    }
  }

  fn place(&mut self, file: HostFile, dir: HostDir, name: &str) -> Result<(), HostError> {
    self.write_step()?;
    let parts = self.dir_parts(dir)?;
    let (node, already) = match self.opens.get(&file.0) {
      Some(Open::Temp { node, placed }) => (node.clone(), placed.clone()),
      _ => return Err(HostError::StaleHandle),
    };
    let mut placed = parts.clone();
    placed.push(name.into());
    if already.as_ref() == Some(&placed) {
      // Placed at the name it was created with: nothing to do.
      return Ok(());
    }
    // A named temporary gains a second link at `name` (the current bytes, same inode); an
    // unnamed one is linked for the first time. An existing name refuses, as `linkat` does.
    let node = match already {
      Some(path) => self.node(&path).cloned().ok_or(HostError::StaleHandle)?,
      None => node,
    };
    if let Some(p) = self.node_mut(&parts) {
      if p.children.contains_key(name) {
        return Err(HostError::Unavailable(SIM_EEXIST));
      }
      p.children.insert(name.into(), node);
    }
    if let Some(Open::Temp { placed: slot, .. }) = self.opens.get_mut(&file.0) {
      *slot = Some(placed);
    }
    self.touch_parent(&parts);
    Ok(())
  }

  fn exchange(&mut self, dir: HostDir, a: &str, b: &str) -> Result<(), HostError> {
    self.write_step()?;
    if !self.exchange_supported {
      return Err(HostError::Unavailable(SIM_EINVAL));
    }
    let parts = self.dir_parts(dir)?;
    let Some(p) = self.node_mut(&parts) else {
      return Err(HostError::NotFound);
    };
    let (Some(na), Some(nb)) = (p.children.remove(a), p.children.remove(b)) else {
      return Err(HostError::NotFound);
    };
    p.children.insert(a.into(), nb);
    p.children.insert(b.into(), na);
    self.touch_parent(&parts);
    Ok(())
  }

  fn rename(
    &mut self,
    dir: HostDir,
    from: &str,
    to_dir: HostDir,
    to: &str,
  ) -> Result<(), HostError> {
    self.write_step()?;
    let f = self.entry_parts(dir, from)?;
    let t = self.entry_parts(to_dir, to)?;
    let from_path = format!("/{}", f.join("/"));
    let to_path = format!("/{}", t.join("/"));
    if self.node(&f).is_none() {
      return Err(HostError::NotFound);
    }
    SimHost::rename(self, &from_path, &to_path);
    Ok(())
  }

  fn unlink(&mut self, dir: HostDir, name: &str) -> Result<(), HostError> {
    self.write_step()?;
    let parts = self.entry_parts(dir, name)?;
    match self.node(&parts) {
      None => return Err(HostError::NotFound),
      Some(n) if n.kind == HostKind::Dir => return Err(HostError::NotFile),
      Some(_) => {}
    }
    let path = format!("/{}", parts.join("/"));
    self.remove(&path);
    Ok(())
  }

  fn mkdir(&mut self, dir: HostDir, name: &str, mode: u32) -> Result<(), HostError> {
    self.write_step()?;
    let parts = self.entry_parts(dir, name)?;
    if self.node(&parts).is_some() {
      return Err(HostError::Unavailable(SIM_EEXIST));
    }
    let path = format!("/{}", parts.join("/"));
    SimHost::mkdir(self, &path);
    if let Some(n) = self.node_mut(&parts) {
      n.mode = mode;
    }
    Ok(())
  }

  fn rmdir(&mut self, dir: HostDir, name: &str) -> Result<(), HostError> {
    self.write_step()?;
    let parts = self.entry_parts(dir, name)?;
    match self.node(&parts) {
      None => return Err(HostError::NotFound),
      Some(n) if n.kind != HostKind::Dir => return Err(HostError::NotDirectory),
      Some(n) if !n.children.is_empty() => return Err(HostError::Unavailable(SIM_EINVAL)),
      Some(_) => {}
    }
    let path = format!("/{}", parts.join("/"));
    self.remove(&path);
    Ok(())
  }

  fn symlink(&mut self, dir: HostDir, name: &str, target: &str) -> Result<(), HostError> {
    self.write_step()?;
    let parts = self.entry_parts(dir, name)?;
    if self.node(&parts).is_some() {
      return Err(HostError::Unavailable(SIM_EEXIST));
    }
    let path = format!("/{}", parts.join("/"));
    SimHost::symlink(self, &path, target);
    Ok(())
  }

  fn sync_media(&mut self, dir: HostDir) -> Result<(), HostError> {
    self.write_step()?;
    self.dir_parts(dir)?;
    self.synced_dirs.push(dir.0);
    Ok(())
  }

  fn sync_dir(&mut self, dir: HostDir) -> Result<(), HostError> {
    self.write_step()?;
    self.dir_parts(dir)?;
    self.synced_dirs.push(dir.0);
    Ok(())
  }
}

impl SimHost {
  /// The live node with inode `ino`, mutably.
  fn node_by_ino_mut(&mut self, ino: u64) -> Option<&mut SimNode> {
    fn find(node: &mut SimNode, ino: u64) -> Option<&mut SimNode> {
      if node.ino == ino {
        return Some(node);
      }
      for child in node.children.values_mut() {
        if let Some(found) = find(child, ino) {
          return Some(found);
        }
      }
      None
    }
    find(&mut self.root, ino)
  }
}
