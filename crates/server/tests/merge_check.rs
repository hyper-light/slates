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

/// A-840's history: a conflict resolved in one ask. The losing work also wrote the third word, a range no rival
/// touched; after its conflict a third work moved the head again in the first word. What each step answered, written
/// down.
fn resolved_in_one_ask(client: &mut Client, instance: &str, shards: u16) -> Vec<String> {
  let mut said = Vec::new();
  let (green, _, second) = collided(client, instance, shards, "one-ask");
  client.edit(second, FILE, 10, 4, b"ZZZZ").unwrap();
  let Submitted::Conflict(windows) = client.submit(second).unwrap() else {
    panic!("the contested range conflicts");
  };
  said.push(format!("conflict: {windows:?}"));
  let (mover, _) = client.create_work(green, "one-ask-mover").unwrap();
  client.edit(mover, FILE, 0, 4, b"MMMM").unwrap();
  let moved = accepted(client, mover);
  // The windows given up, the agent's own bytes written there at the head as it is now: not the head the conflict was
  // judged at, and no check between.
  let replacement = vec![WorkChange::Edit {
    path: FILE.to_owned(),
    at: AT,
    delete_len: LEN,
    bytes: b"RRRR".to_vec(),
  }];
  let resolved = client
    .resolve_conflict(second, windows.clone(), replacement)
    .unwrap();
  assert_eq!(
    resolved,
    Rebased::Rebased(moved),
    "resolved at the head as it is"
  );
  said.push(format!(
    "work: {:?}",
    String::from_utf8(client.read(second, FILE, ReadAt::Head).unwrap()).unwrap()
  ));
  let landed = accepted(client, second);
  said.push(format!(
    "landed {} after the move: {:?}",
    landed - moved,
    String::from_utf8(client.read(green, FILE, ReadAt::Head).unwrap()).unwrap()
  ));
  // A kept operation a rival then hits: the resolution answers its window and changes nothing.
  let (late, _) = client.create_work(green, "one-ask-late").unwrap();
  client.edit(late, FILE, 0, 4, b"LLLL").unwrap();
  client.edit(late, FILE, 10, 4, b"QQQQ").unwrap();
  let (rival, _) = client.create_work(green, "one-ask-rival").unwrap();
  client.edit(rival, FILE, 0, 4, b"NNNN").unwrap();
  client.edit(rival, FILE, 10, 4, b"WWWW").unwrap();
  accepted(client, rival);
  let Submitted::Conflict(late_windows) = client.submit(late).unwrap() else {
    panic!("both words conflict");
  };
  let first_word: Vec<_> = late_windows.iter().filter(|w| w.at == 0).cloned().collect();
  let kept = client
    .resolve_conflict(late, first_word, Vec::new())
    .unwrap();
  said.push(format!("kept still conflicting: {kept:?}"));
  said.push(format!(
    "late work unchanged: {:?}",
    String::from_utf8(client.read(late, FILE, ReadAt::Head).unwrap()).unwrap()
  ));
  said
}

/// A-840. Do: a work's edit of the contested range conflicts; a third work moves the head in another range; the agent
/// resolves in one ask, naming the conflict's windows and writing its own bytes there. Expect the work rebased onto the
/// head as it is (no check, nothing stale), its edit of an uncontested range kept, its replacement in place, and its
/// submit landing all three. Then a resolution that gives up one window while a kept operation still meets the head:
/// expect that window answered and the work unchanged. One shard and eight answer the same.
#[test]
fn a_conflict_resolved_in_one_ask_keeps_what_its_windows_do_not_name() {
  let run = |shards: u16, tag: &str| {
    let (daemon, instance) = daemon(shards, tag);
    let mut client = connect(&instance);
    let said = resolved_in_one_ask(&mut client, &instance, shards);
    drop(client);
    daemon.stop();
    said
  };
  let one = run(ONE_SHARD, "one-ask-one");
  assert_eq!(
    one.get(1..).unwrap(),
    [
      "work: \"MMMM RRRR ZZZZ\\n\"".to_owned(),
      "landed 1 after the move: \"MMMM RRRR ZZZZ\\n\"".to_owned(),
      "kept still conflicting: Conflict([MergeWindow { path: \"f\", at: 10, len: 4, class: 0 }])"
        .to_owned(),
      "late work unchanged: \"LLLL RRRR QQQQ\\n\"".to_owned(),
    ],
    "{one:#?}"
  );
  let many = run(MANY_SHARDS, "one-ask-many");
  assert_eq!(one, many, "one shard and eight answer the same");
}

/// A-840 after the resolution: the work is based at the head it was resolved at, so a rival's write there afterwards,
/// over the replacement or over a kept operation, is a conflict at its submit. What each submit answered.
fn judged_from_the_resolution(
  client: &mut Client,
  instance: &str,
  shards: u16,
  over_replaced: bool,
) -> String {
  let tag = if over_replaced { "after-r" } else { "after-k" };
  let (green, _, second) = collided(client, instance, shards, tag);
  client.edit(second, FILE, 10, 4, b"ZZZZ").unwrap();
  let Submitted::Conflict(windows) = client.submit(second).unwrap() else {
    panic!("the contested range conflicts");
  };
  let replacement = vec![WorkChange::Edit {
    path: FILE.to_owned(),
    at: AT,
    delete_len: LEN,
    bytes: b"RRRR".to_vec(),
  }];
  let Rebased::Rebased(_) = client
    .resolve_conflict(second, windows, replacement)
    .unwrap()
  else {
    panic!("resolved");
  };
  let (rival, _) = client.create_work(green, &format!("{tag}-rival")).unwrap();
  let at = if over_replaced { AT } else { 10 };
  client.edit(rival, FILE, at, 4, b"VVVV").unwrap();
  accepted(client, rival);
  format!("{:?}", client.submit(second).unwrap())
}

/// A-840. Do: resolve a conflict in one ask, then let a rival write the range the replacement wrote, or the range a
/// kept operation wrote, and submit the resolved work. Expect a conflict naming that range each time, on one shard and
/// on eight: the resolution moved the work's base to the head it was judged at, and nothing past it is taken as seen.
#[test]
fn a_write_after_a_one_ask_resolution_meets_the_work_at_its_submit() {
  for shards in [ONE_SHARD, MANY_SHARDS] {
    let (daemon, instance) = daemon(shards, &format!("after-{shards}"));
    let mut client = connect(&instance);
    let replaced = judged_from_the_resolution(&mut client, &instance, shards, true);
    let kept = judged_from_the_resolution(&mut client, &instance, shards, false);
    drop(client);
    daemon.stop();
    assert_eq!(
      (replaced.as_str(), kept.as_str()),
      (
        "Conflict([MergeWindow { path: \"f\", at: 5, len: 4, class: 0 }])",
        "Conflict([MergeWindow { path: \"f\", at: 10, len: 4, class: 0 }])"
      ),
      "{shards} shards"
    );
  }
}

/// A-840 over two files: the work wrote `f`'s contested range and `g`; the conflict is in `f`; after the resolution a
/// rival writes `g`, or `f`'s replaced range. What the work's submit answered.
fn two_files_after_the_resolution(
  client: &mut Client,
  instance: &str,
  shards: u16,
  rival_in_g: bool,
) -> String {
  let tag = if rival_in_g { "two-g" } else { "two-f" };
  let green = green_off(client, instance, shards, &format!("{tag}-green"));
  let (seed, _) = client.create_work(green, &format!("{tag}-seed")).unwrap();
  client.edit(seed, FILE, 0, 0, BASE).unwrap();
  client.edit(seed, "g", 0, 0, BASE).unwrap();
  accepted(client, seed);
  let (work, _) = client.create_work(green, &format!("{tag}-work")).unwrap();
  client.edit(work, FILE, AT, LEN, b"YYYY").unwrap();
  client.edit(work, "g", AT, LEN, b"GGGG").unwrap();
  let (first, _) = client.create_work(green, &format!("{tag}-first")).unwrap();
  client.edit(first, FILE, AT, LEN, b"XXXX").unwrap();
  accepted(client, first);
  let Submitted::Conflict(windows) = client.submit(work).unwrap() else {
    panic!("the contested range conflicts");
  };
  let replacement = vec![WorkChange::Edit {
    path: FILE.to_owned(),
    at: AT,
    delete_len: LEN,
    bytes: b"RRRR".to_vec(),
  }];
  let Rebased::Rebased(_) = client
    .resolve_conflict(work, windows.clone(), replacement)
    .unwrap()
  else {
    panic!("resolved over {windows:?}");
  };
  let (rival, _) = client.create_work(green, &format!("{tag}-rival")).unwrap();
  client
    .edit(rival, if rival_in_g { "g" } else { FILE }, AT, LEN, b"VVVV")
    .unwrap();
  accepted(client, rival);
  format!("{:?}", client.submit(work).unwrap())
}

/// A-840. Do: a work writes two files, conflicts in one and resolves in one ask; a rival then writes the other file's
/// range, or the replaced one. Expect the submit to conflict on exactly that range, on one shard and on eight.
#[test]
fn a_write_after_a_one_ask_resolution_meets_the_work_in_every_file() {
  for shards in [ONE_SHARD, MANY_SHARDS] {
    let (daemon, instance) = daemon(shards, &format!("two-{shards}"));
    let mut client = connect(&instance);
    let in_g = two_files_after_the_resolution(&mut client, &instance, shards, true);
    let in_f = two_files_after_the_resolution(&mut client, &instance, shards, false);
    drop(client);
    daemon.stop();
    assert_eq!(
      (in_g.as_str(), in_f.as_str()),
      (
        "Conflict([MergeWindow { path: \"g\", at: 5, len: 4, class: 0 }])",
        "Conflict([MergeWindow { path: \"f\", at: 5, len: 4, class: 0 }])"
      ),
      "{shards} shards"
    );
  }
}

/// A-840 twice over one file: a work writes [`FILE`]'s second and third words; a rival takes the second, the work gives
/// it up and writes it again; a rival takes the third, the work gives that up and writes it again. The green after the
/// work lands.
fn resolved_twice_in_one_file(client: &mut Client, instance: &str, shards: u16) -> String {
  let green = green_off(client, instance, shards, "twice-green");
  let (seed, _) = client.create_work(green, "twice-seed").unwrap();
  client.edit(seed, FILE, 0, 0, BASE).unwrap();
  accepted(client, seed);
  let (work, _) = client.create_work(green, "twice-work").unwrap();
  client.edit(work, FILE, AT, LEN, b"YYYY").unwrap();
  client.edit(work, FILE, 10, 4, b"ZZZZ").unwrap();
  let give_up = |client: &mut Client, at: u64, rival_bytes: &[u8], mine: &[u8], tag: &str| {
    let (rival, _) = client.create_work(green, tag).unwrap();
    client.edit(rival, FILE, at, 4, rival_bytes).unwrap();
    accepted(client, rival);
    let Submitted::Conflict(windows) = client.submit(work).unwrap() else {
      panic!("{tag}: the rival's range conflicts");
    };
    let replacement = vec![WorkChange::Edit {
      path: FILE.to_owned(),
      at,
      delete_len: 4,
      bytes: mine.to_vec(),
    }];
    let resolved = client
      .resolve_conflict(work, windows.clone(), replacement)
      .unwrap();
    assert!(
      matches!(resolved, Rebased::Rebased(_)),
      "{tag}: resolved over {windows:?}: {resolved:?}"
    );
  };
  give_up(client, AT, b"PPPP", b"RRRR", "twice-first");
  give_up(client, 10, b"QQQQ", b"SSSS", "twice-second");
  accepted(client, work);
  String::from_utf8(client.read(green, FILE, ReadAt::Head).unwrap()).unwrap()
}

/// A-840. Do: resolve a work's conflict on one range of a file in one ask, then its later conflict on another range of
/// the same file the same way. Expect each resolution to give up only its own window: the green ends with both of the
/// work's rewritten ranges, never the first rival's bytes back, on one shard and on eight. A resolution's restated work
/// may hold a file's ranges as one operation, so retiring whole operations a window meets took back the first
/// resolution's bytes (found by the 1,000-client load harness's oracle, 2026-11-21).
#[test]
fn a_second_one_ask_resolution_in_a_file_gives_up_only_its_window() {
  for shards in [ONE_SHARD, MANY_SHARDS] {
    let (daemon, instance) = daemon(shards, &format!("twice-{shards}"));
    let mut client = connect(&instance);
    let green = resolved_twice_in_one_file(&mut client, &instance, shards);
    drop(client);
    daemon.stop();
    assert_eq!(green, "aaaa RRRR SSSS\n", "{shards} shards");
  }
}

/// A-840 with two edits of one file in one replacement: a work writes [`FILE`]'s second and third words, a rival takes
/// both, and the work gives both up and writes both again in one ask. The green after the work lands.
fn two_edits_in_one_replacement(
  client: &mut Client,
  instance: &str,
  shards: u16,
  first: u64,
) -> String {
  let green = green_off(client, instance, shards, &format!("pair-{first}-green"));
  let (seed, _) = client
    .create_work(green, &format!("pair-{first}-seed"))
    .unwrap();
  client.edit(seed, FILE, 0, 0, BASE).unwrap();
  accepted(client, seed);
  let (work, _) = client
    .create_work(green, &format!("pair-{first}-work"))
    .unwrap();
  client.edit(work, FILE, first, LEN, b"YYYY").unwrap();
  client.edit(work, FILE, 10, 4, b"ZZZZ").unwrap();
  let (rival, _) = client
    .create_work(green, &format!("pair-{first}-rival"))
    .unwrap();
  client.edit(rival, FILE, first, LEN, b"PPPP").unwrap();
  client.edit(rival, FILE, 10, 4, b"QQQQ").unwrap();
  accepted(client, rival);
  let Submitted::Conflict(windows) = client.submit(work).unwrap() else {
    panic!("both words conflict");
  };
  let edit = |at: u64, bytes: &[u8]| WorkChange::Edit {
    path: FILE.to_owned(),
    at,
    delete_len: 4,
    bytes: bytes.to_vec(),
  };
  let replacement = vec![edit(first, b"RRRR"), edit(10, b"SSSS")];
  let resolved = client
    .resolve_conflict(work, windows.clone(), replacement)
    .unwrap();
  assert!(
    matches!(resolved, Rebased::Rebased(_)),
    "resolved over {windows:?}: {resolved:?}"
  );
  accepted(client, work);
  String::from_utf8(client.read(green, FILE, ReadAt::Head).unwrap()).unwrap()
}

/// A-840. Do: give up two windows of one file and write both again in one replacement. Expect both written, on one
/// shard and on eight.
#[test]
fn a_one_ask_replacement_writes_every_edit_of_a_file() {
  for shards in [ONE_SHARD, MANY_SHARDS] {
    let (daemon, instance) = daemon(shards, &format!("pair-{shards}"));
    let mut client = connect(&instance);
    let inside = two_edits_in_one_replacement(&mut client, &instance, shards, AT);
    let at_start = two_edits_in_one_replacement(&mut client, &instance, shards, 0);
    drop(client);
    daemon.stop();
    assert_eq!(
      (inside.as_str(), at_start.as_str()),
      ("aaaa RRRR SSSS\n", "RRRR bbbb SSSS\n"),
      "{shards} shards"
    );
  }
}

/// A-840, a kept replacement beside a new one: a work writes [`FILE`]'s second and third words; a rival takes both and
/// the work writes both again in one ask; a rival takes the third again and the work gives up only that. The green
/// after the work lands.
fn kept_replacement_beside_a_new_one(
  client: &mut Client,
  instance: &str,
  shards: u16,
  first: u64,
) -> String {
  let green = green_off(client, instance, shards, &format!("kept-{first}-green"));
  let (seed, _) = client
    .create_work(green, &format!("kept-{first}-seed"))
    .unwrap();
  client.edit(seed, FILE, 0, 0, BASE).unwrap();
  accepted(client, seed);
  let (work, _) = client
    .create_work(green, &format!("kept-{first}-work"))
    .unwrap();
  client.edit(work, FILE, first, LEN, b"YYYY").unwrap();
  client.edit(work, FILE, 10, 4, b"ZZZZ").unwrap();
  let edit = |at: u64, bytes: &[u8]| WorkChange::Edit {
    path: FILE.to_owned(),
    at,
    delete_len: 4,
    bytes: bytes.to_vec(),
  };
  let rivals = |client: &mut Client, ats: &[u64], tag: &str| {
    let (rival, _) = client.create_work(green, tag).unwrap();
    for at in ats {
      client.edit(rival, FILE, *at, 4, b"PPPP").unwrap();
    }
    accepted(client, rival);
  };
  rivals(client, &[first, 10], &format!("kept-{first}-both"));
  let Submitted::Conflict(windows) = client.submit(work).unwrap() else {
    panic!("both words conflict");
  };
  let both = vec![edit(first, b"RRRR"), edit(10, b"SSSS")];
  assert!(matches!(
    client.resolve_conflict(work, windows, both).unwrap(),
    Rebased::Rebased(_)
  ));
  rivals(client, &[10], &format!("kept-{first}-third"));
  let Submitted::Conflict(windows) = client.submit(work).unwrap() else {
    panic!("the third word conflicts");
  };
  assert!(matches!(
    client
      .resolve_conflict(work, windows, vec![edit(10, b"UUUU")])
      .unwrap(),
    Rebased::Rebased(_)
  ));
  accepted(client, work);
  String::from_utf8(client.read(green, FILE, ReadAt::Head).unwrap()).unwrap()
}

/// A-840. Do: a work resolves both of its ranges of a file in one ask, then gives up only one of them in a second.
/// Expect the range the second kept to land with the first resolution's bytes, on one shard and on eight.
#[test]
fn a_kept_replacement_lands_beside_a_new_one() {
  for shards in [ONE_SHARD, MANY_SHARDS] {
    let (daemon, instance) = daemon(shards, &format!("kept-{shards}"));
    let mut client = connect(&instance);
    let inside = kept_replacement_beside_a_new_one(&mut client, &instance, shards, AT);
    let at_start = kept_replacement_beside_a_new_one(&mut client, &instance, shards, 0);
    drop(client);
    daemon.stop();
    assert_eq!(
      (inside.as_str(), at_start.as_str()),
      ("aaaa RRRR UUUU\n", "RRRR bbbb UUUU\n"),
      "{shards} shards"
    );
  }
}

/// Shape: the files of [`one_ask_history`], each [`HISTORY_SLOTS`] slots of [`HISTORY_SLOT`] bytes.
const HISTORY_FILES: [&str; 2] = ["h0", "h1"];
/// Shape: see [`HISTORY_FILES`].
const HISTORY_SLOTS: u64 = 4;
/// Shape: see [`HISTORY_FILES`].
const HISTORY_SLOT: u64 = 8;

/// One generated history of A-840 resolutions (the load harness's pattern, serial): `works` works over one green, each
/// writing three slots drawn by `seed`, submitted in a drawn order; a conflict resolved in one ask, the windows given up
/// accumulated until it succeeds, the work's slots in them written again. Every accept's version must hold the work's
/// bytes in each of its slots; the green's end must equal the accepted works' slots replayed in version order. The
/// history so far, when either fails.
fn one_ask_history(
  client: &mut Client,
  instance: &str,
  shards: u16,
  seed: u64,
) -> Result<(), String> {
  let mut state = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
  let mut draw = |below: u64| {
    state = state
      .wrapping_mul(6_364_136_223_846_793_005)
      .wrapping_add(1_442_695_040_888_963_407);
    (state >> 33) % below
  };
  let green = green_off(client, instance, shards, &format!("hist-{seed}-green"));
  let (seeder, _) = client
    .create_work(green, &format!("hist-{seed}-seed"))
    .unwrap();
  let length = usize::try_from(HISTORY_SLOTS * HISTORY_SLOT).unwrap();
  let mut model: Vec<Vec<u8>> = HISTORY_FILES.iter().map(|_| vec![b'.'; length]).collect();
  for file in HISTORY_FILES {
    client.edit(seeder, file, 0, 0, &model[0]).unwrap();
  }
  accepted(client, seeder);
  let works = 6;
  let mut said = Vec::new();
  let mut plans = Vec::new();
  for index in 0..works {
    let (work, _) = client
      .create_work(green, &format!("hist-{seed}-{index}"))
      .unwrap();
    let mut slots = std::collections::BTreeSet::new();
    while slots.len() < 3 {
      slots.insert((draw(2) as usize, draw(HISTORY_SLOTS) * HISTORY_SLOT));
    }
    let bytes =
      format!("w{index}......").into_bytes()[..usize::try_from(HISTORY_SLOT).unwrap()].to_vec();
    for (file, at) in &slots {
      client
        .edit(work, HISTORY_FILES[*file], *at, HISTORY_SLOT, &bytes)
        .unwrap();
    }
    said.push(format!("work {index} slots {slots:?}"));
    plans.push((
      work,
      slots,
      bytes,
      Vec::<slates_ipc::protocol::MergeWindow>::new(),
      false,
    ));
  }
  let mut steps = 0;
  while plans.iter().any(|plan| !plan.4) {
    steps += 1;
    if steps > 200 {
      return Err(format!("no progress\n{}", said.join("\n")));
    }
    let open: Vec<usize> = (0..works).filter(|index| !plans[*index].4).collect();
    let index = open[draw(open.len() as u64) as usize];
    let (work, slots, bytes, _, _) = plans[index].clone();
    match client.submit(work).unwrap() {
      Submitted::Accepted(version) => {
        said.push(format!("work {index} accepted at {version}"));
        for (file, at) in &slots {
          let at_usize = usize::try_from(*at).unwrap();
          model[*file][at_usize..at_usize + bytes.len()].copy_from_slice(&bytes);
          let held = client
            .read(green, HISTORY_FILES[*file], ReadAt::Version { version })
            .unwrap();
          if held[at_usize..at_usize + bytes.len()] != bytes[..] {
            return Err(format!(
              "version {version} lacks work {index}'s {}@{at}: {:?}\n{}",
              HISTORY_FILES[*file],
              String::from_utf8_lossy(&held),
              said.join("\n")
            ));
          }
        }
        plans[index].4 = true;
      }
      Submitted::Conflict(windows) => {
        said.push(format!("work {index} conflict {windows:?}"));
        let mut over = windows;
        loop {
          let replacement = slots
            .iter()
            .filter(|(file, at)| {
              over.iter().any(|w| {
                w.path == HISTORY_FILES[*file]
                  && *at < w.at + w.len.max(1)
                  && w.at < at + HISTORY_SLOT
              })
            })
            .map(|(file, at)| WorkChange::Edit {
              path: HISTORY_FILES[*file].to_owned(),
              at: *at,
              delete_len: HISTORY_SLOT,
              bytes: bytes.clone(),
            })
            .collect();
          match client
            .resolve_conflict(work, over.clone(), replacement)
            .unwrap()
          {
            Rebased::Rebased(head) => {
              said.push(format!("work {index} resolved over {over:?} at {head}"));
              break;
            }
            Rebased::Conflict(more) => {
              said.push(format!("work {index} resolution kept conflicting {more:?}"));
              for window in more {
                if !over.contains(&window) {
                  over.push(window);
                }
              }
            }
          }
        }
      }
      other => return Err(format!("work {index}: {other:?}\n{}", said.join("\n"))),
    }
    // A rival write between steps, so resolutions meet a moving head.
    if draw(2) == 0 {
      let (rival, _) = client
        .create_work(green, &format!("hist-{seed}-r{steps}"))
        .unwrap();
      let file = draw(2) as usize;
      let at = draw(HISTORY_SLOTS) * HISTORY_SLOT;
      let rival_bytes =
        format!("r{steps}........").into_bytes()[..usize::try_from(HISTORY_SLOT).unwrap()].to_vec();
      client
        .edit(rival, HISTORY_FILES[file], at, HISTORY_SLOT, &rival_bytes)
        .unwrap();
      let version = accepted(client, rival);
      let at_usize = usize::try_from(at).unwrap();
      model[file][at_usize..at_usize + rival_bytes.len()].copy_from_slice(&rival_bytes);
      said.push(format!(
        "rival wrote {}@{at} at {version}",
        HISTORY_FILES[file]
      ));
    }
  }
  for (file, expected) in HISTORY_FILES.iter().zip(&model) {
    let head = client.read(green, file, ReadAt::Head).unwrap();
    if &head != expected {
      return Err(format!(
        "{file}: head {:?}, replay {:?}\n{}",
        String::from_utf8_lossy(&head),
        String::from_utf8_lossy(expected),
        said.join("\n")
      ));
    }
  }
  Ok(())
}

/// A-840's oracle. Do: generated histories of works resolving conflicts in one ask under rival writes, on one shard
/// and on eight. Expect every accepted version to hold its work's slots and the green to end as the replay of the
/// accepted works in version order.
#[test]
fn one_ask_resolutions_land_exactly_what_the_works_wrote() {
  for shards in [ONE_SHARD, MANY_SHARDS] {
    let (daemon, instance) = daemon(shards, &format!("hist-{shards}"));
    let mut client = connect(&instance);
    let failed: Vec<String> = (0..24)
      .filter_map(|seed| one_ask_history(&mut client, &instance, shards, seed).err())
      .collect();
    drop(client);
    daemon.stop();
    assert!(
      failed.is_empty(),
      "{shards} shards: {} failed; the first:\n{}",
      failed.len(),
      failed.first().map_or("", String::as_str)
    );
  }
}

/// A-840, the generated oracle's first failure cut down: a work writes a file's first slot and its third; a rival takes
/// the third; the work gives up only that window, writes it again, and lands. The green's file after.
fn kept_first_slot(client: &mut Client, instance: &str, shards: u16, kept_at: u64) -> String {
  let tag = format!("first-{kept_at}");
  let green = green_off(client, instance, shards, &format!("{tag}-green"));
  let (seed, _) = client.create_work(green, &format!("{tag}-seed")).unwrap();
  client.edit(seed, "h1", 0, 0, &[b'.'; 32]).unwrap();
  accepted(client, seed);
  let (work, _) = client.create_work(green, &format!("{tag}-work")).unwrap();
  client.edit(work, "h1", kept_at, 8, b"w4......").unwrap();
  client.edit(work, "h1", 16, 8, b"w4......").unwrap();
  let (rival, _) = client.create_work(green, &format!("{tag}-rival")).unwrap();
  client.edit(rival, "h1", 16, 8, b"r1......").unwrap();
  accepted(client, rival);
  let Submitted::Conflict(windows) = client.submit(work).unwrap() else {
    panic!("the third slot conflicts");
  };
  // Every own slot a window meets is written again: adjacent writes are one operation, whose window spans both.
  let replacement = [kept_at, 16]
    .into_iter()
    .filter(|at| windows.iter().any(|w| *at < w.at + w.len && w.at < at + 8))
    .map(|at| WorkChange::Edit {
      path: "h1".to_owned(),
      at,
      delete_len: 8,
      bytes: b"w4......".to_vec(),
    })
    .collect();
  assert!(matches!(
    client.resolve_conflict(work, windows, replacement).unwrap(),
    Rebased::Rebased(_)
  ));
  accepted(client, work);
  String::from_utf8_lossy(&client.read(green, "h1", ReadAt::Head).unwrap()).into_owned()
}

/// A-840. Do: give up one slot of a file in one ask while keeping another, at the file's start or inside it, then land.
/// Expect the kept slot's bytes in the green, on one shard and on eight.
#[test]
fn a_kept_slot_beside_a_given_up_one_lands_its_bytes() {
  for shards in [ONE_SHARD, MANY_SHARDS] {
    let (daemon, instance) = daemon(shards, &format!("first-{shards}"));
    let mut client = connect(&instance);
    let at_start = kept_first_slot(&mut client, &instance, shards, 0);
    let inside = kept_first_slot(&mut client, &instance, shards, 8);
    drop(client);
    daemon.stop();
    assert_eq!(
      (at_start.as_str(), inside.as_str()),
      (
        "w4..............w4..............",
        "........w4......w4.............."
      ),
      "{shards} shards"
    );
  }
}
