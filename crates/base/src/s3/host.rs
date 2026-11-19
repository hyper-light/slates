//! The S3 base's host (A-524; §4.15 "An S3 base"): the read-only host seam ([`HostFs`]) answered from the objects
//! under an S3 base's prefix, sans-io. The seam is synchronous and the store is a network away, so the host answers
//! from what it has fetched and names what it has not:
//! - **A miss is parked.** A call the host cannot answer from what it holds answers [`HostError::Pending`] and names
//!   the fetch it needs ([`S3Host::next_fetch`]); the shard's driver runs the fetch against the store (the network
//!   client through [`super::fetch::run`], or [`super::sim::SimS3`] in a test) and hands the answer back
//!   ([`S3Host::landed`]); the parked request, run again, is answered from it. A fetch is asked once however many calls
//!   miss on it, so the fetches under way never outnumber the requests parked on them.
//! - **A directory is its delimited listing** (`ListObjectsV2`, `delimiter=/`), every page of it, mapped to its entries
//!   by [`super::keys`]: each key that names no path, and each whose `LastModified` is no time S3 writes, refused by
//!   its kind and counted, never dropped in silence. A listing serves as the directory's state for one round trip of
//!   the path from when it landed (the time a listing asked again would take to answer, learned from the fetches' own
//!   measured round trips by RFC 9002's estimator); a call after that is answered from it while the host lists the
//!   directory again, and the next call after that listing lands sees what changed, its fingerprint moved. The first
//!   listing of a directory is a miss.
//! - **A file is bound to its object.** Opening a file binds the handle to the object its listing named: every range
//!   read of it carries `If-Match` with that ETag, and the object's version once the store names one, so a handle never
//!   reads two objects' bytes. On a versioned bucket a replaced object's handle still reads its own version, as a
//!   disk's open descriptor reads its own inode; elsewhere the replacement answers `412` and the handle is stale
//!   ([`HostError::StaleHandle`], the base plane's drift, A-151), as a deleted object's (`404`) is. The file's
//!   fingerprint carries the base's identity as its device, a digest of its path as its inode number, its size,
//!   `LastModified` as its times and a digest of its ETag as its birth, so a replaced object is a new file.
//! - **Ranges are the volume's chunks** (§4.5, the copy-on-write unit), fetched as one run from the first a read
//!   misses, and held in a cache whose bytes, listings with them, stay within the budget the caller gives (the shard's
//!   admittable room), the least recently used shed first. A listing larger than the whole budget is refused.
//! - **A refusal answers the calls of one round trip.** A fetch the store refused answers the calls that waited for it
//!   and those within one round trip of its landing (`403` and an expired credential `EACCES`, a missing bucket
//!   `NotFound`, the rest `EIO`, as §4.15's `BaseUnavailable`); a call after that asks again.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use aws_lc_rs::digest;
use slates_transport::pacer::NANOS_PER_SECOND;
use slates_transport::rtt::RttEstimator;
use slates_vfs::host::{
  BaseEntry, Hint, HostDir, HostError, HostFacts, HostFile, HostFs, HostKind, OsCode, WatchState,
};
use slates_vfs::inode::Fingerprint;

use super::client::{RangeRead, S3Error};
use super::fetch::{Fetch, Fetched};
use super::keys::{self, SEPARATOR};
use super::list::{ListPage, Listed};
use super::name::S3Base;
use super::time::ns_of_iso8601;

/// Format: the `st_mode` of a base directory: `S_IFDIR` over `rwxr-xr-x`. S3 keeps no modes; a directory is what its
/// keys' prefixes make, readable and searchable by all, as a directory made under the conventional umask is.
const DIR_MODE: u32 = 0o040_755;
/// Format: the `st_mode` of a base file: `S_IFREG` over `rw-r--r--`, as a file made under the conventional umask is.
const FILE_MODE: u32 = 0o100_644;
/// Format: POSIX's `EIO`, 5 on Linux and macOS: the store did not answer, or answered what was not asked. An errno on
/// every platform, typed so (A-539): the S3 base names a store's refusal in POSIX's words.
const EIO: OsCode = OsCode::Errno(5);
/// Format: POSIX's `ENOMEM`, 12 on Linux and macOS: a directory's listing is larger than the base's whole cache. An
/// errno on every platform, typed so (A-539).
const ENOMEM: OsCode = OsCode::Errno(12);
/// Format: POSIX's `EACCES`, 13 on Linux and macOS: the store refused the credential, or it expired. An errno on every
/// platform, typed so (A-539).
const EACCES: OsCode = OsCode::Errno(13);
/// Format: POSIX's `EINVAL`, 22 on Linux and macOS: a name asked for its link target that is no link, as `readlinkat`
/// answers. S3 has no links, so a base lists none. An errno on every platform, typed so (A-539).
const EINVAL: OsCode = OsCode::Errno(22);
/// Format: the domains a digest of a path and of an ETag are taken in, so neither collides with the other or with a
/// base's identity.
const INODE_DOMAIN: &[u8] = b"slates s3 inode\0";
/// Format: see [`INODE_DOMAIN`].
const BIRTH_DOMAIN: &[u8] = b"slates s3 birth\0";

/// What the host is built with.
#[derive(Clone, Copy, Debug)]
pub struct HostConfig {
  /// The volume's chunk (§4.5): the unit a range is fetched and cached in.
  pub chunk_bytes: u64,
  /// The bytes of fetched ranges and listings the host may hold: the shard's admittable room it is given.
  pub cache_bytes: u64,
  /// The owner every entry reports, as `stat` reports `st_uid` and `st_gid`: the daemon's own user and group.
  pub owner: (u32, u32),
  /// The host's wall clock, nanoseconds since the Unix epoch, the domain S3's times are in.
  pub wall_ns: fn() -> i64,
}

/// Why a host could not be built.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostRefused {
  /// The chunk is zero bytes.
  ZeroChunk,
  /// The cache cannot hold one chunk, so no read could ever be answered.
  CacheBelowChunk,
}

/// Why a listed entry is not in its directory, beside the keys that name no path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntryRefused {
  /// The key names no path ([`keys::Refused`]).
  Name(keys::Refused),
  /// Its `LastModified` is no time S3 writes.
  Time,
}

/// The host's counts, for a test's non-vacuity checks and the base's report.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HostCounts {
  /// Calls answered from what the host held.
  pub hits: u64,
  /// Calls answered [`HostError::Pending`].
  pub misses: u64,
  /// Fetches handed to the driver.
  pub fetches: u64,
  /// Listing pages landed.
  pub pages: u64,
  /// Ranges landed.
  pub ranges: u64,
  /// Directories listed again because their listing outlived its round trip.
  pub relists: u64,
  /// Objects found replaced or deleted under a bound read (`412`, `404`).
  pub drifted: u64,
  /// Fetches the store refused.
  pub failures: u64,
  /// Entries refused, each time a listing named one.
  pub refused_entries: u64,
  /// Cached bytes shed to stay within the budget.
  pub shed_bytes: u64,
}

/// A file the host has opened: the object it is bound to.
#[derive(Clone, Debug)]
struct OpenFile {
  key: String,
  etag: String,
  version: Option<String>,
  size: u64,
  fingerprint: Fingerprint,
  drifted: bool,
}

/// A file as its directory's listing names it: its fingerprint, and the ETag a handle opened on it is bound to.
#[derive(Clone, Debug)]
struct ListedFile {
  fingerprint: Fingerprint,
  etag: String,
}

/// A directory's listing as the host last completed it.
#[derive(Clone, Debug)]
struct Complete {
  entries: Vec<BaseEntry>,
  files: BTreeMap<String, ListedFile>,
  directories: BTreeSet<String>,
  refused: Vec<(String, EntryRefused)>,
  fingerprint: Fingerprint,
  digest: Vec<u8>,
  landed_ns: i64,
  bytes: u64,
}

/// A directory's listing: the last complete one, the one being read, and a refusal answering its window.
#[derive(Clone, Debug, Default)]
struct Listing {
  complete: Option<Complete>,
  reading: Option<Reading>,
  failed: Option<Failed>,
}

/// A listing being read: the pages so far, and the token of the next.
#[derive(Clone, Debug, Default)]
struct Reading {
  objects: Vec<Listed>,
  prefixes: Vec<String>,
  token: Option<String>,
  bytes: u64,
}

/// A refusal of the store, and when it landed.
#[derive(Clone, Copy, Debug)]
struct Failed {
  error: HostError,
  landed_ns: i64,
}

/// A run of an object's chunks to fetch.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Run {
  key: String,
  etag: String,
  version: Option<String>,
  size: u64,
  first: u64,
  count: u64,
}

/// What a miss waits for.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Need {
  /// The next page of the listing of the directory at this path.
  Listing(String),
  /// A run of chunks.
  Chunks(Run),
}

/// A cached chunk: an object's key, its ETag (its content), and the chunk's index.
type ChunkKey = (String, String, u64);

/// What the cache holds.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Cached {
  /// A chunk.
  Chunk(ChunkKey),
  /// The complete listing of the directory at this path.
  Listing(String),
}

/// An S3 base's host.
pub struct S3Host {
  prefix: String,
  dev: u64,
  config: HostConfig,
  dirs: BTreeMap<u64, String>,
  files: BTreeMap<u64, OpenFile>,
  root: HostDir,
  next_handle: u64,
  listings: BTreeMap<String, Listing>,
  chunks: BTreeMap<ChunkKey, Vec<u8>>,
  fetching: BTreeSet<ChunkKey>,
  failed_reads: BTreeMap<(String, String), Failed>,
  recency: BTreeMap<u64, Cached>,
  last_use: BTreeMap<Cached, u64>,
  sizes: BTreeMap<Cached, u64>,
  tick: u64,
  cached_bytes: u64,
  wanted: VecDeque<Need>,
  in_flight: BTreeMap<u64, Need>,
  next_ticket: u64,
  round_trips: RttEstimator,
  counts: HostCounts,
}

impl S3Host {
  /// A host of the objects `base` names, built with `config`; its root directory open ([`S3Host::root`]).
  pub fn new(base: &S3Base, config: HostConfig) -> Result<S3Host, HostRefused> {
    if config.chunk_bytes == 0 {
      return Err(HostRefused::ZeroChunk);
    }
    if config.cache_bytes < config.chunk_bytes {
      return Err(HostRefused::CacheBelowChunk);
    }
    let root = HostDir(1);
    let mut dirs = BTreeMap::new();
    dirs.insert(root.0, String::new());
    Ok(S3Host {
      prefix: base.prefix().to_owned(),
      dev: base.identity(),
      config,
      dirs,
      files: BTreeMap::new(),
      root,
      next_handle: 2,
      listings: BTreeMap::new(),
      chunks: BTreeMap::new(),
      fetching: BTreeSet::new(),
      failed_reads: BTreeMap::new(),
      recency: BTreeMap::new(),
      last_use: BTreeMap::new(),
      sizes: BTreeMap::new(),
      tick: 0,
      cached_bytes: 0,
      wanted: VecDeque::new(),
      in_flight: BTreeMap::new(),
      next_ticket: 1,
      round_trips: RttEstimator::new(),
      counts: HostCounts::default(),
    })
  }

  /// The base's root directory, open from the host's making.
  pub fn root(&self) -> HostDir {
    self.root
  }

  /// The host's counts so far.
  pub fn counts(&self) -> HostCounts {
    self.counts
  }

  /// The bytes the cache holds now.
  pub fn cached_bytes(&self) -> u64 {
    self.cached_bytes
  }

  /// The entries the last complete listing of the directory at `path` refused, each with why.
  pub fn refused(&self, path: &str) -> Vec<(String, EntryRefused)> {
    self
      .listings
      .get(path)
      .and_then(|listing| listing.complete.as_ref())
      .map(|complete| complete.refused.clone())
      .unwrap_or_default()
  }

  /// The next fetch a parked call waits for, with the ticket its answer is handed back under; none while nothing waits.
  pub fn next_fetch(&mut self) -> Option<(u64, Fetch)> {
    let need = self.wanted.pop_front()?;
    let fetch = match &need {
      Need::Listing(path) => Fetch::List {
        prefix: self.key_prefix(path),
        token: self
          .listings
          .get(path)
          .and_then(|listing| listing.reading.as_ref())
          .and_then(|reading| reading.token.clone()),
      },
      Need::Chunks(run) => {
        let first = run.first.saturating_mul(self.config.chunk_bytes);
        Fetch::Range {
          key: run.key.clone(),
          first,
          len: run
            .count
            .saturating_mul(self.config.chunk_bytes)
            .min(run.size.saturating_sub(first)),
          etag: run.etag.clone(),
          version: run.version.clone(),
        }
      }
    };
    let ticket = self.next_ticket;
    self.next_ticket = self.next_ticket.saturating_add(1);
    self.in_flight.insert(ticket, need);
    self.counts.fetches = self.counts.fetches.saturating_add(1);
    Some((ticket, fetch))
  }

  /// The store's `answer` to the fetch handed out under `ticket`, which took `elapsed_ns` from asking to answer (a
  /// sample of the path's round trip when the store answered it). A ticket not handed out, or already landed, is passed
  /// over.
  pub fn landed(&mut self, ticket: u64, answer: Result<Fetched, S3Error>, elapsed_ns: u64) {
    let Some(need) = self.in_flight.remove(&ticket) else {
      return;
    };
    if answer.is_ok() {
      self.round_trips.on_sample(elapsed_ns, 0);
    }
    match (need, answer) {
      (Need::Listing(path), Ok(Fetched::Page(page))) => self.land_page(&path, page),
      (Need::Listing(path), Ok(Fetched::Range(_))) => {
        self.fail_listing(&path, HostError::Unavailable(EIO));
      }
      (Need::Listing(path), Err(error)) => self.fail_listing(&path, refusal_of(&error)),
      (Need::Chunks(run), Ok(Fetched::Range(read))) => self.land_range(&run, read),
      (Need::Chunks(run), Ok(Fetched::Page(_))) => self.fail_run(&run, HostError::Unavailable(EIO)),
      (
        Need::Chunks(run),
        Err(S3Error::Changed | S3Error::Missing { .. } | S3Error::RangeNotSatisfiable),
      ) => self.drift(&run),
      (Need::Chunks(run), Err(error)) => self.fail_run(&run, refusal_of(&error)),
    }
  }

  // ------------------------------------------------------------------------------------------------ listings

  /// The window a listing and a refusal serve for: the path's round trip, learned from the fetches' own (RFC 9002's
  /// initial RTT until one is measured).
  fn window_ns(&self) -> i64 {
    i64::try_from(self.round_trips.smoothed_rtt_or_initial()).unwrap_or(i64::MAX)
  }

  /// The prefix of the keys of the directory at `path`: the base's prefix, and the path with its separator.
  fn key_prefix(&self, path: &str) -> String {
    if path.is_empty() {
      self.prefix.clone()
    } else {
      format!("{}{path}{SEPARATOR}", self.prefix)
    }
  }

  /// Makes sure the directory at `path` has a complete listing to answer from: `Ok` when it has, the refusal a failed
  /// listing answers within its window, or [`HostError::Pending`] with its listing asked for. A listing past its window
  /// still answers while it is listed again.
  fn ask_listing(&mut self, path: &str) -> Result<(), HostError> {
    let now = (self.config.wall_ns)();
    let window = self.window_ns();
    let listing = self.listings.entry(path.to_owned()).or_default();
    if let Some(failed) = listing.failed {
      if now <= failed.landed_ns.saturating_add(window) {
        return Err(failed.error);
      }
      listing.failed = None;
    }
    let (complete, stale) = match &listing.complete {
      Some(complete) => (true, now > complete.landed_ns.saturating_add(window)),
      None => (false, true),
    };
    let ask = stale && listing.reading.is_none();
    if ask {
      listing.reading = Some(Reading::default());
      self.wanted.push_back(Need::Listing(path.to_owned()));
    }
    if complete {
      if ask {
        self.counts.relists = self.counts.relists.saturating_add(1);
      }
      self.counts.hits = self.counts.hits.saturating_add(1);
      self.touch(Cached::Listing(path.to_owned()));
      Ok(())
    } else {
      self.counts.misses = self.counts.misses.saturating_add(1);
      Err(HostError::Pending)
    }
  }

  /// The complete listing of the directory at `path`, after [`S3Host::ask_listing`] answered `Ok`.
  fn complete(&self, path: &str) -> Result<&Complete, HostError> {
    self
      .listings
      .get(path)
      .and_then(|listing| listing.complete.as_ref())
      .ok_or(HostError::Pending)
  }

  /// A page of the listing of the directory at `path` landed: kept, and the next asked for while it is truncated; the
  /// last makes the listing complete.
  fn land_page(&mut self, path: &str, page: ListPage) {
    self.counts.pages = self.counts.pages.saturating_add(1);
    let budget = self.config.cache_bytes;
    let Some(reading) = self
      .listings
      .get_mut(path)
      .and_then(|listing| listing.reading.as_mut())
    else {
      return;
    };
    let added: u64 = page
      .objects
      .iter()
      .map(listed_bytes)
      .chain(page.prefixes.iter().map(|prefix| string_bytes(prefix)))
      .fold(0, u64::saturating_add);
    reading.bytes = reading.bytes.saturating_add(added);
    reading.objects.extend(page.objects);
    reading.prefixes.extend(page.prefixes);
    if reading.bytes > budget {
      self.fail_listing(path, HostError::Unavailable(ENOMEM));
      return;
    }
    match (page.truncated, page.next_token) {
      (true, Some(token)) => {
        reading.token = Some(token);
        self.wanted.push_back(Need::Listing(path.to_owned()));
      }
      // A truncated page without its token cannot be continued: the store answered what no listing is.
      (true, None) => self.fail_listing(path, HostError::Unavailable(EIO)),
      (false, _) => self.complete_listing(path),
    }
  }

  /// The listing being read of the directory at `path` is whole: its entries made, its fingerprint moved when they
  /// changed, and it held in the cache.
  fn complete_listing(&mut self, path: &str) {
    let now = (self.config.wall_ns)();
    let Some(listing) = self.listings.get_mut(path) else {
      return;
    };
    let Some(reading) = listing.reading.take() else {
      return;
    };
    let previous = listing.complete.take();
    let complete = self.complete_of(path, reading, previous.as_ref(), now);
    self.counts.refused_entries = self
      .counts
      .refused_entries
      .saturating_add(u64::try_from(complete.refused.len()).unwrap_or(u64::MAX));
    if previous.is_some() {
      self.uncache(&Cached::Listing(path.to_owned()));
    }
    let bytes = complete.bytes;
    if let Some(listing) = self.listings.get_mut(path) {
      listing.complete = Some(complete);
    }
    self.cache(Cached::Listing(path.to_owned()), bytes);
  }

  /// The complete listing a whole `reading` of the directory at `path` makes at `now`, its change time kept from
  /// `previous` when its entries are the same.
  fn complete_of(
    &self,
    path: &str,
    reading: Reading,
    previous: Option<&Complete>,
    now: i64,
  ) -> Complete {
    let directory = keys::directory_of(&self.key_prefix(path), &reading.objects, &reading.prefixes);
    let mut refused: Vec<(String, EntryRefused)> = directory
      .refused
      .into_iter()
      .map(|(key, why)| (key, EntryRefused::Name(why)))
      .collect();
    let mut entries = Vec::new();
    let mut files = BTreeMap::new();
    let mut summary = digest::Context::new(&digest::SHA256);
    let mut newest: i64 = 0;
    for (name, listed) in directory.files {
      let Some(mtime) = ns_of_iso8601(&listed.last_modified) else {
        refused.push((listed.key, EntryRefused::Time));
        continue;
      };
      let (size, time) = (listed.size.to_be_bytes(), mtime.to_be_bytes());
      summary.update(b"file");
      for part in [
        name.as_bytes(),
        listed.etag.as_bytes(),
        size.as_slice(),
        time.as_slice(),
      ] {
        summary.update(&u64::try_from(part.len()).unwrap_or(u64::MAX).to_be_bytes());
        summary.update(part);
      }
      newest = newest.max(mtime);
      let fingerprint = self.file_fingerprint(&join(path, &name), &listed, mtime);
      entries.push(BaseEntry {
        name: name.as_str().into(),
        kind: HostKind::File,
        fingerprint,
      });
      files.insert(
        name,
        ListedFile {
          fingerprint,
          etag: listed.etag,
        },
      );
    }
    for name in &directory.directories {
      summary.update(b"directory");
      summary.update(&u64::try_from(name.len()).unwrap_or(u64::MAX).to_be_bytes());
      summary.update(name.as_bytes());
      entries.push(BaseEntry {
        name: name.as_str().into(),
        kind: HostKind::Dir,
        fingerprint: self.dir_fingerprint(&join(path, name), 0, 0, 0),
      });
    }
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    let digest = summary.finish().as_ref().to_vec();
    let changed_ns = match previous {
      Some(previous) if previous.digest == digest => previous.fingerprint.ctime_ns,
      _ => now,
    };
    let count = u64::try_from(entries.len()).unwrap_or(u64::MAX);
    let bytes = reading.bytes;
    Complete {
      fingerprint: self.dir_fingerprint(path, count, newest, changed_ns),
      entries,
      files,
      directories: directory.directories,
      refused,
      digest,
      landed_ns: now,
      bytes,
    }
  }

  /// A listing of the directory at `path` refused with `error`: a first listing's parked calls are answered with it for
  /// a window; a listing asked again while an older one answers keeps the older one answering.
  fn fail_listing(&mut self, path: &str, error: HostError) {
    self.counts.failures = self.counts.failures.saturating_add(1);
    let now = (self.config.wall_ns)();
    if let Some(listing) = self.listings.get_mut(path) {
      listing.reading = None;
      if listing.complete.is_none() {
        listing.failed = Some(Failed {
          error,
          landed_ns: now,
        });
      }
    }
  }

  /// A directory's fingerprint: the base's device, its path's inode number, its entries' count as its size, its newest
  /// file's time as its modification and when the host last saw its entries change as its change.
  fn dir_fingerprint(
    &self,
    path: &str,
    count: u64,
    newest_ns: i64,
    changed_ns: i64,
  ) -> Fingerprint {
    Fingerprint {
      dev: self.dev,
      ino: inode_of(path),
      size: count,
      mtime_ns: newest_ns,
      ctime_ns: changed_ns,
      mode: DIR_MODE,
      uid: self.config.owner.0,
      gid: self.config.owner.1,
      birth_ns: 0,
      windows: 0,
    }
  }

  /// A file's fingerprint from its listing: the base's device, its path's inode number, its size, `LastModified` as its
  /// times, and its ETag's digest as its birth.
  fn file_fingerprint(&self, path: &str, listed: &Listed, mtime_ns: i64) -> Fingerprint {
    Fingerprint {
      dev: self.dev,
      ino: inode_of(path),
      size: listed.size,
      mtime_ns,
      ctime_ns: mtime_ns,
      mode: FILE_MODE,
      uid: self.config.owner.0,
      gid: self.config.owner.1,
      birth_ns: birth_of(&listed.etag),
      windows: 0,
    }
  }

  // ------------------------------------------------------------------------------------------------ files

  /// Reads from the cache what it holds of `file` from `off`; a miss asks for the run of chunks the read needs.
  fn read(&mut self, file: HostFile, off: u64, buf: &mut [u8]) -> Result<usize, HostError> {
    let opened = self.files.get(&file.0).ok_or(HostError::StaleHandle)?;
    if opened.drifted {
      return Err(HostError::StaleHandle);
    }
    let len = u64::try_from(buf.len()).unwrap_or(u64::MAX);
    if len == 0 || off >= opened.size {
      return Ok(0);
    }
    let (key, etag) = (opened.key.clone(), opened.etag.clone());
    let end = off.saturating_add(len).min(opened.size);
    let now = (self.config.wall_ns)();
    let binding = (key.clone(), etag.clone());
    if let Some(failed) = self.failed_reads.get(&binding).copied() {
      if now <= failed.landed_ns.saturating_add(self.window_ns()) {
        return Err(failed.error);
      }
      self.failed_reads.remove(&binding);
    }
    let done = self.copy_cached(&key, &etag, off, end, buf);
    if done > 0 {
      self.counts.hits = self.counts.hits.saturating_add(1);
      return Ok(done);
    }
    self.ask_run(file, off, end);
    self.counts.misses = self.counts.misses.saturating_add(1);
    Err(HostError::Pending)
  }

  /// Copies the cached bytes of the object `(key, etag)` from `off` towards `end` into `buf`, chunk after chunk while
  /// they are cached: how many.
  fn copy_cached(&mut self, key: &str, etag: &str, off: u64, end: u64, buf: &mut [u8]) -> usize {
    let chunk_bytes = self.config.chunk_bytes;
    let mut done: usize = 0;
    let mut at = off;
    while at < end {
      let index = at.checked_div(chunk_bytes).unwrap_or(0);
      let chunk_key = (key.to_owned(), etag.to_owned(), index);
      let Some(bytes) = self.chunks.get(&chunk_key) else {
        break;
      };
      let start = index.saturating_mul(chunk_bytes);
      let from = usize::try_from(at.saturating_sub(start)).unwrap_or(usize::MAX);
      let to = usize::try_from(end.saturating_sub(start))
        .unwrap_or(usize::MAX)
        .min(bytes.len());
      let piece = bytes.get(from..to).unwrap_or_default();
      let Some(target) = buf.get_mut(done..done.saturating_add(piece.len())) else {
        break;
      };
      target.copy_from_slice(piece);
      if piece.is_empty() {
        break;
      }
      done = done.saturating_add(piece.len());
      at = at.saturating_add(u64::try_from(piece.len()).unwrap_or(u64::MAX));
      self.touch(Cached::Chunk(chunk_key));
    }
    done
  }

  /// Asks for the run of `file`'s chunks from the one holding `off` towards the one holding `end - 1`, stopping at a
  /// chunk held or under way and at what the cache can hold; nothing when the first is under way already.
  fn ask_run(&mut self, file: HostFile, off: u64, end: u64) {
    let Some(opened) = self.files.get(&file.0) else {
      return;
    };
    let chunk_bytes = self.config.chunk_bytes;
    let first = off.checked_div(chunk_bytes).unwrap_or(0);
    let last = end.saturating_sub(1).checked_div(chunk_bytes).unwrap_or(0);
    let most = self
      .config
      .cache_bytes
      .checked_div(chunk_bytes)
      .unwrap_or(1)
      .max(1);
    let mut count: u64 = 0;
    let mut index = first;
    while index <= last && count < most {
      let chunk_key = (opened.key.clone(), opened.etag.clone(), index);
      if self.chunks.contains_key(&chunk_key) || self.fetching.contains(&chunk_key) {
        break;
      }
      count = count.saturating_add(1);
      index = index.saturating_add(1);
    }
    if count == 0 {
      return;
    }
    let run = Run {
      key: opened.key.clone(),
      etag: opened.etag.clone(),
      version: opened.version.clone(),
      size: opened.size,
      first,
      count,
    };
    for index in first..first.saturating_add(count) {
      self
        .fetching
        .insert((run.key.clone(), run.etag.clone(), index));
    }
    self.wanted.push_back(Need::Chunks(run));
  }

  /// A run's range landed: its chunks held, and every handle bound to its object bound to the version the store named.
  fn land_range(&mut self, run: &Run, read: RangeRead) {
    self.counts.ranges = self.counts.ranges.saturating_add(1);
    self.release(run);
    let first_byte = run.first.saturating_mul(self.config.chunk_bytes);
    let expected = run
      .count
      .saturating_mul(self.config.chunk_bytes)
      .min(run.size.saturating_sub(first_byte));
    if read.etag != run.etag {
      // `If-Match` makes the store answer the bound object or `412`: an answer of another is a replacement.
      self.drift(run);
      return;
    }
    if read.total != run.size || u64::try_from(read.bytes.len()).unwrap_or(u64::MAX) != expected {
      self.fail_run(run, HostError::Unavailable(EIO));
      return;
    }
    if let Some(version) = &read.version {
      for opened in self.files.values_mut() {
        if opened.key == run.key && opened.etag == run.etag && opened.version.is_none() {
          opened.version = Some(version.clone());
        }
      }
    }
    let chunk = usize::try_from(self.config.chunk_bytes).unwrap_or(usize::MAX);
    for (offset, piece) in read.bytes.chunks(chunk).enumerate() {
      let index = run
        .first
        .saturating_add(u64::try_from(offset).unwrap_or(u64::MAX));
      let chunk_key = (run.key.clone(), run.etag.clone(), index);
      self.chunks.insert(chunk_key.clone(), piece.to_vec());
      self.cache(
        Cached::Chunk(chunk_key),
        u64::try_from(piece.len()).unwrap_or(u64::MAX),
      );
    }
  }

  /// The object a run is bound to is no longer the store's (`412`, `404`, `416`): every handle bound to it stale, its
  /// cached chunks gone, and its directory listed again on its next use.
  fn drift(&mut self, run: &Run) {
    self.counts.drifted = self.counts.drifted.saturating_add(1);
    self.release(run);
    for opened in self.files.values_mut() {
      if opened.key == run.key && opened.etag == run.etag {
        opened.drifted = true;
      }
    }
    let gone: Vec<ChunkKey> = self
      .chunks
      .keys()
      .filter(|(key, etag, _)| *key == run.key && *etag == run.etag)
      .cloned()
      .collect();
    for chunk_key in gone {
      self.uncache(&Cached::Chunk(chunk_key));
    }
    let directory = run
      .key
      .strip_prefix(&self.prefix)
      .and_then(|relative| relative.rsplit_once(SEPARATOR))
      .map(|(directory, _)| directory.to_owned())
      .unwrap_or_default();
    if let Some(complete) = self
      .listings
      .get_mut(&directory)
      .and_then(|listing| listing.complete.as_mut())
    {
      complete.landed_ns = i64::MIN;
    }
  }

  /// A run's fetch refused with `error`: the reads of its object answered with it for a window.
  fn fail_run(&mut self, run: &Run, error: HostError) {
    self.counts.failures = self.counts.failures.saturating_add(1);
    self.release(run);
    let now = (self.config.wall_ns)();
    self.failed_reads.insert(
      (run.key.clone(), run.etag.clone()),
      Failed {
        error,
        landed_ns: now,
      },
    );
  }

  /// A run's chunks are no longer under way.
  fn release(&mut self, run: &Run) {
    for index in run.first..run.first.saturating_add(run.count) {
      self
        .fetching
        .remove(&(run.key.clone(), run.etag.clone(), index));
    }
  }

  // ------------------------------------------------------------------------------------------------ the cache

  /// Holds `item` of `bytes` in the cache, shedding the least recently used others until the cache is within its
  /// budget. A listing larger than the whole budget is refused, never held.
  fn cache(&mut self, item: Cached, bytes: u64) {
    if bytes > self.config.cache_bytes {
      if let Cached::Listing(path) = &item {
        let path = path.clone();
        if let Some(listing) = self.listings.get_mut(&path) {
          listing.complete = None;
        }
        self.fail_listing(&path, HostError::Unavailable(ENOMEM));
      }
      return;
    }
    while self.cached_bytes.saturating_add(bytes) > self.config.cache_bytes {
      let Some((_, oldest)) = self.recency.pop_first() else {
        break;
      };
      self.last_use.remove(&oldest);
      let shed = self.sizes.remove(&oldest).unwrap_or(0);
      self.cached_bytes = self.cached_bytes.saturating_sub(shed);
      self.counts.shed_bytes = self.counts.shed_bytes.saturating_add(shed);
      self.forget(&oldest);
    }
    self.sizes.insert(item.clone(), bytes);
    self.cached_bytes = self.cached_bytes.saturating_add(bytes);
    self.touch(item);
  }

  /// Drops `item` from the cache, its bytes given back.
  fn uncache(&mut self, item: &Cached) {
    if let Some(used) = self.last_use.remove(item) {
      self.recency.remove(&used);
    }
    let bytes = self.sizes.remove(item).unwrap_or(0);
    self.cached_bytes = self.cached_bytes.saturating_sub(bytes);
    self.forget(item);
  }

  /// Forgets what `item` held: a chunk's bytes, or a listing's complete state.
  fn forget(&mut self, item: &Cached) {
    match item {
      Cached::Chunk(chunk_key) => {
        self.chunks.remove(chunk_key);
      }
      Cached::Listing(path) => {
        if let Some(listing) = self.listings.get_mut(path) {
          listing.complete = None;
        }
      }
    }
  }

  /// Marks `item` the most recently used.
  fn touch(&mut self, item: Cached) {
    if !self.sizes.contains_key(&item) {
      return;
    }
    self.tick = self.tick.saturating_add(1);
    if let Some(used) = self.last_use.insert(item.clone(), self.tick) {
      self.recency.remove(&used);
    }
    self.recency.insert(self.tick, item);
  }

  // ------------------------------------------------------------------------------------------------ handles

  /// The path of the open directory `dir`.
  fn dir_path(&self, dir: HostDir) -> Result<String, HostError> {
    self.dirs.get(&dir.0).cloned().ok_or(HostError::StaleHandle)
  }

  /// A new handle number.
  fn handle(&mut self) -> u64 {
    let handle = self.next_handle;
    self.next_handle = self.next_handle.saturating_add(1);
    handle
  }
}

impl HostFs for S3Host {
  fn facts(&mut self, dir: HostDir) -> Result<HostFacts, HostError> {
    self.dir_path(dir)?;
    // Format: S3 states `LastModified` to the second (`HeadObject`'s is an HTTP-date, RFC 9110 §5.6.7; a listing's the
    // same instant, written `.000`).
    Ok(HostFacts {
      timestamp_granularity_ns: NANOS_PER_SECOND,
    })
  }

  fn fingerprint_dir(&mut self, dir: HostDir) -> Result<Fingerprint, HostError> {
    let path = self.dir_path(dir)?;
    self.ask_listing(&path)?;
    Ok(self.complete(&path)?.fingerprint)
  }

  fn list(&mut self, dir: HostDir) -> Result<Vec<BaseEntry>, HostError> {
    let path = self.dir_path(dir)?;
    self.ask_listing(&path)?;
    Ok(self.complete(&path)?.entries.clone())
  }

  fn open_dir(&mut self, parent: HostDir, name: &str) -> Result<HostDir, HostError> {
    let path = self.dir_path(parent)?;
    self.ask_listing(&path)?;
    let complete = self.complete(&path)?;
    if complete.directories.contains(name) {
      let handle = self.handle();
      self.dirs.insert(handle, join(&path, name));
      return Ok(HostDir(handle));
    }
    if complete.files.contains_key(name) {
      return Err(HostError::NotDirectory);
    }
    Err(HostError::NotFound)
  }

  fn open_file(&mut self, dir: HostDir, name: &str) -> Result<HostFile, HostError> {
    let path = self.dir_path(dir)?;
    self.ask_listing(&path)?;
    let complete = self.complete(&path)?;
    let Some(listed) = complete.files.get(name) else {
      return Err(if complete.directories.contains(name) {
        HostError::NotFile
      } else {
        HostError::NotFound
      });
    };
    let (fingerprint, etag) = (listed.fingerprint, listed.etag.clone());
    let key = format!("{}{name}", self.key_prefix(&path));
    let handle = self.handle();
    self.files.insert(
      handle,
      OpenFile {
        key,
        etag,
        version: None,
        size: fingerprint.size,
        fingerprint,
        drifted: false,
      },
    );
    Ok(HostFile(handle))
  }

  fn fstat(&mut self, file: HostFile) -> Result<Fingerprint, HostError> {
    self
      .files
      .get(&file.0)
      .map(|opened| opened.fingerprint)
      .ok_or(HostError::StaleHandle)
  }

  fn read_at(&mut self, file: HostFile, off: u64, buf: &mut [u8]) -> Result<usize, HostError> {
    self.read(file, off, buf)
  }

  fn read_link(&mut self, dir: HostDir, name: &str) -> Result<Box<str>, HostError> {
    let path = self.dir_path(dir)?;
    self.ask_listing(&path)?;
    let complete = self.complete(&path)?;
    if complete.files.contains_key(name) || complete.directories.contains(name) {
      return Err(HostError::Unavailable(EINVAL));
    }
    Err(HostError::NotFound)
  }

  fn close_file(&mut self, file: HostFile) {
    self.files.remove(&file.0);
  }

  fn close_dir(&mut self, dir: HostDir) {
    if dir != self.root {
      self.dirs.remove(&dir.0);
    }
  }

  fn watch(&mut self, _dir: HostDir) -> WatchState {
    WatchState::Unavailable
  }

  fn hints(&mut self) -> Vec<Hint> {
    Vec::new()
  }

  fn now_ns(&mut self) -> i64 {
    (self.config.wall_ns)()
  }
}

/// `name` under the directory at `path`.
fn join(path: &str, name: &str) -> String {
  if path.is_empty() {
    name.to_owned()
  } else {
    format!("{path}{SEPARATOR}{name}")
  }
}

/// A store's refusal as the host answers it: a missing bucket `NotFound`, a refused or expired credential `EACCES`,
/// anything else (the store unavailable past its retries, a mismatch, a hostile page) `EIO`.
fn refusal_of(error: &S3Error) -> HostError {
  match error {
    S3Error::Missing { .. } => HostError::NotFound,
    S3Error::Denied { .. } | S3Error::CredentialExpired => HostError::Unavailable(EACCES),
    _ => HostError::Unavailable(EIO),
  }
}

/// The inode number of the entry at `path`: a digest of the path, never zero, so an entry keeps its number across
/// listings and restarts.
fn inode_of(path: &str) -> u64 {
  let digest = digest_of(INODE_DOMAIN, path.as_bytes());
  u64::from_be_bytes(digest).max(1)
}

/// The birth of an object with `etag`: a digest of the ETag, never zero, so a replaced object is a new file.
fn birth_of(etag: &str) -> i64 {
  let digest = digest_of(BIRTH_DOMAIN, etag.as_bytes());
  match i64::from_be_bytes(digest) {
    0 => 1,
    birth => birth,
  }
}

/// The first eight bytes of the SHA-256 of `bytes` in `domain`.
fn digest_of(domain: &[u8], bytes: &[u8]) -> [u8; size_of::<u64>()] {
  let mut context = digest::Context::new(&digest::SHA256);
  context.update(domain);
  context.update(bytes);
  let digest = context.finish();
  let mut word = [0u8; size_of::<u64>()];
  for (to, from) in word.iter_mut().zip(digest.as_ref()) {
    *to = *from;
  }
  word
}

/// The bytes a listed object holds in the host: its key, ETag and time, and the fixed part of its entry.
fn listed_bytes(listed: &Listed) -> u64 {
  string_bytes(&listed.key)
    .saturating_add(string_bytes(&listed.etag))
    .saturating_add(string_bytes(&listed.last_modified))
    .saturating_add(u64::try_from(size_of::<Listed>() + size_of::<BaseEntry>()).unwrap_or(u64::MAX))
}

/// The bytes a string holds.
fn string_bytes(text: &str) -> u64 {
  u64::try_from(text.len()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
  use std::cell::Cell;

  use proptest::prelude::*;

  use super::*;
  use crate::s3::client::Style;
  use crate::s3::sim::{Credential, SimS3};

  /// Shape: a test's chunk, 4 KiB, so a file of a few kilobytes spans chunks.
  const CHUNK: u64 = 4_096;
  /// Shape: the round trip each landing is measured at, 5 ms, a near store's.
  const ROUND_TRIP_NS: u64 = 5_000_000;
  /// Shape: the base's prefix in the tests' bucket.
  const PREFIX: &str = "base/";
  /// Shape: how many times a test asks a call before it calls the host stuck: far past what any call here needs (a
  /// listing of three pages and a run of chunks).
  const ROUNDS: usize = 16;
  /// Shape: the clock's first reading in each test, 2023-11-14T22:13:20Z.
  const START_NS: i64 = 1_700_000_000_000_000_000;

  thread_local! {
    /// The test's clock, read by the store and the host alike; each test sets it first.
    static NOW: Cell<i64> = const { Cell::new(START_NS) };
  }

  /// The test's clock.
  fn clock() -> i64 {
    NOW.with(Cell::get)
  }

  /// Moves the test's clock on by `ns`.
  fn advance(ns: u64) {
    NOW.with(|now| now.set(now.get() + i64::try_from(ns).unwrap()));
  }

  /// A host of the tests' base, its cache `cache_chunks` chunks.
  fn host_over(cache_chunks: u64) -> S3Host {
    NOW.with(|now| now.set(START_NS));
    let base = S3Base::new(
      "http://127.0.0.1:9000",
      "garage",
      "bucket",
      PREFIX,
      Style::Path,
    )
    .unwrap();
    S3Host::new(
      &base,
      HostConfig {
        chunk_bytes: CHUNK,
        cache_bytes: cache_chunks * CHUNK,
        owner: (501, 20),
        wall_ns: clock,
      },
    )
    .unwrap()
  }

  /// Runs every fetch the host wants against `store`, landings that ask for more included, each measured at
  /// [`ROUND_TRIP_NS`].
  fn drive(host: &mut S3Host, store: &mut SimS3) {
    while let Some((ticket, fetch)) = host.next_fetch() {
      let answer = store.answer(&fetch);
      host.landed(ticket, answer, ROUND_TRIP_NS);
    }
  }

  /// Asks `call` of the host until it answers other than pending, driving its fetches between, as a parked request is
  /// run again when its fetch lands.
  fn settle<T>(
    host: &mut S3Host,
    store: &mut SimS3,
    mut call: impl FnMut(&mut S3Host) -> Result<T, HostError>,
  ) -> Result<T, HostError> {
    for _ in 0..ROUNDS {
      match call(host) {
        Err(HostError::Pending) => drive(host, store),
        answered => return answered,
      }
    }
    panic!("the call was still pending after {ROUNDS} rounds of fetches");
  }

  /// The names and kinds of the directory `dir`'s entries, settled.
  fn names(host: &mut S3Host, store: &mut SimS3, dir: HostDir) -> Vec<(String, HostKind)> {
    settle(host, store, |host| host.list(dir))
      .unwrap()
      .into_iter()
      .map(|entry| (entry.name.to_string(), entry.kind))
      .collect()
  }

  /// The whole of `file`'s bytes, read through the host a call at a time, settled.
  fn read_whole(
    host: &mut S3Host,
    store: &mut SimS3,
    file: HostFile,
  ) -> Result<Vec<u8>, HostError> {
    let size = host.fstat(file)?.size;
    let mut bytes = vec![0u8; usize::try_from(size).unwrap()];
    let mut done = 0;
    while done < bytes.len() {
      let at = u64::try_from(done).unwrap();
      let n = settle(host, store, |host| {
        host.read_at(file, at, &mut bytes[done..])
      })?;
      assert!(n > 0, "a read inside the file answers bytes");
      done += n;
    }
    Ok(bytes)
  }

  /// Bytes no two offsets of which repeat within a chunk, so a misplaced chunk shows.
  fn pattern(len: u64, seed: u8) -> Vec<u8> {
    (0..len)
      .map(|at| u8::try_from((at * 7 + u64::from(seed)) % 251).unwrap())
      .collect()
  }

  /// A-524 (§4.15 "An S3 base": a directory is its delimited listing): do put files, a nested directory, a directory
  /// marker and a key whose component is `..` under the base's prefix, listings answered two keys a page, and list the
  /// root and a subdirectory. Expect the root's files and directories by name and kind across its truncated pages, the
  /// `..` key refused by its kind and counted, never listed; the subdirectory's own entries; a file's fingerprint as
  /// the module states it; and the first listing of each a miss.
  #[test]
  fn a_directory_is_its_keys_listed_across_truncated_pages() {
    let mut host = host_over(64);
    let mut store = SimS3::new(false, clock);
    store.set_page_keys(2);
    for key in [
      "base/a.txt",
      "base/b/c.txt",
      "base/b/d/e.txt",
      "base/f.txt",
      "base/g/",
      "base/../x",
    ] {
      store.put(key, key.as_bytes());
    }
    let root = host.root();
    assert_eq!(
      host.list(root),
      Err(HostError::Pending),
      "the first listing is a miss"
    );
    assert_eq!(
      names(&mut host, &mut store, root),
      [
        ("a.txt".to_owned(), HostKind::File),
        ("b".to_owned(), HostKind::Dir),
        ("f.txt".to_owned(), HostKind::File),
        ("g".to_owned(), HostKind::Dir),
      ]
    );
    assert_eq!(
      host.refused(""),
      [(
        "base/../".to_owned(),
        EntryRefused::Name(keys::Refused::DotComponent)
      )]
    );
    let b = settle(&mut host, &mut store, |host| host.open_dir(root, "b")).unwrap();
    assert_eq!(
      names(&mut host, &mut store, b),
      [
        ("c.txt".to_owned(), HostKind::File),
        ("d".to_owned(), HostKind::Dir)
      ]
    );
    let file = settle(&mut host, &mut store, |host| host.open_file(root, "a.txt")).unwrap();
    let fingerprint = host.fstat(file).unwrap();
    assert_eq!(fingerprint.size, 10);
    assert_eq!(
      (fingerprint.mode, fingerprint.uid, fingerprint.gid),
      (FILE_MODE, 501, 20)
    );
    assert_eq!(fingerprint.mtime_ns, START_NS);
    assert_ne!(fingerprint.birth_ns, 0);
    assert_eq!(host.open_file(root, "b"), Err(HostError::NotFile));
    assert_eq!(host.open_dir(root, "a.txt"), Err(HostError::NotDirectory));
    assert_eq!(host.open_file(root, "missing"), Err(HostError::NotFound));
    let counts = host.counts();
    assert!(counts.pages >= 4 && counts.misses >= 2, "{counts:?}");
    assert_eq!(counts.refused_entries, 1);
  }

  /// A-524 (a miss is parked; a listing serves for the path's round trip): do list the root, and ask again before its
  /// fetch lands; land it; list within one measured round trip, then after it while an outsider adds a key, and again
  /// once that listing lands. Expect one fetch for the two misses; the landed listing to answer; no fetch within the
  /// round trip; the stale listing answered while the root is listed again, and the next call to see the new key with
  /// the directory's fingerprint moved.
  #[test]
  fn a_miss_is_parked_until_its_fetch_lands_and_a_listing_serves_one_round_trip() {
    let mut host = host_over(64);
    let mut store = SimS3::new(false, clock);
    store.put("base/a", b"a");
    let root = host.root();
    assert_eq!(host.list(root), Err(HostError::Pending));
    assert_eq!(host.fingerprint_dir(root), Err(HostError::Pending));
    let (ticket, fetch) = host.next_fetch().unwrap();
    assert_eq!(
      fetch,
      Fetch::List {
        prefix: PREFIX.to_owned(),
        token: None
      }
    );
    assert_eq!(host.next_fetch(), None, "two misses, one fetch");
    host.landed(ticket, store.answer(&fetch), ROUND_TRIP_NS);
    let before = host.fingerprint_dir(root).unwrap();
    advance(ROUND_TRIP_NS / 2);
    assert_eq!(host.list(root).unwrap().len(), 1);
    assert_eq!(
      host.next_fetch(),
      None,
      "a listing within its round trip answers alone"
    );
    advance(ROUND_TRIP_NS * 2);
    store.put("base/b", b"b");
    assert_eq!(
      host.list(root).unwrap().len(),
      1,
      "the stale listing answers"
    );
    drive(&mut host, &mut store);
    assert_eq!(host.list(root).unwrap().len(), 2, "the relisting landed");
    assert_ne!(host.fingerprint_dir(root).unwrap(), before);
    assert_eq!(host.counts().relists, 1);
  }

  /// A-524 (a file is bound to its object; ranges are chunks): do open a file of two and a half chunks and read across
  /// its first chunk's end, then the same bytes again, then its tail. Expect the first read a miss whose fetch is the
  /// run of the two chunks it needs bound by `If-Match` to the listed ETag; the bytes as put; the second read a hit
  /// with no fetch; and the whole file as put.
  #[test]
  fn a_read_fetches_the_run_of_chunks_it_needs_bound_to_its_objects_etag() {
    let mut host = host_over(64);
    let mut store = SimS3::new(false, clock);
    let bytes = pattern(CHUNK * 5 / 2, 3);
    let etag = store.put("base/big.bin", &bytes);
    let root = host.root();
    let file = settle(&mut host, &mut store, |host| {
      host.open_file(root, "big.bin")
    })
    .unwrap();
    let mut buf = [0u8; 20];
    let at = CHUNK - 10;
    assert_eq!(host.read_at(file, at, &mut buf), Err(HostError::Pending));
    let (ticket, fetch) = host.next_fetch().unwrap();
    assert_eq!(
      fetch,
      Fetch::Range {
        key: "base/big.bin".to_owned(),
        first: 0,
        len: 2 * CHUNK,
        etag,
        version: None
      }
    );
    host.landed(ticket, store.answer(&fetch), ROUND_TRIP_NS);
    assert_eq!(host.read_at(file, at, &mut buf), Ok(20));
    assert_eq!(&buf[..], &bytes[usize::try_from(at).unwrap()..][..20]);
    let hits = host.counts().hits;
    assert_eq!(host.read_at(file, at, &mut buf), Ok(20));
    assert_eq!(host.next_fetch(), None);
    assert_eq!(host.counts().hits, hits + 1);
    assert_eq!(read_whole(&mut host, &mut store, file).unwrap(), bytes);
    assert_eq!(
      host.read_at(file, CHUNK * 3, &mut buf),
      Ok(0),
      "past the end"
    );
  }

  /// A-524 (`412` under a bound read, A-151): do open a file of two chunks on an unversioned bucket and read its first
  /// chunk; let an outsider replace the object; read its second chunk. Expect the store's `412`, the handle stale ever
  /// after (never the new object's bytes under the old handle), its fingerprint the old object's; then, once the root
  /// is listed again, the file opened anew a new object (another birth) reading the replacement whole.
  #[test]
  fn an_object_replaced_under_a_bound_read_answers_412_and_its_handle_is_stale() {
    let mut host = host_over(64);
    let mut store = SimS3::new(false, clock);
    let old = pattern(CHUNK * 2, 1);
    let new = pattern(CHUNK * 2, 2);
    store.put("base/f.bin", &old);
    let root = host.root();
    let file = settle(&mut host, &mut store, |host| host.open_file(root, "f.bin")).unwrap();
    let was = host.fstat(file).unwrap();
    let mut first = vec![0u8; usize::try_from(CHUNK).unwrap()];
    // A read of the first chunk alone, so the second is fetched after the replacement.
    let mut done = 0;
    while done < first.len() {
      let at = u64::try_from(done).unwrap();
      done += settle(&mut host, &mut store, |host| {
        host.read_at(file, at, &mut first[done..])
      })
      .unwrap();
    }
    assert_eq!(first, old[..first.len()]);
    store.put("base/f.bin", &new);
    let mut second = vec![0u8; usize::try_from(CHUNK).unwrap()];
    assert_eq!(
      settle(&mut host, &mut store, |host| host.read_at(
        file,
        CHUNK,
        &mut second
      )),
      Err(HostError::StaleHandle)
    );
    assert_eq!(
      host.read_at(file, 0, &mut second),
      Err(HostError::StaleHandle)
    );
    assert_eq!(host.fstat(file).unwrap(), was);
    assert_eq!(host.counts().drifted, 1);
    advance(ROUND_TRIP_NS * 4);
    settle(&mut host, &mut store, |host| host.list(root)).unwrap();
    drive(&mut host, &mut store);
    let reopened = settle(&mut host, &mut store, |host| host.open_file(root, "f.bin")).unwrap();
    assert_ne!(host.fstat(reopened).unwrap().birth_ns, was.birth_ns);
    assert_eq!(read_whole(&mut host, &mut store, reopened).unwrap(), new);
  }

  /// A-524 (a versioned bucket's open handle reads its own version, as a disk's open descriptor reads its own inode):
  /// do open a file of two chunks on a versioned bucket, read its first chunk, let an outsider replace the object, and
  /// read the rest. Expect the second chunk's fetch bound to the version the first landing named, and the whole file
  /// the old object's bytes, nothing drifted.
  #[test]
  fn a_versioned_objects_open_handle_reads_its_own_version_whatever_replaces_it() {
    let mut host = host_over(1);
    let mut store = SimS3::new(true, clock);
    let old = pattern(CHUNK * 2, 5);
    store.put("base/v.bin", &old);
    let root = host.root();
    let file = settle(&mut host, &mut store, |host| host.open_file(root, "v.bin")).unwrap();
    let mut chunk = vec![0u8; usize::try_from(CHUNK).unwrap()];
    assert_eq!(
      settle(&mut host, &mut store, |host| host
        .read_at(file, 0, &mut chunk)),
      Ok(chunk.len())
    );
    store.put("base/v.bin", &pattern(CHUNK * 2, 6));
    assert_eq!(
      host.read_at(file, CHUNK, &mut chunk),
      Err(HostError::Pending)
    );
    let (ticket, fetch) = host.next_fetch().unwrap();
    let Fetch::Range { version, .. } = &fetch else {
      panic!("a range");
    };
    assert!(version.is_some(), "bound to the version the store named");
    host.landed(ticket, store.answer(&fetch), ROUND_TRIP_NS);
    assert_eq!(read_whole(&mut host, &mut store, file).unwrap(), old);
    assert_eq!(host.counts().drifted, 0);
  }

  /// A-524 (a deleted object is a missing entry): do open a file, let an outsider delete its object, read it, and list
  /// the root again. Expect the read stale (`404` under its binding), and the file gone from the root's listing and
  /// `NotFound` to open.
  #[test]
  fn a_deleted_object_leaves_its_handle_stale_and_its_name_missing() {
    let mut host = host_over(64);
    let mut store = SimS3::new(false, clock);
    store.put("base/gone", b"soon gone");
    let root = host.root();
    let file = settle(&mut host, &mut store, |host| host.open_file(root, "gone")).unwrap();
    store.delete("base/gone");
    let mut buf = [0u8; 4];
    assert_eq!(
      settle(&mut host, &mut store, |host| host
        .read_at(file, 0, &mut buf)),
      Err(HostError::StaleHandle)
    );
    advance(ROUND_TRIP_NS * 4);
    settle(&mut host, &mut store, |host| host.list(root)).unwrap();
    drive(&mut host, &mut store);
    assert!(names(&mut host, &mut store, root).is_empty());
    assert_eq!(
      settle(&mut host, &mut store, |host| host.open_file(root, "gone")),
      Err(HostError::NotFound)
    );
  }

  /// A-524 (typed refusals that answer one round trip): do list the root while the store refuses an expired
  /// credential, ask again within the round trip, then with a fresh credential within it and after it (the path's
  /// round trip is RFC 9002's initial one while no answer has measured it). Expect the parked call answered `EACCES`,
  /// the second answered it with no fetch, the third still it, and the fourth asking again and answered the listing.
  #[test]
  fn a_refused_listing_answers_its_round_trip_and_is_asked_again_after() {
    let mut host = host_over(64);
    let mut store = SimS3::new(false, clock);
    store.put("base/a", b"a");
    store.set_credential(Credential::Expired);
    let root = host.root();
    assert_eq!(
      settle(&mut host, &mut store, |host| host.list(root)),
      Err(HostError::Unavailable(EACCES))
    );
    assert_eq!(host.list(root), Err(HostError::Unavailable(EACCES)));
    assert_eq!(host.next_fetch(), None);
    store.set_credential(Credential::Valid);
    advance(ROUND_TRIP_NS);
    assert_eq!(host.list(root), Err(HostError::Unavailable(EACCES)));
    advance(slates_transport::rtt::INITIAL_PTO_NS);
    assert_eq!(
      names(&mut host, &mut store, root),
      [("a".to_owned(), HostKind::File)]
    );
    assert_eq!(host.counts().failures, 1);
  }

  /// A-524 (the cache holds its budget): do read four chunks of a file one at a time through a cache of two, then the
  /// first again. Expect the cache never past two chunks, the least recently used shed, and the first chunk fetched
  /// again.
  #[test]
  fn the_cache_holds_its_budget_shedding_the_least_recently_used() {
    let mut host = host_over(2);
    let mut store = SimS3::new(false, clock);
    let bytes = pattern(CHUNK * 4, 9);
    store.put("base/four", &bytes);
    let root = host.root();
    let file = settle(&mut host, &mut store, |host| host.open_file(root, "four")).unwrap();
    let mut chunk = vec![0u8; usize::try_from(CHUNK).unwrap()];
    for index in 0..4 {
      assert_eq!(
        settle(&mut host, &mut store, |host| host.read_at(
          file,
          index * CHUNK,
          &mut chunk
        )),
        Ok(chunk.len())
      );
      assert_eq!(
        chunk,
        bytes[usize::try_from(index * CHUNK).unwrap()..][..chunk.len()]
      );
      assert!(host.cached_bytes() <= 2 * CHUNK, "{}", host.cached_bytes());
    }
    let ranges = host.counts().ranges;
    assert_eq!(
      host.read_at(file, 0, &mut chunk),
      Err(HostError::Pending),
      "the first chunk was shed"
    );
    drive(&mut host, &mut store);
    assert_eq!(host.counts().ranges, ranges + 1);
    assert!(host.counts().shed_bytes >= 2 * CHUNK);
  }

  /// One outsider's step, or a look by the host.
  #[derive(Clone, Debug)]
  enum Step {
    /// Puts `len` bytes seeded `seed` at the key `which` names.
    Put { which: usize, len: u64, seed: u8 },
    /// Deletes the key `which` names.
    Delete { which: usize },
    /// The host lists every directory and reads every file after the clock passes a round trip.
    Look,
  }

  /// Shape: the keys the oracle's outsider writes: files at the root and nested, and a directory marker.
  const KEYS: [&str; 6] = [
    "base/a",
    "base/b",
    "base/d/c",
    "base/d/e",
    "base/d/f/g",
    "base/h/",
  ];

  /// A step, generated as a plain tuple so the strategy needs no `Arc`-backed `prop_oneof!` (R2).
  fn step() -> impl Strategy<Value = Step> {
    (0u8..4, 0..KEYS.len(), 0..CHUNK * 3, any::<u8>()).prop_map(|(kind, which, len, seed)| {
      match kind {
        0 | 1 => Step::Put { which, len, seed },
        2 => Step::Delete { which },
        _ => Step::Look,
      }
    })
  }

  /// The host's whole tree after a round trip has passed, each directory listed again: every directory's entries by
  /// path, and every file's bytes by path.
  fn look(
    host: &mut S3Host,
    store: &mut SimS3,
  ) -> (Vec<(String, HostKind)>, Vec<(String, Vec<u8>)>) {
    advance(ROUND_TRIP_NS * 4);
    let mut entries = Vec::new();
    let mut files = Vec::new();
    let mut pending = vec![(host.root(), String::new())];
    while let Some((dir, path)) = pending.pop() {
      settle(host, store, |host| host.list(dir)).unwrap();
      drive(host, store);
      for (name, kind) in names(host, store, dir) {
        let full = join(&path, &name);
        entries.push((full.clone(), kind));
        match kind {
          HostKind::Dir => {
            let opened = settle(host, store, |host| host.open_dir(dir, &name)).unwrap();
            pending.push((opened, full));
          }
          _ => {
            let file = settle(host, store, |host| host.open_file(dir, &name)).unwrap();
            files.push((full, read_whole(host, store, file).unwrap()));
            host.close_file(file);
          }
        }
      }
    }
    entries.sort();
    files.sort();
    (entries, files)
  }

  /// The tree the bucket holds now, by the keys' own mapping: every directory and file by path, and every file's bytes.
  fn model(store: &SimS3) -> (Vec<(String, HostKind)>, Vec<(String, Vec<u8>)>) {
    let tree = keys::tree_of(PREFIX, &store.keys());
    let mut entries: Vec<(String, HostKind)> = tree
      .directories
      .iter()
      .map(|path| (path.clone(), HostKind::Dir))
      .chain(tree.files.keys().map(|path| (path.clone(), HostKind::File)))
      .collect();
    entries.sort();
    let files = tree
      .files
      .iter()
      .map(|(path, listed)| (path.clone(), store.current(&listed.key).unwrap().to_vec()))
      .collect();
    (entries, files)
  }

  proptest! {
    #![proptest_config(slates_test_seeds::unseeded(proptest::test_runner::Config::default()))]

    /// A-524 (the oracle: `SimS3` against the bucket's own tree): do let an outsider put and delete objects at root
    /// and nested keys and a directory marker, of up to three chunks, through a cache of two, and look at the whole
    /// tree through the host after each round trip. Expect every look to show exactly the tree the bucket's keys map to
    /// (`keys::tree_of`), every file's bytes the object's current ones, whatever the order; and the fetches, pages,
    /// relistings and sheds the looks needed to have happened.
    #[test]
    fn the_host_shows_the_tree_the_bucket_holds_whatever_outsiders_do(
      steps in proptest::collection::vec(step(), 1..24),
      versioned in any::<bool>(),
    ) {
      let mut host = host_over(2);
      let mut store = SimS3::new(versioned, clock);
      let mut looked = 0;
      for step in steps.iter().chain(std::iter::once(&Step::Look)) {
        match step {
          Step::Put { which, len, seed } => {
            store.put(KEYS[*which], &pattern(if KEYS[*which].ends_with('/') { 0 } else { *len }, *seed));
          }
          Step::Delete { which } => store.delete(KEYS[*which]),
          Step::Look => {
            prop_assert_eq!(look(&mut host, &mut store), model(&store));
            looked += 1;
          }
        }
        advance(1);
      }
      prop_assert!(looked >= 1);
      prop_assert!(host.counts().fetches >= 1 && host.counts().pages >= 1);
    }
  }
}
