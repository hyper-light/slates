//! A merge check publishes nothing and a resolution is bound to what was checked (§4.16, A-796; ported from the Codex
//! agent's early-conflict work): `CheckMerge` answers the verdict a submit would get now, with the head and base it
//! judged and the work's revision, and changes neither the work nor the green; `ReadMergeCheck` reads the work and the
//! green at the checked head; `ResolveMerge` retires the canonical operations the agent names, rebases the rest onto the
//! checked head, applies the agent's replacement there and restates the work, refused `StaleMergeView` with nothing
//! changed once the head or the work moved. Each history runs on one shard and on eight, where every work is off its
//! green's shard (A-247), and must answer the same.
// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::indexing_slicing
)]
#![cfg(unix)]

use slates_client::{Client, ClientError, MergeView, Rebased, Refusal, Submitted, WorkChange};
use slates_ipc::protocol::{ReadAt, VolumeId};
use slates_server::{Daemon, DaemonConfig};

mod common;
use common::client::connect;

/// Shape: the daemon where a work and its green share a partition.
const ONE_SHARD: u16 = 1;
/// Shape: enough shards that a work made by a client of another shard than its green's is off the green's shard.
const MANY_SHARDS: u16 = 8;
/// Shape: the file every history edits, and its base bytes: three words, the middle one the contested range.
const FILE: &str = "f";
/// Shape: see [`FILE`].
const BASE: &[u8] = b"aaaa bbbb cccc\n";
/// Shape: the contested range of [`BASE`].
const AT: u64 = 5;
/// Shape: see [`AT`].
const LEN: u64 = 4;

fn daemon(shards: u16, tag: &str) -> (Daemon, String) {
  let profile = common::machine_profile();
  let instance = format!("srv-merge-check-{tag}-{}", std::process::id());
  let daemon = Daemon::start(
    &profile,
    DaemonConfig::derive(&profile, &instance, Some(shards)),
    slates_server::SegmentSource::Create {
      name: format!("slates-seg-merge-check-{tag}-{}", std::process::id()),
    },
  )
  .unwrap();
  daemon
    .bootstrap(true)
    .expect("the fixture explicitly creates its local consensus group");
  (daemon, instance)
}

/// A green made by a client of another shard than `client`'s on a many-shard daemon (A-400), so every work `client`
/// makes is off its green's shard; on one shard, `client`'s own.
fn green_off(client: &mut Client, instance: &str, shards: u16, name: &str) -> VolumeId {
  if shards < 2 {
    return client.create_green(name, false).unwrap();
  }
  let here = client.client_id() % u32::from(shards);
  loop {
    let mut maker = connect(instance);
    if maker.client_id() % u32::from(shards) != here {
      return maker.create_green(name, false).unwrap();
    }
  }
}

fn accepted(client: &mut Client, work: VolumeId) -> u64 {
  match client.submit(work).unwrap() {
    Submitted::Accepted(version) => version,
    other => panic!("not accepted: {other:?}"),
  }
}

/// A green holding [`BASE`] at [`FILE`], and two works over it whose edits of the contested range collide: the first
/// accepted, the second's pending. The head, and the second work.
fn collided(
  client: &mut Client,
  instance: &str,
  shards: u16,
  tag: &str,
) -> (VolumeId, u64, VolumeId) {
  let green = green_off(client, instance, shards, &format!("{tag}-green"));
  let (seed, _) = client.create_work(green, &format!("{tag}-seed")).unwrap();
  client.edit(seed, FILE, 0, 0, BASE).unwrap();
  accepted(client, seed);
  let (first, _) = client.create_work(green, &format!("{tag}-first")).unwrap();
  let (second, _) = client.create_work(green, &format!("{tag}-second")).unwrap();
  client.edit(first, FILE, AT, LEN, b"XXXX").unwrap();
  client.edit(second, FILE, AT, LEN, b"YYYY").unwrap();
  let head = accepted(client, first);
  (green, head, second)
}

fn stale(result: Result<impl std::fmt::Debug, ClientError>) -> bool {
  matches!(result, Err(ClientError::Refused(Refusal::StaleMergeView)))
}

/// The history: a check of a conflicting work and of a clean one, both publishing nothing; the views; a resolution
/// bound to the checked head; refusals once the head or the work moved; a resolution whose kept operations still
/// conflict. What each step answered, written down.
fn history(client: &mut Client, instance: &str, shards: u16) -> Vec<String> {
  let mut said = Vec::new();
  let (green, head, second) = collided(client, instance, shards, "h");
  let base = head - 1;
  let check = checked_conflicting(client, (green, head, base), second, &mut said);
  views_named(client, &check, &mut said);
  clean_checked(client, green, base, &mut said);
  resolved_and_landed(client, (green, head, second), &check, &mut said);
  let late = refused_once_moved(client, green, &mut said);
  kept_conflict(client, green, late, &mut said);
  said
}

/// A check of the conflicting work `second`: the windows, the head and base it judged, nothing changed, nothing
/// published.
fn checked_conflicting(
  client: &mut Client,
  (green, head, base): (VolumeId, u64, u64),
  second: VolumeId,
  said: &mut Vec<String>,
) -> slates_client::MergeCheck {
  let check = client.check_merge(second).unwrap();
  said.push(format!(
    "check: head {} base {} conflicts {} declaration {}",
    check.stamp.head - base,
    check.stamp.base - base,
    check.conflicts.len(),
    check.declaration.len()
  ));
  assert!(!check.conflicts.is_empty(), "the second work conflicts");
  assert_eq!((check.stamp.head, check.stamp.base), (head, base));
  assert_eq!(
    client.read(second, FILE, ReadAt::Head).unwrap(),
    b"aaaa YYYY cccc\n",
    "a check leaves the work's bytes"
  );
  let again = client.check_merge(second).unwrap();
  assert_eq!(
    again.stamp, check.stamp,
    "a check changes nothing it judged"
  );
  assert_eq!(
    client.versions(green).unwrap(),
    head,
    "a check publishes nothing"
  );

  check
}

/// The views the check names: the green's and the work's.
fn views_named(client: &mut Client, check: &slates_client::MergeCheck, said: &mut Vec<String>) {
  let theirs = client
    .read_merge_check(check.stamp, MergeView::Green, FILE)
    .unwrap();
  let mine = client
    .read_merge_check(check.stamp, MergeView::Work, FILE)
    .unwrap();
  said.push(format!(
    "views: green {:?} work {:?}",
    String::from_utf8_lossy(&theirs),
    String::from_utf8_lossy(&mine)
  ));
  assert_eq!(theirs, b"aaaa XXXX cccc\n");
  assert_eq!(mine, b"aaaa YYYY cccc\n");
}

/// A clean work's check: no window, and its base stays where it was (a check is not a rebase).
fn clean_checked(client: &mut Client, green: VolumeId, base: u64, said: &mut Vec<String>) {
  let (clean, _) = client.create_work(green, "h-clean").unwrap();
  client.edit(clean, "other", 0, 0, b"own\n").unwrap();
  let clean_check = client.check_merge(clean).unwrap();
  said.push(format!(
    "clean: conflicts {} base {}",
    clean_check.conflicts.len(),
    clean_check.stamp.base - base
  ));
  assert!(clean_check.conflicts.is_empty());
}

/// A resolution bound to the check: every operation of the work on the file retired, the agent's own bytes written at
/// the head, then submitted, and landed as the agent wrote it.
fn resolved_and_landed(
  client: &mut Client,
  (green, head, second): (VolumeId, u64, VolumeId),
  check: &slates_client::MergeCheck,
  said: &mut Vec<String>,
) {
  let discard: Vec<u64> = check
    .declaration
    .iter()
    .enumerate()
    .filter(|(_, op)| op.path == FILE)
    .map(|(index, _)| u64::try_from(index).unwrap())
    .collect();
  assert!(!discard.is_empty(), "the declaration names the file");
  let resolved = client
    .resolve_merge(
      check.stamp,
      &discard,
      vec![WorkChange::Edit {
        path: FILE.to_owned(),
        at: AT,
        delete_len: LEN,
        bytes: b"ZZZZ".to_vec(),
      }],
    )
    .unwrap();
  said.push(format!("resolved: {resolved:?}").replace(&head.to_string(), "HEAD"));
  assert_eq!(resolved, Rebased::Rebased(head));
  assert_eq!(
    client.read(second, FILE, ReadAt::Head).unwrap(),
    b"aaaa ZZZZ cccc\n"
  );
  let merged = accepted(client, second);
  assert_eq!(
    client
      .read(green, FILE, ReadAt::Version { version: merged })
      .unwrap(),
    b"aaaa ZZZZ cccc\n",
    "the resolution lands as the agent wrote it"
  );
}

/// A check whose head then moves, then one whose work then changes: the resolution and the views refused, the work
/// unchanged. The late work.
fn refused_once_moved(client: &mut Client, green: VolumeId, said: &mut Vec<String>) -> VolumeId {
  let (late, _) = client.create_work(green, "h-late").unwrap();
  client.edit(late, FILE, AT, LEN, b"LLLL").unwrap();
  let late_check = client.check_merge(late).unwrap();
  let (mover, _) = client.create_work(green, "h-mover").unwrap();
  client.edit(mover, FILE, 0, 4, b"MMMM").unwrap();
  accepted(client, mover);
  let moved = client.resolve_merge(late_check.stamp, &[], Vec::new());
  said.push(format!(
    "after the head moved: stale {}",
    stale(moved.clone())
  ));
  assert!(stale(moved), "a moved head refuses the resolution");
  assert_eq!(
    client.read(late, FILE, ReadAt::Head).unwrap(),
    b"aaaa LLLL cccc\n"
  );

  // A check whose work then changes: refused too.
  let fresh = client.check_merge(late).unwrap();
  client.edit(late, "other", 0, 0, b"more\n").unwrap();
  let changed = client.resolve_merge(fresh.stamp, &[], Vec::new());
  let read = client.read_merge_check(fresh.stamp, MergeView::Work, FILE);
  said.push(format!(
    "after the work changed: stale {} read stale {}",
    stale(changed.clone()),
    stale(read.clone())
  ));
  assert!(stale(changed));
  assert!(stale(read));

  late
}

/// A resolution that keeps the conflicting operation: another work writes the contested range first; the windows, and
/// nothing changed.
fn kept_conflict(client: &mut Client, green: VolumeId, late: VolumeId, said: &mut Vec<String>) {
  let (rival, _) = client.create_work(green, "h-rival").unwrap();
  client.edit(rival, FILE, AT, LEN, b"RRRR").unwrap();
  accepted(client, rival);
  let kept = client.check_merge(late).unwrap();
  assert!(!kept.conflicts.is_empty(), "the late work now conflicts");
  let still = client.resolve_merge(kept.stamp, &[], Vec::new()).unwrap();
  said.push(format!(
    "kept the conflict: {}",
    matches!(still, Rebased::Conflict(ref windows) if !windows.is_empty())
  ));
  assert!(matches!(still, Rebased::Conflict(ref windows) if !windows.is_empty()));
  assert_eq!(
    client.read(late, FILE, ReadAt::Head).unwrap(),
    b"aaaa LLLL cccc\n"
  );
}

/// §4.16, A-796 (the Codex agent's early-conflict check and conditional resolution). Do: the history above on one
/// shard and on eight. Expect: every step as asserted, and the same answers on both.
#[test]
fn a_merge_check_publishes_nothing_and_a_resolution_is_bound_to_what_was_checked() {
  let run = |shards: u16, tag: &str| {
    let (daemon, instance) = daemon(shards, tag);
    let mut client = connect(&instance);
    let said = history(&mut client, &instance, shards);
    drop(client);
    daemon.stop();
    said
  };
  let one = run(ONE_SHARD, "one");
  let many = run(MANY_SHARDS, "many");
  assert_eq!(one, many, "one shard and eight answer the same");
}

/// A-796 across a restart (the Codex agent's "lost Work observations after restart"). Do: a work's one edit, checked;
/// the daemon restarted over its anchor (the work rebuilt from its log); one more edit; a resolution naming the check's
/// stamp. Expect `StaleMergeView` and the work unchanged: a stamp taken before the restart never names the work after
/// it changed, however many changes it took to get there.
#[test]
fn a_stamp_from_before_a_restart_never_names_the_work_after_it_changed() {
  let profile = common::machine_profile();
  let instance = format!("srv-merge-check-restart-{}", std::process::id());
  let config = DaemonConfig::derive(&profile, &instance, Some(ONE_SHARD));
  let anchor = common::anchor::held_anchor("merge-check-restart", &profile, &config);
  let first = Daemon::start(&profile, config.clone(), common::anchor::source_of(&anchor)).unwrap();
  first.bootstrap(true).unwrap();
  let mut client = connect(&instance);
  let green = client.create_green("restart-green", false).unwrap();
  let (work, _) = client.create_work(green, "restart-work").unwrap();
  client.edit(work, "a", 0, 0, b"one\n").unwrap();
  let stamp = client.check_merge(work).unwrap().stamp;
  drop(client);
  first.stop();
  let second = Daemon::start(&profile, config, common::anchor::source_of(&anchor)).unwrap();
  let mut client = connect(&instance);
  client.edit(work, "b", 0, 0, b"two\n").unwrap();
  let resolved = client.resolve_merge(stamp, &[], Vec::new());
  assert!(
    stale(resolved.clone()),
    "a pre-restart stamp named the changed work: {resolved:?}"
  );
  assert_eq!(client.read(work, "b", ReadAt::Head).unwrap(), b"two\n");
  drop(client);
  second.stop();
}

/// A-796's metadata rule (the Codex agent's `rebase_with`; AC-6.13). Do: the head removes a file; a work set its mode
/// before it saw that; the work's check conflicts; a resolution keeps the mode and writes the file anew at the head.
/// Expect the resolution to rebase (the kept mode is judged against the file the replacement establishes, not the one
/// the head removed) and its submit to land the new file.
#[test]
fn a_kept_mode_on_a_file_the_resolution_writes_anew_is_not_a_conflict() {
  let (daemon, instance) = daemon(ONE_SHARD, "meta");
  let mut client = connect(&instance);
  let green = client.create_green("meta-green", false).unwrap();
  let (seed, _) = client.create_work(green, "meta-seed").unwrap();
  client.edit(seed, FILE, 0, 0, BASE).unwrap();
  accepted(&mut client, seed);
  let (work, _) = client.create_work(green, "meta-work").unwrap();
  let (remover, _) = client.create_work(green, "meta-remover").unwrap();
  client
    .declare(
      work,
      slates_client::WorkOp::SetMode {
        path: FILE.to_owned(),
        mode: 0o100_755,
      },
    )
    .unwrap();
  client
    .declare(
      remover,
      slates_client::WorkOp::Unlink {
        path: FILE.to_owned(),
      },
    )
    .unwrap();
  let head = accepted(&mut client, remover);
  let check = client.check_merge(work).unwrap();
  assert!(
    !check.conflicts.is_empty(),
    "a mode on a removed file conflicts"
  );
  let resolved = client
    .resolve_merge(
      check.stamp,
      &[],
      vec![WorkChange::Edit {
        path: FILE.to_owned(),
        at: 0,
        delete_len: 0,
        bytes: b"anew\n".to_vec(),
      }],
    )
    .unwrap();
  assert_eq!(resolved, Rebased::Rebased(head));
  let merged = accepted(&mut client, work);
  assert_eq!(
    client
      .read(green, FILE, ReadAt::Version { version: merged })
      .unwrap(),
    b"anew\n"
  );
  drop(client);
  daemon.stop();
}
