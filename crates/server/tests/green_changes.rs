//! What a green changed since a version (§4.16 `changed_since`), at the scale a green reaches: every file a work's
//! submit changed, however many. One reply is one bulk chunk, so an answer longer than one chunk is answered in pages
//! the client walks. Before 2026-11-08 the whole answer was one reply, refused "reply too large for the bulk area" at
//! 800 changed files.
// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::indexing_slicing,
  clippy::unwrap_in_result
)]

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use slates_client::Client;
use slates_server::{Daemon, DaemonConfig, SegmentSource};

mod common;
use common::client::connect;

/// Shape: one shard, the green's owner, which answers its changes.
const TEST_SHARDS: u16 = 1;
/// Shape: the files one submit changes: about ten replies' worth of paths at these names.
const CHANGED_FILES: usize = 3_000;
/// Shape: how long the asynchronous answer may take to arrive in full.
const ASYNC_WAIT: Duration = Duration::from_secs(60);

fn daemon(tag: &str) -> (Daemon, String) {
  let profile = common::machine_profile();
  let instance = format!("srv-green-changes-{tag}-{}", std::process::id());
  let config = DaemonConfig::derive(&profile, &instance, Some(TEST_SHARDS));
  let daemon = Daemon::start(
    &profile,
    config,
    SegmentSource::Create {
      name: format!("slates-seg-green-changes-{tag}-{}", std::process::id()),
    },
  )
  .unwrap();
  daemon
    .bootstrap(true)
    .expect("the fixture explicitly creates its local consensus group");
  (daemon, instance)
}

fn path(n: usize) -> String {
  format!("/changed/file-{n:05}.txt")
}

/// A green whose one submit changed `paths`; the green and the version the work was based on.
fn green_with_changes(client: &mut Client, paths: &[String]) -> (slates_client::VolumeId, u64) {
  let green = client.create_green("changes-green", false).unwrap();
  let (work, base) = client.create_work(green, "changes-work").unwrap();
  for path in paths {
    client
      .edit(work, path, 0, 0, b"changed")
      .unwrap_or_else(|e| panic!("edit {path}: {e}"));
  }
  client.submit(work).unwrap();
  (green, base)
}

/// §4.16. Do: submit a work that changed 3,000 files, then ask the green what changed since the work's base, through
/// the client's call and its asynchronous form (begin, then poll to the end). Expect: each answer holds every changed
/// path exactly once. Before 2026-11-08 each was refused `BadRequest { reason: "reply too large for the bulk area" }`.
#[test]
fn a_green_answers_every_path_it_changed_however_many() {
  let (daemon, instance) = daemon("many");
  let mut client = connect(&instance);
  let paths: Vec<String> = (0..CHANGED_FILES).map(path).collect();
  let (green, base) = green_with_changes(&mut client, &paths);
  let changed = client
    .changed_since(green, base)
    .expect("every path the green changed");
  let expected: BTreeSet<&str> = paths.iter().map(String::as_str).collect();
  let answered: BTreeSet<&str> = changed.iter().map(|p| p.trim_start_matches('/')).collect();
  assert_eq!(changed.len(), paths.len(), "every changed path, each once");
  assert_eq!(
    answered,
    expected.iter().map(|p| p.trim_start_matches('/')).collect(),
    "exactly the changed paths"
  );
  let word = client.changed_since_begin(green, base).unwrap().word();
  let started = Instant::now();
  let walked = loop {
    if let Some(paths) = client
      .changed_since_poll(word)
      .expect("the asynchronous answer")
    {
      break paths;
    }
    assert!(
      started.elapsed() < ASYNC_WAIT,
      "the asynchronous answer arrived within {ASYNC_WAIT:?}"
    );
    std::hint::spin_loop();
  };
  assert_eq!(walked, changed, "the asynchronous answer is the same");
  drop(client);
  daemon.stop();
}

/// One page of a changed-since answer through the wire, as the client walks it: its paths and whether more follow.
fn page(
  client: &mut Client,
  green: slates_client::VolumeId,
  version: u64,
  after: Option<String>,
) -> (Vec<String>, bool) {
  match client
    .call(&slates_ipc::protocol::RequestBody::ChangedSince {
      green,
      version,
      after,
    })
    .unwrap()
  {
    slates_ipc::protocol::ReplyBody::ChangedSince { paths, more } => (paths, more),
    other => panic!("a changed-since page: {other:?}"),
  }
}

/// §4.16. Do: submit a work that changed 3,000 files and walk the answer a page at a time; before each page after the
/// first, submit another work that changes one new file. Expect: more than one page, every path changed when the walk
/// began answered exactly once, none twice, and the pages in path order.
#[test]
fn a_path_changed_before_the_walk_is_answered_exactly_once_while_the_green_moves_on() {
  let (daemon, instance) = daemon("churn");
  let mut client = connect(&instance);
  let paths: Vec<String> = (0..CHANGED_FILES).map(path).collect();
  let (green, base) = green_with_changes(&mut client, &paths);
  let mut answered: Vec<String> = Vec::new();
  let mut pages = 0usize;
  loop {
    if pages > 0 {
      let (work, _) = client
        .create_work(green, &format!("churn-work-{pages}"))
        .unwrap();
      client
        .edit(work, &format!("/churn/new-{pages:04}.txt"), 0, 0, b"new")
        .unwrap();
      client.submit(work).unwrap();
    }
    let (page_paths, more) = page(&mut client, green, base, answered.last().cloned());
    pages += 1;
    assert!(
      page_paths.windows(2).all(|pair| pair[0] < pair[1])
        && answered
          .last()
          .is_none_or(|last| page_paths.first().is_none_or(|first| first > last)),
      "page {pages} continues in path order"
    );
    answered.extend(page_paths);
    if !more {
      break;
    }
  }
  eprintln!(
    "walked {} paths in {pages} pages ({} submits during the walk)",
    answered.len(),
    pages - 1
  );
  assert!(pages > 1, "the answer took {pages} page(s): it must page");
  let seen: BTreeSet<&str> = answered.iter().map(|p| p.trim_start_matches('/')).collect();
  assert_eq!(seen.len(), answered.len(), "no path answered twice");
  for path in &paths {
    assert!(
      seen.contains(path.trim_start_matches('/')),
      "{path}, changed before the walk, is answered"
    );
  }
  drop(client);
  daemon.stop();
}

/// Format: the length of each directory name in the longest-path probe: under the 255-byte name limit, so only the
/// path's whole length is tested.
const COMPONENT_BYTES: usize = 200;

/// A path of about `bytes` bytes, in directories of [`COMPONENT_BYTES`].
fn long_path(bytes: usize) -> String {
  let mut path = String::new();
  while path.len() + 1 + COMPONENT_BYTES < bytes {
    path.push('/');
    path.push_str(&"d".repeat(COMPONENT_BYTES));
  }
  path.push('/');
  path.push_str(&"f".repeat(bytes.saturating_sub(path.len() + 1).max(1)));
  path
}

/// §4.16, §4.3. Do: on a green, change a file at the longest path an edit carries (found by asking: one byte more is
/// refused), and ask what changed. Expect: the answer holds it: every path a work can change, a green can answer.
#[test]
fn the_longest_path_an_edit_carries_is_answered() {
  let (daemon, instance) = daemon("longest");
  let mut client = connect(&instance);
  let green = client.create_green("longest-green", false).unwrap();
  let (mut fits, mut refused) = (1usize, 1usize << 16);
  while refused - fits > 1 {
    let probe = (fits + refused) / 2;
    let (work, _) = client
      .create_work(green, &format!("probe-{probe}"))
      .unwrap();
    match client.edit(work, &long_path(probe), 0, 0, b"x") {
      Ok(()) => fits = probe,
      Err(_) => refused = probe,
    }
  }
  let (work, base) = client.create_work(green, "longest-work").unwrap();
  let longest = long_path(fits);
  client.edit(work, &longest, 0, 0, b"x").unwrap();
  client.submit(work).unwrap();
  let changed = client
    .changed_since(green, base)
    .unwrap_or_else(|e| panic!("an answer holding a {fits}-byte path: {e}"));
  eprintln!("the longest path an edit carries and the green answers: {fits} bytes");
  assert!(
    changed
      .iter()
      .any(|p| p.trim_start_matches('/') == longest.trim_start_matches('/')),
    "the {fits}-byte path is answered"
  );
  drop(client);
  daemon.stop();
}

/// §4.16 (A-264). Do: a work writes `/original.txt`, links `/alias.txt` to it, makes `/pointer` a symlink, sets a mode
/// on the original, sets an xattr on it and makes `/d` a directory; submit it, then ask what changed since its base.
/// Expect: every one of those paths. A work learns from `changed_since` which of its pending paths the green moved
/// under, and a link, a symlink, a mode, an xattr or a directory moved under it is that as much as content is. Before
/// 2026-11-08 only `/original.txt` was answered: the content index alone was read.
#[test]
fn a_green_answers_every_dimension_it_changed() {
  use slates_ipc::protocol::WorkOp;
  let (daemon, instance) = daemon("dimensions");
  let mut client = connect(&instance);
  let green = client.create_green("dimensions-green", false).unwrap();
  let (work, base) = client.create_work(green, "dimensions-work").unwrap();
  client.edit(work, "/original.txt", 0, 0, b"bytes").unwrap();
  for op in [
    WorkOp::Link {
      path: "/alias.txt".into(),
      target: "/original.txt".into(),
    },
    WorkOp::Symlink {
      path: "/pointer".into(),
      target: "original.txt".into(),
    },
    WorkOp::SetMode {
      path: "/original.txt".into(),
      mode: 0o600,
    },
    WorkOp::SetXattr {
      path: "/original.txt".into(),
      name: "user.k".into(),
      value: b"v".to_vec(),
    },
    WorkOp::Mkdir { path: "/d".into() },
  ] {
    client.declare(work, op).unwrap();
  }
  client.submit(work).unwrap();
  let changed: Vec<String> = client
    .changed_since(green, base)
    .unwrap()
    .iter()
    .map(|path| path.trim_start_matches('/').to_owned())
    .collect();
  for path in ["original.txt", "alias.txt", "pointer", "d"] {
    assert!(
      changed.iter().any(|changed| changed == path),
      "{path} changed: {changed:?}"
    );
  }
  drop(client);
  daemon.stop();
}
