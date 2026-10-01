//! Shared fixtures of the landing tests: a store over one RAM region, volume configuration,
//! the grant-flow session, and the overlay edits the scenarios are built from. Every test crate
//! includes this module with `mod common;`.

#![allow(dead_code)]

pub(crate) mod removal;

use slates_land::engine::{
  Audit, HostSlot, LandingRefusal, LandingReport, LandingRequest, LandingTarget, Observer,
  Presented, Unobserved, begin_landing, land, settle_grant,
};
use slates_land::grant::{GrantScope, Grants, Leases, Surface};
use slates_land::manifest::Filter;
use slates_mem::arena::ChunkArena;
use slates_mem::handle::Handle;
use slates_mem::region::Region;
use slates_vfs::clock::StepClock;
use slates_vfs::dir::{Child, DirNode};
use slates_vfs::host::{HostFs, LandFs};
use slates_vfs::inode::Kind;
use slates_vfs::names::NameEquivalence;
use slates_vfs::quota::Quota;
use slates_vfs::volume::{Store, StoreConfig, Volume, VolumeConfig};

/// Shape: how far the simulated clock moves between building a base tree and mounting it. A disk tree
/// predates its mount, so its last change is older than the timestamp granularity and its listing is
/// trusted once read; a mount in the same tick as the change re-lists on every use (the listing racy rule,
/// §4.5). One second passes any granularity these tests set.
pub(crate) const SETTLED_NS: i64 = 1_000_000_000;
/// Format: the page size the tests use (the store's granule).
pub(crate) const PAGE: usize = 4096;
/// Shape: pages in the test region (16 MiB of content).
pub(crate) const REGION_PAGES: usize = 4096;
/// Shape: the large-file class boundary, one chunk window.
pub(crate) const LARGE: u64 = 65_536;
/// Shape: the lease and grant terms these tests use (monotonic ns); no test crosses them.
pub(crate) const TERM_NS: u64 = 1_000_000_000_000;
/// Shape: a mode for created files.
pub(crate) const FILE_MODE: u32 = 0o100_644;
/// Shape: a mode for created directories.
pub(crate) const DIR_MODE: u32 = 0o040_755;

pub(crate) fn store() -> Store {
  let mut arena = ChunkArena::new(PAGE);
  arena
    .add_region(Region::map(PAGE * REGION_PAGES, PAGE, false).unwrap())
    .unwrap();
  Store::new(
    &StoreConfig {
      page: PAGE,
      cache_line: 64,
      max_dirs: 1 << 17,
      max_inodes: 1 << 17,
      max_chunks: 1 << 16,
      max_dir_blocks: 1 << 16,
      dir_cutover: 4,
    },
    arena,
    0,
  )
}

pub(crate) fn config() -> VolumeConfig {
  VolumeConfig {
    prefix: 7,
    names: NameEquivalence::Exact,
    quota: Quota::Bounded { limit: 1 << 30 },
    journal_bytes: 1 << 20,
    clock: Box::new(StepClock::new(1_000_000, 1_000)),
  }
}

pub(crate) fn scratch(store: &mut Store) -> Volume {
  Volume::create(store, config()).unwrap()
}

/// Shape: the consumer these tests land as (its principal identity bytes).
pub(crate) const CONSUMER: &[u8] = b"consumer-one";
/// Shape: the id of the volume these tests land.
pub(crate) const VOLUME: [u8; 16] = [7; 16];
/// Shape: the snapshot these tests land.
pub(crate) const SNAPSHOT: u64 = 1;

/// Shape: the holder the harness's landings take the target lease as.
pub(crate) const LEASE_HOLDER: u64 = 1;

/// The key `target`'s landing lease is held under: its canonical identity, as the engine checks it.
pub(crate) fn target_lease_key<H: HostFs>(host: &mut H, target: &LandingTarget) -> String {
  let identity = host.fingerprint_dir(target.dir).unwrap();
  slates_land::grant::lease_key(&slates_land::grant::TargetIdentity {
    key: target.key.clone(),
    device: identity.dev,
    inode: identity.ino,
  })
}

pub(crate) fn request(landing_id: u64) -> LandingRequest {
  LandingRequest {
    landing_id,
    consumer: CONSUMER.into(),
    volume: VOLUME,
    snapshot: SNAPSHOT,
    source: slates_land::source::Source::Head,
    grant: None,
    filter: Filter::default(),
    now_ns: 1,
    media_durability: false,
    large_class_bytes: LARGE,
    cores: 2,
    max_depth: 8,
    variance_permille: 100,
  }
}

/// The grants, leases and audit log of one session.
pub(crate) struct Session {
  pub(crate) grants: Grants,
  pub(crate) leases: Leases,
  pub(crate) audit: Audit,
  /// The budget of each slice a granted landing is stepped in: unbounded (`u64::MAX`) for the one-call
  /// landing, `0` for one unit per slice (AUD-29-25: the sliced run must land exactly as the one call does).
  pub(crate) slice_budget_ns: u64,
  /// The slices the session's sliced landings took (the sliced oracles' non-vacuity counter).
  pub(crate) slices: u64,
}

impl Default for Session {
  fn default() -> Self {
    Self::new()
  }
}

impl Session {
  pub(crate) fn new() -> Self {
    Self {
      grants: Grants::default(),
      leases: Leases::default(),
      audit: Audit::new(1 << 10),
      slice_budget_ns: u64::MAX,
      slices: 0,
    }
  }

  /// A session whose granted landings run one unit per slice.
  pub(crate) fn sliced() -> Self {
    Self {
      slice_budget_ns: 0,
      ..Self::new()
    }
  }
}

/// Everything one landing call needs.
pub(crate) struct Setup<'a, H: LandFs> {
  pub(crate) host: &'a mut H,
  pub(crate) target: &'a LandingTarget,
  pub(crate) vol: &'a mut Volume,
  pub(crate) store: &'a mut Store,
  pub(crate) session: &'a mut Session,
}

impl<H: LandFs> Setup<'_, H> {
  pub(crate) fn try_land<O: Observer<H>>(
    &mut self,
    req: &LandingRequest,
    observer: &mut O,
  ) -> Result<LandingReport, LandingRefusal> {
    // A granted landing holds the target's landing lease under its canonical identity, taken for the landing
    // and released after, as the server takes it from the control shard (AUD-29-03). A presentation writes
    // nothing and holds none.
    let lease = match req.grant {
      Some(_) => {
        let key = target_lease_key(self.host, self.target);
        Some(
          self
            .session
            .leases
            .take(&key, LEASE_HOLDER, req.now_ns, TERM_NS)
            .map_err(LandingRefusal::LeaseHeld)?,
        )
      }
      None => None,
    };
    let result = if self.session.slice_budget_ns == u64::MAX {
      land(
        self.host,
        self.target,
        self.vol,
        self.store,
        &mut self.session.grants,
        lease.as_ref(),
        &mut self.session.audit,
        req,
        observer,
      )
    } else {
      self.land_in_slices(req, lease.as_ref(), observer)
    };
    if let Some(held) = &lease {
      self.session.leases.release(held);
    }
    result
  }

  /// The landing stepped in slices of the session's budget, counting them.
  fn land_in_slices<O: Observer<H>>(
    &mut self,
    req: &LandingRequest,
    lease: Option<&slates_land::grant::LandingLease>,
    observer: &mut O,
  ) -> Result<LandingReport, LandingRefusal> {
    let mut run = begin_landing(
      HostSlot::Lent(&mut *self.host),
      self.target,
      self.vol,
      self.store,
      &mut self.session.grants,
      lease,
      &mut self.session.audit,
      req,
    )
    .map_err(|(_, refusal)| refusal)?;
    let result = loop {
      self.session.slices += 1;
      if let Some(ended) = run.step(
        self.vol,
        self.store,
        &mut self.session.audit,
        observer,
        self.session.slice_budget_ns,
      ) {
        break ended;
      }
      if run.ready_to_finish() {
        break run.finish(self.vol, self.store, &mut self.session.audit, observer);
      }
    };
    settle_grant(&mut self.session.grants, run.grant(), &result);
    result
  }

  /// Presents: the `GrantRequired` reply.
  pub(crate) fn present(&mut self, req: &LandingRequest) -> Box<Presented> {
    match self.try_land(req, &mut Unobserved) {
      Err(LandingRefusal::GrantRequired(p)) => p,
      other => panic!("expected GrantRequired, got {other:?}"),
    }
  }

  /// Presents, grants the presented hash once, lands with `observer` before each write.
  pub(crate) fn land_with<O: Observer<H>>(
    &mut self,
    mut req: LandingRequest,
    observer: &mut O,
  ) -> Result<LandingReport, LandingRefusal> {
    let presented = self.present(&req);
    req.grant = self.session.grants.issue(
      Surface::Cli,
      presented.manifest.hash,
      presented.binding.clone(),
      GrantScope::Once,
      req.now_ns,
      TERM_NS,
    );
    self.try_land(&req, observer)
  }

  pub(crate) fn land(&mut self, req: LandingRequest) -> Result<LandingReport, LandingRefusal> {
    self.land_with(req, &mut Unobserved)
  }
}

// ---------------------------------------------------------------- overlay edits

pub(crate) fn split(path: &str) -> (&str, &str) {
  match path.rfind('/') {
    Some(0) => ("/", &path[1..]),
    Some(i) => (&path[..i], &path[i + 1..]),
    None => ("/", path),
  }
}

pub(crate) fn dir_of<H: HostFs>(
  vol: &mut Volume,
  host: &mut H,
  store: &mut Store,
  path: &str,
) -> Handle<DirNode> {
  let located = vol.with_host(host).resolve(store, path).unwrap();
  match located.child {
    Child::Dir(h) => h,
    other => panic!("{path} is {other:?}"),
  }
}

pub(crate) fn write_file<H: HostFs>(
  vol: &mut Volume,
  host: &mut H,
  store: &mut Store,
  path: &str,
  bytes: &[u8],
) {
  let (dir, name) = split(path);
  let d = dir_of(vol, host, store, dir);
  let mut o = vol.with_host(host);
  let no = match o.lookup(store, d, name) {
    Ok(l) => l.inode,
    Err(_) => o.create_file(store, d, name, FILE_MODE).unwrap(),
  };
  o.truncate(store, no, 0).unwrap();
  if !bytes.is_empty() {
    o.write(store, no, 0, bytes).unwrap();
  }
}

pub(crate) fn mkdir<H: HostFs>(vol: &mut Volume, host: &mut H, store: &mut Store, path: &str) {
  let (dir, name) = split(path);
  let d = dir_of(vol, host, store, dir);
  vol.with_host(host).mkdir(store, d, name, DIR_MODE).unwrap();
}

pub(crate) fn unlink<H: HostFs>(vol: &mut Volume, host: &mut H, store: &mut Store, path: &str) {
  let (dir, name) = split(path);
  let d = dir_of(vol, host, store, dir);
  vol.with_host(host).unlink(store, d, name).unwrap();
}

pub(crate) fn rmdir<H: HostFs>(vol: &mut Volume, host: &mut H, store: &mut Store, path: &str) {
  let (dir, name) = split(path);
  let d = dir_of(vol, host, store, dir);
  vol.with_host(host).rmdir(store, d, name).unwrap();
}

pub(crate) fn rename<H: HostFs>(
  vol: &mut Volume,
  host: &mut H,
  store: &mut Store,
  from: &str,
  to: &str,
) {
  let (fd, fname) = split(from);
  let (td, tname) = split(to);
  let f = dir_of(vol, host, store, fd);
  let t = dir_of(vol, host, store, td);
  vol
    .with_host(host)
    .rename(store, f, fname, t, tname)
    .unwrap();
}

pub(crate) fn symlink<H: HostFs>(
  vol: &mut Volume,
  host: &mut H,
  store: &mut Store,
  path: &str,
  target: &str,
) {
  let (dir, name) = split(path);
  let d = dir_of(vol, host, store, dir);
  vol.with_host(host).symlink(store, d, name, target).unwrap();
}

/// Removes a tree through the overlay the way `rm -r` does: files first, directories bottom-up.
pub(crate) fn rm_r<H: HostFs>(vol: &mut Volume, host: &mut H, store: &mut Store, path: &str) {
  let d = dir_of(vol, host, store, path);
  let entries: Vec<(String, bool)> = vol
    .with_host(host)
    .readdir(store, d)
    .unwrap()
    .iter()
    .map(|row| (row.name.to_string(), row.kind == Kind::Dir))
    .collect();
  for (name, is_dir) in entries {
    let child = format!("{}/{name}", path.trim_end_matches('/'));
    if is_dir {
      rm_r(vol, host, store, &child);
    } else {
      unlink(vol, host, store, &child);
    }
  }
  rmdir(vol, host, store, path);
}

pub(crate) fn read_through<H: HostFs>(
  vol: &mut Volume,
  host: &mut H,
  store: &mut Store,
  path: &str,
) -> Vec<u8> {
  let located = vol.with_host(host).resolve(store, path).unwrap();
  let mut buf = [0u8; 64];
  let n = vol
    .with_host(host)
    .read(store, located.inode, 0, &mut buf)
    .unwrap();
  buf[..n].to_vec()
}
