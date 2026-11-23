//! A daemon's allocation by site while it serves stock Mercurial (goal 93, the 410 MiB bar; main 01a20182's order of
//! 2026-11-23, (A)): its own binary, so the slates binary's allocator ([`LargeMapped`]) is the global one, wrapped in a
//! byte sampler that runs only while a row runs. An in-process daemon (a changeset store, the cleartext HTTP edge on
//! loopback, an HTTP token its issuer enrolled) is driven by real `hg` subprocesses, so what is sampled is the daemon's
//! alone: a push of a `hg debugbuilddag` history, a clone, a stream clone, and the clone verified. Each served row's
//! top sites are printed with their bytes and bytes a changeset, the figures the churn's cuts are ordered by; the test
//! holds each row to its success and its sampler to having counted. Where `hg` is absent it skips, and says so.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::print_stdout,
  clippy::print_stderr,
  clippy::disallowed_methods,
  clippy::disallowed_types,
  clippy::indexing_slicing,
  clippy::arithmetic_side_effects,
  clippy::cast_precision_loss
)]

mod common;

use std::alloc::{GlobalAlloc, Layout};
use std::collections::HashMap;
use std::process::Command;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use slates_client::RepositorySpec;
use slates_ipc::protocol::{CredentialMaterial, ObjectFormat, Principal, VcsTool};
use slates_land::store_root::{StoreGrant, StoreRoot};
use slates_mem::large::LargeMapped;
use slates_server::config::VcsHttpConfig;
use slates_server::{Daemon, DaemonConfig};
use slates_store::owner::StoreOwner;
use slates_store::store::Sizing;

use common::anchor::{held_anchor, source_of};
use common::client::connect;
use common::target::target_dir;

/// Shape: the changesets the pushed history holds, each a new file and the overwritten file rewritten: enough for the
/// push's and the clones' per-changeset work to show above their fixed costs.
const CHANGESETS: u64 = 400;
/// Shape: the bytes between samples.
const SAMPLE: u64 = 256 << 10;
/// Shape: the sites each row prints.
const LISTED: usize = 5;
/// Shape: the daemon's shards.
const SHARDS: u16 = 2;
/// Shape: the store's sizing, a test's (as `slates_server`'s own store tests size theirs).
const SIZING: Sizing = Sizing {
  memory: 8 << 20,
  disk: 1 << 30,
  extent_bytes: 128 << 10,
  fanout: 8,
};
/// Shape: the store owner's waiting requests.
const WAITING: usize = 64;

/// Whether the sampler counts: only while a row runs.
static ON: AtomicBool = AtomicBool::new(false);
/// The bytes allocated while on, and since the last sample.
static TOTAL: AtomicU64 = AtomicU64::new(0);
static SINCE: AtomicU64 = AtomicU64::new(0);
/// The sampled bytes by site.
static SITES: Mutex<Option<HashMap<String, u64>>> = Mutex::new(None);

std::thread_local! {
  /// Set while this thread samples: its own allocations are not sampled.
  static GUARD: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// A backtrace's site: its innermost two frames outside the allocator, the standard library and this test.
fn site(trace: &str) -> String {
  let skip = |name: &str| {
    [
      "std::", "core::", "alloc::", "<alloc::", "<core::", "<std::", "__rust", "__rdl",
    ]
    .iter()
    .any(|prefix| name.starts_with(prefix))
      || [
        "slates_mem::large",
        "hg_push_heap::",
        "backtrace",
        "GlobalAlloc",
      ]
      .iter()
      .any(|part| name.contains(part))
  };
  let frames: Vec<&str> = trace
    .lines()
    .map(str::trim)
    .filter_map(|line| {
      let (number, name) = line.split_once(": ")?;
      number.chars().all(|c| c.is_ascii_digit()).then_some(name)
    })
    .filter(|name| !skip(name))
    .take(2)
    .collect();
  let short = |name: &str| {
    name
      .rsplit_once("::h")
      .map_or(name, |(head, _)| head)
      .to_owned()
  };
  match frames.as_slice() {
    [inner, caller, ..] => format!("{} <- {}", short(inner), short(caller)),
    [inner] => short(inner),
    [] => "(no frame)".to_owned(),
  }
}

/// Notes `size` bytes allocated: sampled every [`SAMPLE`] while on.
fn note(size: usize) {
  if !ON.load(Ordering::Relaxed) {
    return;
  }
  let size = size as u64;
  TOTAL.fetch_add(size, Ordering::Relaxed);
  let since = SINCE.fetch_add(size, Ordering::Relaxed) + size;
  if since < SAMPLE || GUARD.with(|guard| guard.replace(true)) {
    return;
  }
  SINCE.store(0, Ordering::Relaxed);
  let key = site(&std::backtrace::Backtrace::force_capture().to_string());
  if let Ok(mut sites) = SITES.lock() {
    *sites
      .get_or_insert_with(HashMap::new)
      .entry(key)
      .or_insert(0) += since;
  }
  GUARD.with(|guard| guard.set(false));
}

/// `LargeMapped`, sampled.
struct Sampled;

// SAFETY: every call is `LargeMapped`'s, unchanged; the sampler only reads the layout's size before it.
unsafe impl GlobalAlloc for Sampled {
  unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
    note(layout.size());
    // SAFETY: as the caller's contract with `alloc`.
    unsafe { LargeMapped.alloc(layout) }
  }
  unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
    // SAFETY: as the caller's contract with `dealloc`.
    unsafe { LargeMapped.dealloc(ptr, layout) }
  }
  unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
    note(layout.size());
    // SAFETY: as the caller's contract with `alloc_zeroed`.
    unsafe { LargeMapped.alloc_zeroed(layout) }
  }
  unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
    note(new_size.saturating_sub(layout.size()));
    // SAFETY: as the caller's contract with `realloc`.
    unsafe { LargeMapped.realloc(ptr, layout, new_size) }
  }
}

#[global_allocator]
static ALLOCATOR: Sampled = Sampled;

/// One row's figures: what the daemon allocated while it ran, and its top sites.
struct Row {
  /// The row's name.
  name: &'static str,
  /// The bytes allocated.
  total: u64,
  /// The top sites, most bytes first.
  sites: Vec<(String, u64)>,
}

/// Runs `command` as row `name`, the sampler on for its length: its figures; panics where it fails.
fn row(name: &'static str, command: &mut Command) -> Row {
  if let Ok(mut sites) = SITES.lock() {
    *sites = None;
  }
  TOTAL.store(0, Ordering::Relaxed);
  SINCE.store(0, Ordering::Relaxed);
  ON.store(true, Ordering::Relaxed);
  let output = command.output().expect("hg ran");
  ON.store(false, Ordering::Relaxed);
  assert!(
    output.status.success(),
    "{name}: {}\n{}",
    String::from_utf8_lossy(&output.stdout),
    String::from_utf8_lossy(&output.stderr)
  );
  let mut sites: Vec<(String, u64)> = SITES
    .lock()
    .ok()
    .and_then(|sites| sites.clone())
    .unwrap_or_default()
    .into_iter()
    .collect();
  sites.sort_by(|left, right| right.1.cmp(&left.1));
  sites.truncate(LISTED);
  Row {
    name,
    total: TOTAL.load(Ordering::Relaxed),
    sites,
  }
}

/// Prints `row`'s figures, each a changeset of [`CHANGESETS`].
fn print(row: &Row) {
  println!(
    "hg_push_heap: {}: {} B allocated, {} B a changeset",
    row.name,
    row.total,
    row.total / CHANGESETS
  );
  for (name, bytes) in &row.sites {
    println!(
      "hg_push_heap: {}:   {:>5.1}% {:>9} B a changeset  {name}",
      row.name,
      *bytes as f64 * 100.0 / row.total.max(1) as f64,
      bytes / CHANGESETS
    );
  }
}

/// An `hg` command with this test's configuration (`HGRCPATH`), in `dir`.
fn hg(hgrc: &str, dir: &str) -> Command {
  let mut command = Command::new("hg");
  command
    .env("HGRCPATH", hgrc)
    .env("HGPLAIN", "1")
    .env("HGUSER", "slates-test")
    .current_dir(dir);
  command
}

/// Goal 93 (main's order of 2026-11-23, (A): the lasting regression test of a daemon's churn under stock Mercurial).
/// Do: start a two-shard daemon holding a changeset store, its HTTP edge on loopback; create a Mercurial repository and
/// enroll an HTTP token for this process's principal; build a history of [`CHANGESETS`] changesets with
/// `hg debugbuilddag`; push it, clone it, stream-clone it and verify the clone with stock `hg`, the daemon's allocation
/// sampled by site for each row. Expect every row to succeed, and the push, the clone and the stream clone each to
/// have allocated (the sampler counts); each row's top sites printed with their bytes a changeset. Skips, saying so,
/// where `hg` is not installed.
#[test]
fn a_daemons_allocation_by_site_under_stock_mercurial() {
  if Command::new("hg")
    .arg("version")
    .arg("-q")
    .output()
    .is_err()
  {
    eprintln!("SKIPPED a_daemons_allocation_by_site_under_stock_mercurial: no `hg` on PATH");
    return;
  }
  let scratch = target_dir();
  let base = scratch.path.clone();
  // The changeset store, its owner started over its own directory.
  let store_dir = format!("{base}/store");
  std::fs::create_dir_all(&store_dir).unwrap();
  let store_dir = std::fs::canonicalize(&store_dir).unwrap();
  let grant = StoreGrant {
    repository: 0,
    target: StoreRoot::identify(&store_dir).unwrap(),
    issued_ns: 0,
  };
  let owner = StoreOwner::start(
    StoreRoot::open(&store_dir, &grant).unwrap(),
    SIZING,
    WAITING,
  )
  .unwrap();
  // The daemon, its edge and its store.
  let profile = common::machine_profile();
  let instance = format!("hg-push-heap-{}", std::process::id());
  let mut config = DaemonConfig::derive(&profile, &instance, Some(SHARDS));
  config.vcs_http = Some(VcsHttpConfig {
    address: std::net::SocketAddrV4::new(std::net::Ipv4Addr::LOCALHOST, 0),
    identity: None,
  });
  config.vcs_disk = Some(owner.handle());
  let anchor = held_anchor(&instance, &profile, &config);
  let daemon = Daemon::start(&profile, config, source_of(&anchor)).unwrap();
  daemon.bootstrap(true).unwrap();
  let port = daemon.vcs_http_port().expect("the edge listens");
  // The repository, and a token for this process's principal.
  let mut client = connect(&instance);
  client
    .create_repository(&RepositorySpec {
      name: "project".to_owned(),
      tool: VcsTool::Mercurial,
      format: ObjectFormat::Sha1,
      default_branch: "branch:default:0".to_owned(),
      green: None,
    })
    .unwrap();
  let token = format!("hg-push-heap-token-{}", std::process::id());
  let principal = Principal::Uid {
    host: client.daemon_status().unwrap().anchor,
    uid: rustix::process::getuid().as_raw(),
  };
  let material = CredentialMaterial::HttpToken {
    id: slates_server::credentials::token_id(token.as_bytes()),
    cram_md5_contexts: slates_server::credentials::cram_md5_contexts(token.as_bytes()),
  };
  let secret = daemon.segment().issuer_secret().unwrap();
  let proof = slates_server::credentials::enroll_proof(&secret, &principal, &material, None);
  client
    .enroll_credential(principal, material, None, proof)
    .unwrap();
  // Stock hg's configuration: the token as Basic's password, as the hg matrix gives it.
  let url = format!("http://127.0.0.1:{port}/project");
  let hgrc = format!("{base}/hgrc");
  std::fs::write(
    &hgrc,
    format!(
      "[auth]\nslates.prefix = http://127.0.0.1:{port}/\nslates.username = slates\nslates.password = {token}\n"
    ),
  )
  .unwrap();
  let source = format!("{base}/source");
  let made = hg(&hgrc, &base).args(["init", &source]).output().unwrap();
  assert!(made.status.success(), "hg init");
  let built = hg(&hgrc, &source)
    .args([
      "debugbuilddag",
      "--new-file",
      "--overwritten-file",
      &format!("+{CHANGESETS}"),
    ])
    .output()
    .unwrap();
  assert!(
    built.status.success(),
    "{}",
    String::from_utf8_lossy(&built.stderr)
  );
  // The rows, each sampled.
  let push = row(
    "push",
    hg(&hgrc, &source).args(["push", "-q", "--new-branch", "-f", &url]),
  );
  let clone = row(
    "clone",
    hg(&hgrc, &base).args(["clone", "-q", &url, "cloned"]),
  );
  let stream = row(
    "stream-clone",
    hg(&hgrc, &base).args(["clone", "-q", "--stream", &url, "streamed"]),
  );
  let verified = hg(&hgrc, &format!("{base}/cloned"))
    .args(["verify", "-q"])
    .output()
    .unwrap();
  daemon.stop();
  drop(anchor);
  owner.stop().unwrap();
  for row in [&push, &clone, &stream] {
    print(row);
  }
  assert!(
    verified.status.success(),
    "the clone verifies: {}",
    String::from_utf8_lossy(&verified.stderr)
  );
  for row in [&push, &clone, &stream] {
    assert!(
      row.total > 0 && !row.sites.is_empty(),
      "{}: the sampler counted",
      row.name
    );
  }
}
