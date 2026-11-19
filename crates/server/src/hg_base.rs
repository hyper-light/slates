//! A repository's `.hg` as Mercurial's import reads it, through the base plane's read seam (A-649 part 4; §4.17.3 piece
//! 3, "an existing repository's store imported as a base"; §4.15, a repository directory taken by a read; §4.5's host
//! seam).
//!
//! **What it is.** [`HgBase`] answers `slates_hg::import::RepositoryFiles`, a file's length and then its bytes by
//! ranges, each file named by its path's names under `.hg`, from the host seam ([`HostFs`]): `.hg` is opened under the
//! repository's directory and each name under the one before it, never a path string, so a rename of the repository by
//! its user changes nothing (§4.4), and the seam has no verb that writes (R1). The host itself refuses a name that is
//! not one entry (empty, `.`, `..`, or holding a separator: `slates_base`'s `one_entry`), never follows a link
//! (`O_NOFOLLOW`, `FILE_OPEN_REPARSE_POINT`), and names the fetch a read of an S3 base waits on (A-555).
//!
//! **Absent is not refused.** A name the host does not find is a file that is not there (`None`), which the import
//! reads as 7.2 reads one: `revlog._get_data` reads an absent file as empty, and a store before its first commit has no
//! changelog. Every other refusal is carried through typed ([`BaseRefused::Host`]) where 7.2's `open` raises and it
//! aborts: a directory where a file is named or the reverse, a link, a fetch still pending, the host unavailable. A
//! name that is not UTF-8, which the seam's names are, is refused by kind before the host is asked
//! ([`BaseRefused::NotUtf8`]): only the plain encoding (7.2's `exp-very-fragile-and-unsafe-plain-store-encoding`) names
//! a file by its path's own bytes, since 7.2's every other encoding writes a byte past ASCII as `~` and two hexadecimal
//! digits (`store._buildencodefun`).
//!
//! **A file's length is its length when it was opened.** A file is opened once a lend and its length taken then
//! (`fstat`), and its reads end there, so a revlog another process appends to is read as the prefix it held when it was
//! opened, as 7.2's reader keeps the index length it opened with. A file replaced while held (a save by rename) is read
//! as it was opened, since its descriptor keeps its bytes; a file cut short while held (a strip) ends where it now
//! ends, so a read past that returns fewer bytes and the revlog reader refuses the revlog as truncated.
//!
//! **Bounds.** A read is charged to the import's room before a byte is held, and charged at most what the file holds
//! from its offset, so an index entry pointing a chunk past its data file's end costs nothing. The handles held at once
//! are bounded whatever a path's depth: the last file's directory, and the two files a revlog's reader reads in turn
//! ([`FILES_HELD`]); every other directory is closed as soon as the one under it is open. A path a hostile listing
//! nests deep costs an open a level, never a descriptor held a level, which on the daemon's one descriptor table would
//! refuse other clients' opens. Everything a lend opened is closed when it is dropped, a refusal's path included; the
//! repository's directory is its caller's.
//!
//! **Lent, never held across a wait.** It borrows the host for as long as its caller lends it: one turn of a sliced
//! import (`slates_hg::import::Import::advance`) where the host is the shard's, lent afresh for each turn as a
//! repository's store is lent for each step of an answer (`crate::vcs_http::git::upload`), or the whole import where
//! the import's task owns its host. A turn's lend opens again what the turn before it closed: the files of the revlog
//! being read and the walk to their directory, counted ([`Opened`]).
//!
//! **Owed** (GAPS): the daemon's import of a repository's directory, which holds the host and lends this a turn at a
//! time on the repository's owner shard, and the register's entry of what the import returns.

use std::collections::VecDeque;

use slates_hg::import::{FileRefused, RepositoryFiles};
use slates_hg::{Admission, PastAdmission};
use slates_vfs::host::{HostDir, HostError, HostFile, HostFs};

/// Format: the directory of a repository's Mercurial files, under which the import names every file.
const DOT_HG: &str = ".hg";

/// Format: the files held open at once: a revlog's two, its index and its data file, which its reader reads in turn
/// (an entry, then the chunk the entry points at; `slates_hg::revlog::Source`), so neither is opened again while the
/// revlog is read.
const FILES_HELD: usize = 2;

/// An open file: its handle, and its length when it was opened.
type Open = (HostFile, u64);

/// Why the base did not hand a file over (the module doc's refusals).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum BaseRefused {
  /// The host refused, for its own reason (§4.15's `HostError`): a directory where a file is named or the reverse, a
  /// link, a fetch still pending on an S3 base, the host unavailable.
  Host(HostError),
  /// A name is not UTF-8, which the host seam's names are: only the plain encoding leaves one.
  NotUtf8 {
    /// The name.
    name: Box<[u8]>,
  },
}

/// What a lend opened, and what it answered from a file it held (the module doc's bounds, counted).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Opened {
  /// The directories opened: the walks' cost, which a turn's lend pays again.
  pub(crate) directories: u64,
  /// The files opened.
  pub(crate) files: u64,
  /// The lengths and reads answered from a file already held, opening nothing: the held files' non-vacuity count.
  pub(crate) reused: u64,
}

/// A file held open.
struct Held {
  /// Its names under `.hg`, as the import asked for it.
  names: Vec<Vec<u8>>,
  /// Its handle, and its length when it was opened, where its reads end.
  open: Open,
}

/// A repository's `.hg` read through the host seam, for as long as the host is lent (the module doc).
pub(crate) struct HgBase<'h, H: HostFs + ?Sized> {
  /// The host, lent.
  host: &'h mut H,
  /// The repository's directory: its caller's, never closed here.
  repository: HostDir,
  /// The last file's directory, by its names under the repository's (`.hg` first), held until a file elsewhere is
  /// opened.
  directory: Option<(Vec<Box<str>>, HostDir)>,
  /// The files held, the one read longest ago first; at most [`FILES_HELD`].
  files: VecDeque<Held>,
  /// What this lend opened and reused.
  opened: Opened,
}

impl<'h, H: HostFs + ?Sized> HgBase<'h, H> {
  /// The `.hg` of the repository at `repository`, read through `host` while it is lent.
  pub(crate) fn new(host: &'h mut H, repository: HostDir) -> Self {
    Self {
      host,
      repository,
      directory: None,
      files: VecDeque::new(),
      opened: Opened::default(),
    }
  }

  /// What this lend has opened and reused so far.
  pub(crate) fn opened(&self) -> Opened {
    self.opened
  }

  /// The host, for a test to look at what the lend holds.
  #[cfg(test)]
  fn host(&self) -> &H {
    self.host
  }

  /// The file at `path` under `.hg`: held already, or opened and held now; `None` when no file is there.
  fn held(&mut self, path: &[&[u8]]) -> Result<Option<Open>, BaseRefused> {
    if let Some(open) = self.reuse(path) {
      return Ok(Some(open));
    }
    let names = names_under_hg(path)?;
    let Some(open) = self.open(&names)? else {
      return Ok(None);
    };
    self.hold(path, open);
    Ok(Some(open))
  }

  /// The held file at `path`, made the one read last; counted.
  fn reuse(&mut self, path: &[&[u8]]) -> Option<Open> {
    let found = self
      .files
      .iter()
      .position(|held| same_path(&held.names, path))?;
    let held = self.files.remove(found)?;
    let open = held.open;
    self.files.push_back(held);
    self.opened.reused = self.opened.reused.saturating_add(1);
    Some(open)
  }

  /// Holds the file just opened at `path`, closing the one read longest ago when [`FILES_HELD`] are held.
  fn hold(&mut self, path: &[&[u8]], open: Open) {
    if self.files.len() >= FILES_HELD
      && let Some(oldest) = self.files.pop_front()
    {
      self.host.close_file(oldest.open.0);
    }
    self.files.push_back(Held {
      names: path.iter().map(|name| name.to_vec()).collect(),
      open,
    });
  }

  /// Opens the file at `names` (`.hg` first) and takes its length: under the last file's directory when it is the same,
  /// else at the end of a walk from the repository's; `None` when a name is not there. Counted.
  fn open(&mut self, names: &[&str]) -> Result<Option<Open>, BaseRefused> {
    let Some((name, parents)) = names.split_last() else {
      return Ok(None);
    };
    let Some(directory) = self.directory_at(parents)? else {
      return Ok(None);
    };
    let file = match self.host.open_file(directory, name) {
      Ok(file) => file,
      Err(HostError::NotFound) => return Ok(None),
      Err(refused) => return Err(BaseRefused::Host(refused)),
    };
    self.opened.files = self.opened.files.saturating_add(1);
    match self.host.fstat(file) {
      Ok(fingerprint) => Ok(Some((file, fingerprint.size))),
      Err(refused) => {
        self.host.close_file(file);
        Err(BaseRefused::Host(refused))
      }
    }
  }

  /// The directory at `names` under the repository's: the repository's own for none, the last file's when it is the
  /// same, else walked from the repository's and held in place of the last file's; `None` when a name is not there.
  fn directory_at(&mut self, names: &[&str]) -> Result<Option<HostDir>, BaseRefused> {
    if let Some((held, directory)) = &self.directory
      && held.iter().map(|name| &**name).eq(names.iter().copied())
    {
      return Ok(Some(*directory));
    }
    if let Some((_, directory)) = self.directory.take() {
      self.host.close_dir(directory);
    }
    let Some((first, rest)) = names.split_first() else {
      return Ok(Some(self.repository));
    };
    let Some(directory) = self.walk(first, rest)? else {
      return Ok(None);
    };
    self.directory = Some((names.iter().map(|name| Box::from(*name)).collect(), directory));
    Ok(Some(directory))
  }

  /// Opens `first` under the repository's directory and each of `rest` under the one before it, closing each as soon
  /// as the one under it is open: the last, or `None` when a name is not there.
  fn walk(&mut self, first: &str, rest: &[&str]) -> Result<Option<HostDir>, BaseRefused> {
    let Some(mut at) = self.open_directory(self.repository, first)? else {
      return Ok(None);
    };
    for name in rest {
      let next = self.open_directory(at, name);
      self.host.close_dir(at);
      match next? {
        Some(directory) => at = directory,
        None => return Ok(None),
      }
    }
    Ok(Some(at))
  }

  /// The directory `name` under `parent`, counted; `None` when it is not there.
  fn open_directory(&mut self, parent: HostDir, name: &str) -> Result<Option<HostDir>, BaseRefused> {
    match self.host.open_dir(parent, name) {
      Ok(directory) => {
        self.opened.directories = self.opened.directories.saturating_add(1);
        Ok(Some(directory))
      }
      Err(HostError::NotFound) => Ok(None),
      Err(refused) => Err(BaseRefused::Host(refused)),
    }
  }

  /// Reads `file` from `offset` into `bytes` until they are full or the file ends: how many were read.
  fn fill(&mut self, file: HostFile, offset: u64, bytes: &mut [u8]) -> Result<usize, HostError> {
    let mut read = 0usize;
    while let Some(rest) = bytes.get_mut(read..)
      && !rest.is_empty()
    {
      let at = offset.saturating_add(u64::try_from(read).unwrap_or(u64::MAX));
      match self.host.read_at(file, at, rest)? {
        0 => break,
        got => read = read.saturating_add(got),
      }
    }
    Ok(read)
  }
}

impl<H: HostFs + ?Sized> RepositoryFiles for HgBase<'_, H> {
  type Refused = BaseRefused;

  fn len(&mut self, path: &[&[u8]]) -> Result<Option<u64>, FileRefused<BaseRefused>> {
    let held = self.held(path).map_err(FileRefused::Host)?;
    Ok(held.map(|(_, len)| len))
  }

  fn read(
    &mut self,
    path: &[&[u8]],
    offset: u64,
    len: u64,
    room: &mut Admission,
  ) -> Result<Option<Vec<u8>>, FileRefused<BaseRefused>> {
    let Some((file, file_len)) = self.held(path).map_err(FileRefused::Host)? else {
      return Ok(None);
    };
    let held = offset
      .saturating_add(len)
      .min(file_len)
      .saturating_sub(offset);
    let mut bytes = admitted(held, room)?;
    let read = self
      .fill(file, offset, &mut bytes)
      .map_err(|refused| FileRefused::Host(BaseRefused::Host(refused)))?;
    bytes.truncate(read);
    Ok(Some(bytes))
  }
}

impl<H: HostFs + ?Sized> Drop for HgBase<'_, H> {
  fn drop(&mut self) {
    while let Some(held) = self.files.pop_front() {
      self.host.close_file(held.open.0);
    }
    if let Some((_, directory)) = self.directory.take() {
      self.host.close_dir(directory);
    }
  }
}

/// `len` zeroed bytes, charged to `room` first: refused with nothing charged past the room, or past what this machine
/// can address.
fn admitted(len: u64, room: &mut Admission) -> Result<Vec<u8>, FileRefused<BaseRefused>> {
  let past = PastAdmission {
    asked: len,
    left: room.left(),
  };
  let size = usize::try_from(len).map_err(|_| FileRefused::PastAdmission(past))?;
  room.charge(len).map_err(FileRefused::PastAdmission)?;
  Ok(vec![0; size])
}

/// `path`'s names as the host takes them, `.hg` first; a name that is not UTF-8 refused by kind.
fn names_under_hg<'p>(path: &[&'p [u8]]) -> Result<Vec<&'p str>, BaseRefused> {
  let mut names = Vec::with_capacity(path.len().saturating_add(1));
  names.push(DOT_HG);
  for name in path.iter().copied() {
    let utf8 = core::str::from_utf8(name).map_err(|_| BaseRefused::NotUtf8 {
      name: Box::from(name),
    })?;
    names.push(utf8);
  }
  Ok(names)
}

/// Whether a held file's names are `path`'s.
fn same_path(names: &[Vec<u8>], path: &[&[u8]]) -> bool {
  names.iter().map(Vec::as_slice).eq(path.iter().copied())
}

#[cfg(test)]
mod tests {
  use std::collections::{BTreeMap, BTreeSet};

  use slates_hg::import::{
    self, Clock, FileRefused, Import, ImportRefused, Imported, Progress, RepositoryFiles, Turn,
  };
  use slates_hg::phases::PhaseHead;
  use slates_hg::{Admission, Node, PastAdmission};
  use slates_vcs::ContentStore;
  use slates_vfs::host::sim::{Interference, SimHost, SimVerb};
  use slates_vfs::host::{FetchId, HostDir, HostError, HostFs};

  use super::{BaseRefused, HgBase, Opened};

  /// A repository's files, each by its path under `.hg`, with its bytes.
  type Files = &'static [(&'static str, &'static [u8])];

  /// An index's rows, each its key and its body.
  type Rows = Vec<(Vec<u8>, Vec<u8>)>;

  /// An import's counts: changesets, manifests, file logs, file revisions, phase roots dropped, bookmarks dropped and
  /// passed over, markers, and texts rebuilt from the one before.
  type Counts = (u32, u32, u64, u64, u64, u64, u64, u64, u64);

  /// Each file a fixture repository holds, by its path under `.hg`: `.hg`'s own files from their fixtures, then the
  /// store's, each `store/` and its path in `$store`, the fixture directory 7.2 left the store in.
  macro_rules! repository {
    ($store:literal, [$($hg:literal => $fixture:literal),* $(,)?], [$($file:literal),* $(,)?]) => {
      &[
        $(($hg, include_bytes!(concat!("../../hg/fixtures/", $fixture)).as_slice()),)*
        $((
          concat!("store/", $file),
          include_bytes!(concat!("../../hg/fixtures/", $store, "/", $file)).as_slice(),
        ),)*
      ]
    };
  }

  /// Shape: the `states` repository stock 7.2 kept (`crates/hg/fixtures/state/make-state-fixtures.sh`): drafts, an
  /// amend, a secret changeset, bookmarks and obsolescence markers; its store share-safe, listed by its fncache, and
  /// its changelog split into an index and a data file, which the reader reads in turn.
  const STATES: Files = repository!(
    "state/states",
    [
      "requires" => "state/states-hg-requires.txt",
      "bookmarks" => "state/states-bookmarks",
    ],
    [
      "00changelog.d",
      "00changelog.i",
      "00manifest.i",
      "data/a.i",
      "data/b.i",
      "data/c.i",
      "data/d.i",
      "fncache",
      "obsstore",
      "phaseroots",
      "requires",
    ]
  );

  /// Shape: the history 7.2 stored under its file index (`crates/hg/fixtures/import/make-import-fixtures.sh`): names
  /// under every rule of its encoding (a reserved name, a trailing space and period, `.i` and `.d` directories) and a
  /// hashed path eight directories deep.
  const FILE_INDEX: Files = repository!(
    "import/fileindex",
    ["requires" => "import/fileindex-hg-requires.txt"],
    [
      "00changelog.d",
      "00changelog.i",
      "00manifest.i",
      "data/_capital___under.txt.i",
      "data/_f_o_o.i",
      "data/au~78.txt.i",
      "data/colon~3aquest~3fstar~2a.txt.i",
      "data/co~6d1.i",
      "data/co~6e.i",
      "data/foo.i",
      "data/lp~749.x.i",
      "data/plain.txt.i",
      "data/pr~6e.d.i",
      "data/quote~22lt~3cgt~3epipe~7cback~5cslash.txt.i",
      "data/tilde~7e~c3~a9.txt.i",
      "data/trailing .i",
      "data/trailing..i",
      "data/x.i.hg/inside.txt.i",
      "data/y.d.hg/inside.txt.i",
      "data/z.hg.hg/inside.txt.i",
      "data/~20space.i",
      "data/~2eleading.i",
      "dh/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaab1068971bbd8477250f462eb099cbb09d78312dc.i",
      "dh/director/seventh_/third.di/fourth.d/fifth.di/sixth.di/eighth.d/deep-file-na8045308eb5772f8d366bd883527c27741dfa75db.i",
      "dh/upper_zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz.txt.i20053374f434dbfe7d9cbc451df115259a188c19.i",
      "fileindex",
      "fileindex-list.150e1cfc",
      "fileindex-meta.b870a51b",
      "fileindex-tree.88698448",
      "requires",
    ]
  );

  /// Shape: one file of ten bytes under two directories of the store.
  const TEN_BYTES: Files = &[("store/data/a.i", b"0123456789")];

  /// Shape: the directory the tests' repositories are at, under the simulated host's root.
  const REPOSITORY: &str = "repository";

  /// Shape: a content store's window, room for a few rows a table, as the import's own tests make one.
  const WINDOW: usize = 1 << 12;

  /// Shape: a content store's budget and an import's room, each history's many times over.
  const ROOM: u64 = 1 << 24;

  /// Shape: a nesting deeper than every handle the base holds at once.
  const DEPTH: usize = 32;

  /// A new content store.
  fn store() -> ContentStore {
    ContentStore::new(ROOM, WINDOW).unwrap()
  }

  /// A host holding `files` under `/repository/.hg`, each directory made once and before what it holds, and the
  /// repository's directory open.
  fn laid_out<'f>(files: impl IntoIterator<Item = (&'f str, &'f [u8])>) -> (SimHost, HostDir) {
    let mut host = SimHost::new();
    let files: Vec<(String, &[u8])> = files
      .into_iter()
      .map(|(path, bytes)| (format!("/{REPOSITORY}/.hg/{path}"), bytes))
      .collect();
    let directories: BTreeSet<&str> = files.iter().flat_map(|(path, _)| parents(path)).collect();
    for directory in directories {
      host.mkdir(directory);
    }
    for (path, bytes) in &files {
      host.replace_file(path, bytes);
    }
    let root = host.root();
    let repository = host.open_dir(root, REPOSITORY).unwrap();
    (host, repository)
  }

  /// Each directory above `path`, the outermost first.
  fn parents(path: &str) -> impl Iterator<Item = &str> {
    path
      .match_indices('/')
      .skip(1)
      .filter_map(move |(at, _)| path.get(..at))
  }

  /// A repository's path under `.hg`, as the import names it.
  fn names(path: &str) -> Vec<&[u8]> {
    path.split('/').map(str::as_bytes).collect()
  }

  /// The host's refusal `refused`, as the base hands it to the import.
  fn by_host(refused: HostError) -> FileRefused<BaseRefused> {
    FileRefused::Host(BaseRefused::Host(refused))
  }

  /// A length as the seam gives one.
  fn to_u64(len: usize) -> u64 {
    u64::try_from(len).unwrap_or(u64::MAX)
  }

  /// The same files read from memory, as the import's own tests read them: the oracle the base's answers are held
  /// to, and the paths of the files the import asked for that the repository holds.
  struct Memory {
    files: BTreeMap<Vec<u8>, &'static [u8]>,
    asked: BTreeSet<Vec<u8>>,
  }

  impl Memory {
    /// `files`, in memory.
    fn of(files: Files) -> Memory {
      let files = files
        .iter()
        .map(|(path, bytes)| (path.as_bytes().to_vec(), *bytes))
        .collect();
      Memory {
        files,
        asked: BTreeSet::new(),
      }
    }

    /// The bytes at `path`, noted as asked for.
    fn bytes(&mut self, path: &[&[u8]]) -> Option<&'static [u8]> {
      let joined = path.join(&b'/');
      let bytes = self.files.get(&joined).copied()?;
      self.asked.insert(joined);
      Some(bytes)
    }
  }

  impl RepositoryFiles for Memory {
    type Refused = BaseRefused;

    fn len(&mut self, path: &[&[u8]]) -> Result<Option<u64>, FileRefused<BaseRefused>> {
      Ok(self.bytes(path).map(|bytes| to_u64(bytes.len())))
    }

    fn read(
      &mut self,
      path: &[&[u8]],
      offset: u64,
      len: u64,
      room: &mut Admission,
    ) -> Result<Option<Vec<u8>>, FileRefused<BaseRefused>> {
      let Some(bytes) = self.bytes(path) else {
        return Ok(None);
      };
      let at = |value: u64| {
        usize::try_from(value)
          .unwrap_or(usize::MAX)
          .min(bytes.len())
      };
      let range = bytes
        .get(at(offset)..at(offset.saturating_add(len)))
        .unwrap_or_default();
      room
        .charge(to_u64(range.len()))
        .map_err(FileRefused::PastAdmission)?;
      Ok(Some(range.to_vec()))
    }
  }

  /// What an import made, compared whole: its index row for row, every count, and the state beside its history.
  #[derive(Debug, PartialEq, Eq)]
  struct Made {
    rows: Rows,
    counts: Counts,
    phase_roots: Vec<PhaseHead>,
    bookmarks: Vec<(Vec<u8>, Node)>,
  }

  /// What `imported` made, its texts in `store`.
  fn made(mut imported: Imported, store: &ContentStore) -> Made {
    let rows = imported
      .index
      .rows()
      .iter(store)
      .map(|row| (row.key().to_vec(), row.body().to_vec()))
      .collect();
    let counts = (
      imported.changesets,
      imported.manifests,
      imported.file_logs,
      imported.file_revisions,
      imported.dropped_phase_roots,
      imported.dropped_bookmarks,
      imported.malformed_bookmarks,
      imported.markers,
      imported.rebuilt_from_cache,
    );
    Made {
      rows,
      counts,
      phase_roots: imported.phase_roots,
      bookmarks: imported.bookmarks,
    }
  }

  /// `files` imported from memory: what the import made, and the files it asked for.
  fn from_memory(files: Files) -> (Made, Memory) {
    let mut memory = Memory::of(files);
    let mut store = store();
    let imported = import::import(&mut memory, &mut store, Admission::new(ROOM)).unwrap();
    (made(imported, &store), memory)
  }

  /// Holds the import through the base equal to the import from memory, and the history it made real: rows,
  /// changesets and file revisions.
  fn check_made(through_base: &Made, from_memory: &Made) {
    let (changesets, file_revisions) = (through_base.counts.0, through_base.counts.3);
    assert!(!through_base.rows.is_empty() && changesets > 0 && file_revisions > 0);
    assert_eq!(through_base, from_memory);
  }

  /// Holds every file the import asked for opened once, and more of the answers given from a held file than files
  /// opened: each length was asked before its file was read, and a split changelog read an entry and a chunk in turn.
  fn check_opened_once(opened: Opened, memory: &Memory) {
    assert_eq!(opened.files, to_u64(memory.asked.len()));
    assert!(opened.reused > opened.files, "{opened:?}");
  }

  /// A-649 part 4 (the oracle): do import the `states` and file index repositories through the base, one lend for the
  /// whole import, and again from the same files in memory. Expect the two imports equal, index row for row, with
  /// every count, phase root and bookmark; every file the import asked for opened once, its later lengths and reads
  /// answered from the held file; and nothing the lend opened left open once it ends.
  #[test]
  fn a_repository_read_through_the_base_imports_as_its_files_in_memory_do() {
    for files in [STATES, FILE_INDEX] {
      let (mut host, repository) = laid_out(files.iter().copied());
      let open_before = host.open_handles();
      let mut store = store();
      let mut base = HgBase::new(&mut host, repository);
      let imported = import::import(&mut base, &mut store, Admission::new(ROOM)).unwrap();
      let opened = base.opened();
      drop(base);
      let (expected, memory) = from_memory(files);
      check_made(&made(imported, &store), &expected);
      check_opened_once(opened, &memory);
      assert_eq!(host.open_handles(), open_before);
    }
  }

  /// A clock a tick a reading, so a turn of quantum 1 ends after its first unit.
  struct Ticking(u64);

  impl Clock for Ticking {
    fn now_ns(&mut self) -> u64 {
      self.0 = self.0.saturating_add(1);
      self.0
    }
  }

  /// One turn of `import` under `clock`, the host lent for that turn alone: its progress, and whether the turn's lend
  /// left nothing open of its own beside the `open_before` handles open before the import.
  fn lent_turn(
    host: &mut SimHost,
    repository: HostDir,
    import: &mut Import,
    store: &mut ContentStore,
    (clock, open_before): (&mut Ticking, usize),
  ) -> (Progress, bool) {
    let mut base = HgBase::new(&mut *host, repository);
    let turn = Turn {
      clock,
      quantum_ns: 1,
    };
    let progress = import.advance(&mut base, store, turn).unwrap();
    drop(base);
    (progress, host.open_handles() == open_before)
  }

  /// A-649 part 4 (lent a turn at a time): do import the `states` repository a unit a turn, the host lent afresh for
  /// each turn as the shard lends it, and again from the same files in memory. Expect more than one turn, nothing left
  /// open after any turn, and the two imports equal, index row for row.
  #[test]
  fn an_import_lent_the_host_a_turn_at_a_time_equals_the_whole_import() {
    let (mut host, repository) = laid_out(STATES.iter().copied());
    let open_before = host.open_handles();
    let mut store = store();
    let start = Import::start(&mut HgBase::new(&mut host, repository), Admission::new(ROOM));
    let mut import = start.unwrap();
    let mut clock = Ticking(0);
    let mut closed_after_every_turn = host.open_handles() == open_before;
    loop {
      let ticking = (&mut clock, open_before);
      let (progress, closed) = lent_turn(&mut host, repository, &mut import, &mut store, ticking);
      closed_after_every_turn &= closed;
      if progress == Progress::Done {
        break;
      }
    }
    assert!(closed_after_every_turn && import.turns() > 1);
    let piecewise = made(import.into_imported().unwrap(), &store);
    assert_eq!(piecewise, from_memory(STATES).0);
  }

  /// A-649 part 4: do ask the base for a file that is not there, a file under a directory that is not there, a
  /// directory where a file is named, a file where a directory is named, and read the absent file. Expect `None` for
  /// each absent one, the host's own refusal for each of the wrong kind, and nothing left open.
  #[test]
  fn an_absent_file_is_none_and_a_name_of_the_wrong_kind_is_refused() {
    let (mut host, repository) = laid_out(TEN_BYTES.iter().copied());
    let open_before = host.open_handles();
    let mut base = HgBase::new(&mut host, repository);
    for (path, expected) in [
      ("store/data/b.i", Ok(None)),
      ("store/none/a.i", Ok(None)),
      ("store/data", Err(by_host(HostError::NotFile))),
      ("store/data/a.i/b.i", Err(by_host(HostError::NotDirectory))),
    ] {
      assert_eq!(base.len(&names(path)), expected, "{path}");
    }
    let mut room = Admission::new(ROOM);
    let absent = base.read(&names("store/data/b.i"), 0, 1, &mut room);
    drop(base);
    assert_eq!((absent, room.left()), (Ok(None), ROOM));
    assert_eq!(host.open_handles(), open_before);
  }

  /// A-649 part 4: do ask the base for a file whose last name is not UTF-8. Expect it refused by kind, naming the
  /// name, without a call to the host.
  #[test]
  fn a_name_that_is_not_utf8_is_refused_before_the_host_is_asked() {
    let (mut host, repository) = laid_out(TEN_BYTES.iter().copied());
    let calls_before = host.calls();
    let name: &[u8] = b"\xff.i";
    let refused = HgBase::new(&mut host, repository).len(&[b"store", b"data", name]);
    let expected = FileRefused::Host(BaseRefused::NotUtf8 { name: name.into() });
    assert_eq!((refused, host.calls()), (Err(expected), calls_before));
  }

  /// A-649 part 4: do read a file of ten bytes for five from its eighth byte, for five from past its end, and whole
  /// under a room of nine. Expect the two bytes it holds read and charged and no more, nothing read or charged past
  /// its end, and the read past the room refused with nothing charged.
  #[test]
  fn a_read_is_charged_what_the_file_holds_and_refused_past_the_room() {
    let (mut host, repository) = laid_out(TEN_BYTES.iter().copied());
    let mut base = HgBase::new(&mut host, repository);
    let path = names("store/data/a.i");
    let mut room = Admission::new(ROOM);
    let tail = base.read(&path, 8, 5, &mut room);
    let past_end = base.read(&path, 12, 5, &mut room);
    assert_eq!((tail, past_end), (Ok(Some(b"89".to_vec())), Ok(Some(Vec::new()))));
    assert_eq!(room.left(), ROOM - 2);
    let mut small = Admission::new(9);
    let refused = base.read(&path, 0, 10, &mut small);
    let expected = FileRefused::PastAdmission(PastAdmission { asked: 10, left: 9 });
    assert_eq!((refused, small.left()), (Err(expected), 9));
  }

  /// Shape: the host's calls of a first length of `store/data/a.i`: `.hg`, `store` and `data` opened, the file opened
  /// and its length taken. The next call is the first read's.
  const FIRST_LENGTH_CALLS: u64 = 5;

  /// A-649 part 4: do take a file's length, have an outsider replace the file (a save by rename) before the next
  /// read, read it whole, then take its length and read it through a new lend. Expect the held file read as it was
  /// opened, its descriptor keeping its bytes and its length, and the new lend reading the new file.
  #[test]
  fn a_file_replaced_while_held_is_read_as_it_was_opened() {
    let (mut host, repository) = laid_out(TEN_BYTES.iter().copied());
    let replaced = b"abcdefghijklmnop";
    host.interfere_at_call(
      FIRST_LENGTH_CALLS,
      Interference::Replace {
        path: format!("/{REPOSITORY}/.hg/store/data/a.i"),
        bytes: replaced.to_vec(),
      },
    );
    let path = names("store/data/a.i");
    let mut room = Admission::new(ROOM);
    let mut base = HgBase::new(&mut host, repository);
    let held = (base.len(&path), base.read(&path, 0, ROOM, &mut room));
    drop(base);
    assert!(host.interfered());
    assert_eq!(held, (Ok(Some(10)), Ok(Some(b"0123456789".to_vec()))));
    let mut base = HgBase::new(&mut host, repository);
    let new = (base.len(&path), base.read(&path, 0, ROOM, &mut room));
    assert_eq!(new, (Ok(Some(16)), Ok(Some(replaced.to_vec()))));
  }

  /// What a lend holds open beside the `open_before` handles open before it, and what it has opened.
  fn holding(base: &HgBase<'_, SimHost>, open_before: usize) -> (usize, Opened) {
    let held = base.host().open_handles().saturating_sub(open_before);
    (held, base.opened())
  }

  /// A-649 part 4 (bounds): do take the length of a file `DEPTH` directories down, of its neighbour, and of a file at
  /// the store's top. Expect the walk down to open every directory and hold one, the neighbour opened under the held
  /// directory without a walk, the top file after a walk of two, and never more than a directory and two files held
  /// whatever the depth.
  #[test]
  fn the_handles_held_stay_bounded_however_deep_a_path() {
    let deep = format!("store/{}", "d/".repeat(DEPTH));
    let (first, second) = (format!("{deep}a.i"), format!("{deep}b.i"));
    let files = [
      (first.as_str(), b"a".as_slice()),
      (second.as_str(), b"b".as_slice()),
      ("store/c.i", b"c".as_slice()),
    ];
    let (mut host, repository) = laid_out(files);
    let open_before = host.open_handles();
    let mut base = HgBase::new(&mut host, repository);
    let walked = to_u64(DEPTH).saturating_add(2);
    let opened = |directories, files| Opened {
      directories,
      files,
      reused: 0,
    };
    base.len(&names(&first)).unwrap();
    assert_eq!(holding(&base, open_before), (2, opened(walked, 1)));
    base.len(&names(&second)).unwrap();
    assert_eq!(holding(&base, open_before), (3, opened(walked, 2)));
    base.len(&names("store/c.i")).unwrap();
    assert_eq!(holding(&base, open_before), (3, opened(walked + 2, 3)));
    drop(base);
    assert_eq!(host.open_handles(), open_before);
  }

  /// A-649 part 4: do import the `states` repository with the manifest log's open answered a fetch still pending (an
  /// S3 base's first read of it). Expect the import refused with the host's refusal as the host gave it, and nothing
  /// the lend opened left open.
  #[test]
  fn a_host_refusal_is_carried_through_and_nothing_is_left_open() {
    let (mut host, repository) = laid_out(STATES.iter().copied());
    let open_before = host.open_handles();
    let pending = HostError::Pending(FetchId(7));
    let manifest = format!("/{REPOSITORY}/.hg/store/00manifest.i");
    host.fail(SimVerb::OpenFile, &manifest, pending, 1);
    let mut store = store();
    let mut base = HgBase::new(&mut host, repository);
    let refused = import::import(&mut base, &mut store, Admission::new(ROOM)).err();
    drop(base);
    let expected = ImportRefused::Host(BaseRefused::Host(pending));
    assert_eq!((refused, host.open_handles()), (Some(expected), open_before));
  }
}
