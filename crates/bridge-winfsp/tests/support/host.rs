//! A-198's taken-request boundary through the real WinFsp driver. The parent is the anchor, and two re-invoked
//! processes serve the same held control handle in turn. The first explicitly retains a request taken by the DLL;
//! killing it must not strand that request. A separate process performs the Windows read and must return its bytes
//! or ERROR_OPERATION_ABORTED after the successor starts. Each role has a distinct exit, and every process is reaped.
//! The small volume is reconstructed with the same fixed file in each role: this tests transport settlement, while
//! CLI's twenty-kill oracle establishes persistence and mapped-view coherence through the real daemon.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::indexing_slicing
)]

use super::*;
use std::ffi::{OsStr, OsString};
use std::io::Read;
use std::os::windows::fs::OpenOptionsExt;
use std::time::{Duration, Instant};

use slates_anchor::handles::{ENV_HANDLES, HandleMessage, WHAT_WINFSP_MOUNT, parse_env};
use slates_anchor::held_windows::{Channel, Held, channel, handle_from_value, handle_value};
use slates_bridge_core::{Attachments, Rights, View, VolumeBridge};
use slates_db::catalog::{Principal, VolumeId};
use slates_ipc::win_spawn::{
  Process, Spawn, StandardOutput, duplicate_inheritable, environment_block,
};
use slates_mem::arena::ChunkArena;
use slates_mem::region::Region;
use slates_vfs::clock::HostClock;
use slates_vfs::names::NameEquivalence;
use slates_vfs::quota::Quota;
use slates_vfs::volume::{Store, StoreConfig, Volume, VolumeConfig};
use windows_sys::Win32::Foundation::{ERROR_OPERATION_ABORTED, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_NO_BUFFERING;
use windows_sys::Win32::System::Threading::{CreateEventW, SetEvent, WaitForSingleObject};

/// Format: the role selected by this fixture, the inherited channel's hex handle and its drive text.
const ROLE: &str = "SLATES_WINFSP_REQUEST_ROLE";
/// Format: the channel handle for the first server, before any held handoff exists.
const CHANNEL: &str = "SLATES_WINFSP_REQUEST_CHANNEL";
/// Format: the drive the fixed-file fixture serves.
const DRIVE: &str = "SLATES_WINFSP_REQUEST_DRIVE";
/// Shape: a role's completed exit, different from libtest selecting no test.
const ROLE_EXIT: i32 = 3;
/// Shape: the wait shared with the existing cross-process anchor fixture, ten liveness budgets.
const WAIT: Duration = Duration::from_secs(10);
/// Derived: a server outlives the parent's whole read wait. Otherwise its own idle timeout could delete the volume
/// and cancel a stranded read just before the parent's deadline, falsely certifying takeover settlement.
const ROLE_WAIT: Duration = WAIT.saturating_mul(2);
/// Shape: the event and pipe wait in Win32's milliseconds, derived from WAIT.
const WAIT_MS: u32 = 10_000;
/// Shape: the cadence of this test's outcome polls, as in CLI's live-mount oracle.
const POLL: Duration = Duration::from_millis(20);
/// Shape: the fixed read file and the volume's page unit.
const PAGE: usize = 4096;
/// Shape: a region for the fixed file and directory, matching the bridge-core data-change fixture.
const REGION_PAGES: usize = 1024;
/// Format: the one held mount entry, and two stage markers used only on the fixture's private pipe.
const ENTRY: u64 = 1;
/// Format: a private Released marker: the first server has taken the read and retains it unanswered.
const READ_TAKEN: u64 = ENTRY + 1;
/// Format: a private Released marker: the successor has started on the held control handle.
const SUCCESSOR_READY: u64 = READ_TAKEN + 1;
/// Format: this module's full libtest role name; exact filtering must execute it, never zero tests.
const ROLE_TEST: &str = "host::tests::request_role";
/// Format: a private Released marker: a purge's open reached the purging server, which retains it unanswered.
const PURGE_TAKEN: u64 = SUCCESSOR_READY + 1;
/// Format: a private Released marker: the closing server has ended an open on the shard and retains the Close's reply.
const CLOSE_TAKEN: u64 = PURGE_TAKEN + 1;
/// Format: a private Released marker base: the closing successor's recovery, plus how many replayed Cleanups and Closes
/// the file system refused.
const CLOSE_REFUSALS: u64 = CLOSE_TAKEN + 1;
/// Shape: the refusal counts the marker range above carries; a run refusing more is a failure anyway.
const CLOSE_REFUSAL_RANGE: u64 = 16;
/// Format: a private Released marker: the holding successor served the Close of the open the dead daemon made.
const HOLD_CLOSED: u64 = CLOSE_REFUSALS + CLOSE_REFUSAL_RANGE;
/// Format: a private Released marker: the holding successor refused that open's Cleanup or Close.
const HOLD_CLOSE_REFUSED: u64 = HOLD_CLOSED + 1;
/// Format: the holding client's inherited event, as a hex handle value, set once its read has returned.
const READ_DONE: &str = "SLATES_WINFSP_REQUEST_READ_DONE";
/// Format: the holding client's inherited event, as a hex handle value, set once it may close.
const STOP: &str = "SLATES_WINFSP_REQUEST_STOP";
/// Derived: the rule's bound (§4.6, 2026-11-06): a killed daemon exits within one transaction timeout, the longest its
/// receive worker's control wait lasts (`crate::dispatcher::TRANSACT_TIMEOUT_MS`).
const EXIT_WITHIN_MS: u32 = crate::dispatcher::TRANSACT_TIMEOUT_MS;
/// Format: the purge client role's full libtest name (`crate::purge_client::serve_from_env` in a test binary).
const PURGE_CLIENT_TEST: &str = "host::tests::purge_client_role";

struct Child {
  process: Process,
}

impl Child {
  fn wait(&mut self) -> Option<i32> {
    // SAFETY: the process handle stays owned throughout this bounded wait.
    let waited = unsafe { WaitForSingleObject(self.process.raw_handle(), WAIT_MS) };
    if waited == WAIT_TIMEOUT {
      return None;
    }
    assert_eq!(waited, WAIT_OBJECT_0);
    self.process.try_wait().unwrap()
  }

  /// The exit code once the process ended within `ms`, or `None`.
  fn wait_within(&mut self, ms: u32) -> Option<i32> {
    // SAFETY: the process handle stays owned throughout this bounded wait.
    let waited = unsafe { WaitForSingleObject(self.process.raw_handle(), ms) };
    if waited == WAIT_TIMEOUT {
      return None;
    }
    assert_eq!(waited, WAIT_OBJECT_0);
    self.process.try_wait().unwrap()
  }

  fn kill_and_reap(&mut self) {
    if self.process.try_wait().unwrap().is_none() {
      self.process.kill().unwrap();
    }
    assert!(self.wait().is_some(), "the fixture must reap its child");
  }
}

impl Drop for Child {
  fn drop(&mut self) {
    self.kill_and_reap();
  }
}

fn spawn(
  role: &str,
  drive: &str,
  extra: &[(&OsStr, &OsStr)],
  handles: &[windows_sys::Win32::Foundation::HANDLE],
) -> Child {
  let program = std::env::current_exe().unwrap();
  let args: Vec<OsString> = ["--ignored", "--exact", ROLE_TEST, "--nocapture"]
    .into_iter()
    .map(OsString::from)
    .collect();
  let mut environment = vec![
    (OsStr::new(ROLE), OsStr::new(role)),
    (OsStr::new(DRIVE), OsStr::new(drive)),
  ];
  environment.extend_from_slice(extra);
  let block = environment_block(&environment);
  Child {
    process: slates_ipc::win_spawn::spawn(&Spawn {
      program: program.as_os_str(),
      args: &args,
      environment: Some(&block),
      inherited: handles,
      output: StandardOutput::Inherited,
      job: None,
      process_security: None,
    })
    .unwrap(),
  }
}

/// Teardown first ends every dispatcher process, then releases the anchor's controls, then ends the reader. Thus an
/// unanswered kernel read is cancelled by volume deletion even if an assertion failed in the parent.
struct Scenario {
  servers: Vec<Child>,
  held: Option<Held>,
  reader: Option<Child>,
  probe: Option<Child>,
}

impl Drop for Scenario {
  fn drop(&mut self) {
    for server in &mut self.servers {
      server.kill_and_reap();
    }
    self.held = None;
    for reader in self.reader.iter_mut().chain(self.probe.iter_mut()) {
      reader.kill_and_reap();
    }
  }
}

fn free_drive() -> String {
  // SAFETY: this argument-free query reads the current DOS drive mask.
  let used = unsafe { windows_sys::Win32::Storage::FileSystem::GetLogicalDrives() };
  let letter = ('E'..='Z')
    .find(|letter| used & (1 << (u32::from(*letter) - u32::from('A'))) == 0)
    .unwrap();
  format!("{letter}:")
}

/// Owns a value passed exactly once in the parent's explicit spawn list. parse_env rejects aliased handoff values.
fn inherited_handle(value: u64) -> OwnedHandle {
  let raw = handle_from_value(value).unwrap();
  // SAFETY: this role takes each inherited value once; it is a live owned duplicate in this process's handle table.
  let owned = unsafe { OwnedHandle::from_raw_handle(raw) };
  slates_ipc::win_spawn::clear_inheritance(&owned).unwrap();
  owned
}

/// A-198 Step 9.4. Do: kill the serving process after it has taken a Windows read, then start its successor on the
/// three held handles. Expect: the original read completes with its bytes or typed cancellation within WAIT.
#[test]
fn a_read_taken_before_the_kill_is_settled_after_takeover() {
  if std::env::var_os("WINFSP_TEST_MOUNT").is_none() {
    eprintln!("SKIP: the taken-request oracle needs WINFSP_TEST_MOUNT=1 and WinFsp installed");
    return;
  }
  let drive = free_drive();
  let (anchor, daemon) = channel().unwrap();
  let inherited = duplicate_inheritable(daemon.raw()).unwrap();
  let value = format!("{:x}", handle_value(inherited.raw()));
  let mut scenario = Scenario {
    servers: Vec::with_capacity(2),
    held: Some(Held::new(1)),
    reader: None,
    probe: None,
  };
  scenario.servers.push(spawn(
    "first",
    &drive,
    &[(OsStr::new(CHANNEL), OsStr::new(&value))],
    &[inherited.raw()],
  ));
  drop(inherited);
  let message = anchor
    .recv(WAIT_MS)
    .unwrap()
    .expect("the first mount's hold");
  let answer = scenario
    .held
    .as_mut()
    .unwrap()
    .apply(scenario.servers[0].process.raw_handle(), message);
  assert_eq!(answer, Some(HandleMessage::Held { id: ENTRY }));
  anchor.send(&answer.unwrap(), WAIT_MS).unwrap();
  scenario.reader = Some(spawn("reader", &drive, &[], &[]));
  assert_eq!(
    anchor.recv(WAIT_MS).unwrap(),
    Some(HandleMessage::Released { id: READ_TAKEN })
  );
  eprintln!("WINFSP INFLIGHT: first server took the read; killing it before its reply");
  scenario.servers[0].kill_and_reap();
  let handoff = scenario.held.as_ref().unwrap().handoff(&daemon).unwrap();
  scenario.servers.push(spawn(
    "successor",
    &drive,
    &[(OsStr::new(ENV_HANDLES), OsStr::new(handoff.env()))],
    &handoff.handles(),
  ));
  drop(handoff);
  assert_eq!(
    anchor.recv(WAIT_MS).unwrap(),
    Some(HandleMessage::Released {
      id: SUCCESSOR_READY
    })
  );
  scenario.probe = Some(spawn("probe", &drive, &[], &[]));
  assert_eq!(
    scenario.probe.as_mut().unwrap().wait(),
    Some(ROLE_EXIT),
    "the successor must answer a fresh request on the same held volume"
  );
  eprintln!("WINFSP INFLIGHT: successor answered a fresh read on the original volume");
  assert_eq!(
    scenario.reader.as_mut().unwrap().wait(),
    Some(ROLE_EXIT),
    "a taken read stayed unanswered after the successor started"
  );
}

/// The rule of 2026-11-06 (§4.6 Windows; A-190's purge, the open sibling of
/// `docs/bugs/2026-11-06-a-daemons-own-io-on-its-volume-strands-its-exit.md`). Do: mount, hold the volume with the
/// anchor, purge a file, retain the purge's open unanswered in the serving process, and kill that process. Expect: it
/// exits within one transaction timeout. A daemon thread's own open of its volume would sit in WinFsp's processing
/// queue, never cancelled, and its process could not exit while the anchor holds the volume.
#[test]
fn a_daemon_killed_while_its_purges_open_is_delivered_exits_within_a_transaction_timeout() {
  if std::env::var_os("WINFSP_TEST_MOUNT").is_none() {
    eprintln!("SKIP: the purge kill oracle needs WINFSP_TEST_MOUNT=1 and WinFsp installed");
    return;
  }
  let drive = free_drive();
  let (anchor, daemon) = channel().unwrap();
  let inherited = duplicate_inheritable(daemon.raw()).unwrap();
  let value = format!("{:x}", handle_value(inherited.raw()));
  let mut scenario = Scenario {
    servers: Vec::with_capacity(1),
    held: Some(Held::new(1)),
    reader: None,
    probe: None,
  };
  scenario.servers.push(spawn(
    "purger",
    &drive,
    &[(OsStr::new(CHANNEL), OsStr::new(&value))],
    &[inherited.raw()],
  ));
  drop(inherited);
  let message = anchor.recv(WAIT_MS).unwrap().expect("the purger's hold");
  let answer = scenario
    .held
    .as_mut()
    .unwrap()
    .apply(scenario.servers[0].process.raw_handle(), message);
  assert_eq!(answer, Some(HandleMessage::Held { id: ENTRY }));
  anchor.send(&answer.unwrap(), WAIT_MS).unwrap();
  assert_eq!(
    anchor.recv(WAIT_MS).unwrap(),
    Some(HandleMessage::Released { id: PURGE_TAKEN })
  );
  eprintln!("WINFSP PURGE KILL: the purge's open is delivered and retained; killing the server");
  let killed = Instant::now();
  scenario.servers[0].process.kill().unwrap();
  let exited = scenario.servers[0].wait_within(EXIT_WITHIN_MS);
  let waited = killed.elapsed();
  if exited.is_none() {
    // Delete the volume first so the stranded server can end and the fixture can reap it.
    scenario.held = None;
  }
  eprintln!("WINFSP PURGE KILL: exited {exited:?} after {waited:?}");
  assert!(
    exited.is_some(),
    "a daemon killed while its purge's open was delivered did not exit within {EXIT_WITHIN_MS} ms"
  );
}

fn fixed_volume(store: &mut Store) -> (Volume, OpContext) {
  let mut volume = Volume::create(
    store,
    VolumeConfig {
      prefix: 1,
      names: NameEquivalence::Exact,
      quota: Quota::Bounded { limit: 1 << 30 },
      journal_bytes: 1 << 16,
      clock: Box::new(HostClock::default()),
    },
  )
  .unwrap();
  let mut attachments = Attachments::new();
  let id = attachments
    .attach(
      VolumeId { bytes: [0; 16] },
      View::Current,
      Principal::Uid { uid: 0 },
      Rights {
        read: true,
        write: true,
      },
    )
    .unwrap();
  let cx = attachments.context(id).unwrap();
  let mut bridge = VolumeBridge::new(VolumeId { bytes: [0; 16] }, &mut volume, store);
  let root = bridge.root(&cx).unwrap();
  for name in ["file", "sentinel"] {
    let (file, handle) = bridge
      .create(ObjectId::new(root, 0), &cx, name, 0o644, 0)
      .unwrap();
    let object = ObjectId::new(file.ino, file.generation);
    bridge.write(object, &cx, 0, &[b'A'; PAGE]).unwrap();
    bridge.release(object, &cx, handle).unwrap();
  }
  (volume, cx)
}

/// Whether the role is a first server: it is handed the anchor's channel, and asks it to hold its mount.
fn is_first(role: &str) -> bool {
  matches!(role, "first" | "purger" | "closer-first" | "holder-first")
}

fn inherited_controls(role: &str) -> (Channel, Option<Inherited>) {
  if is_first(role) {
    let value = u64::from_str_radix(&std::env::var(CHANNEL).unwrap(), 16).unwrap();
    (Channel::adopt(inherited_handle(value)), None)
  } else {
    let (channel, mut entries) = parse_env(&std::env::var(ENV_HANDLES).unwrap(), 1).unwrap();
    let entry = entries.pop().unwrap();
    assert_eq!(entry.id, ENTRY);
    let mut values = entry.values.into_iter();
    let inherited = Inherited {
      volume: inherited_handle(values.next().unwrap()),
      link: inherited_handle(values.next().unwrap()),
      ledger: inherited_handle(values.next().unwrap()),
      record: Record::decode(&entry.record).unwrap(),
    };
    assert!(values.next().is_none());
    (Channel::adopt(inherited_handle(channel)), Some(inherited))
  }
}

fn await_ready(mount: &mut MountThread) -> Box<Ready> {
  let started = Instant::now();
  loop {
    assert!(started.elapsed() < WAIT, "the fixture's mount must start");
    if let Some(outcome) = mount.outcome() {
      match outcome {
        Outcome::Mounted(result) => return result.unwrap(),
        Outcome::Stopped => panic!("the fixture's mount stopped before serving"),
        _ => panic!("an unsolicited mount outcome"),
      }
    }
    #[allow(clippy::disallowed_methods)]
    std::thread::sleep(POLL);
  }
}

fn serve(role: &str, drive: &str) -> ! {
  let (channel, inherited) = inherited_controls(role);
  install_purge_client();
  let mut store = fixed_store();
  let (mut volume, cx) = fixed_volume(&mut store);
  let mut bridge = VolumeBridge::new(VolumeId { bytes: [0; 16] }, &mut volume, &mut store);
  let (send, receive) = sync_channel(REQUEST_BOUND);
  let mut mount = MountThread::start(
    drive,
    true,
    send,
    Waker::noop().clone(),
    slates_bridge_core::volume_bridge::MAX_OPEN_HANDLES,
    inherited,
  )
  .unwrap();
  serve_mounted(role, &channel, &receive, &mut mount, &mut bridge, &cx)
}

fn fixed_store() -> Store {
  let mut arena = ChunkArena::new(PAGE);
  arena
    .add_region(Region::map(PAGE * REGION_PAGES, PAGE, false).unwrap())
    .unwrap();
  Store::new(
    &StoreConfig {
      page: PAGE,
      cache_line: 128,
      max_dirs: 64,
      max_inodes: 256,
      max_chunks: REGION_PAGES,
      max_dir_blocks: 64,
      dir_cutover: 16,
    },
    arena,
    0,
  )
}

fn serve_mounted(
  role: &str,
  channel: &Channel,
  receive: &Receiver<Request>,
  mount: &mut MountThread,
  bridge: &mut dyn Bridge,
  cx: &OpContext,
) -> ! {
  let mut ready = await_ready(mount);
  if is_first(role) {
    channel
      .send(
        &HandleMessage::Hold {
          id: ENTRY,
          what: WHAT_WINFSP_MOUNT,
          values: ready.handle_values().unwrap(),
          record: ready.offer.record.encode().unwrap(),
        },
        WAIT_MS,
      )
      .unwrap();
    assert_eq!(
      channel.recv(WAIT_MS).unwrap(),
      Some(HandleMessage::Held { id: ENTRY })
    );
  } else {
    channel
      .send(
        &HandleMessage::Released {
          id: SUCCESSOR_READY,
        },
        WAIT_MS,
      )
      .unwrap();
  }
  let mut names = OpenNames::new();
  let mut close_reported = false;
  let mut recovery_counted = false;
  if role == "purger" {
    let root = ObjectId::new(bridge.root(cx).unwrap(), 0);
    let file = bridge.lookup(root, cx, "file").unwrap();
    mount
      .purge(vec![crate::purge::Purge {
        object: ObjectId::new(file.ino, file.generation),
        path: "\\file".to_owned(),
        offset: 0,
        len: u64::try_from(PAGE).unwrap(),
      }])
      .unwrap();
  }
  loop {
    // The closing successor's recovery is told while requests are served: it replays the Close through this shard.
    let request = next_request(receive, || {
      report_close_recovery(role, channel, mount, &mut close_reported);
    });
    report_close_recovery(role, channel, mount, &mut close_reported);
    let Some(request) = tell_if_holding(role, request, channel, bridge, cx, &mut names, &mut ready)
    else {
      continue;
    };
    count_recovery(role, &request, mount, &mut recovery_counted);
    if role == "purger" && matches!(request.op, Op::PurgeOpen { .. }) {
      channel
        .send(&HandleMessage::Released { id: PURGE_TAKEN }, WAIT_MS)
        .unwrap();
      // Retain the purge's open, its reply sender included, until the parent kills this process.
      let _retained = request;
      let answer = channel.recv(WAIT_MS).unwrap();
      panic!("the purger should have died while retaining the purge's open, got {answer:?}");
    }
    let request = retain_served_close(role, request, channel, bridge, cx, &mut names, &mut ready);
    if role == "first" && matches!(request.op, Op::Read { .. }) {
      channel
        .send(&HandleMessage::Released { id: READ_TAKEN }, WAIT_MS)
        .unwrap();
      // Retain the real taken request, including its reply sender, until the parent kills this process.
      let _retained = request;
      let answer = channel.recv(WAIT_MS).unwrap();
      panic!("the first server should have died while retaining its read, got {answer:?}");
    }
    assert!(
      request
        .serve(bridge, cx, &mut names, &mut ready.ledger)
        .send()
    );
  }
}

/// The closing server's Close: ended on the shard, as a live daemon ends it, its reply then retained until the parent
/// kills this process, so the successor replays a Close whose open is already ended. Any other request is handed back.
fn retain_served_close(
  role: &str,
  request: Request,
  channel: &Channel,
  bridge: &mut dyn Bridge,
  cx: &OpContext,
  names: &mut OpenNames,
  ready: &mut Ready,
) -> Request {
  if role != "closer-first" || !matches!(request.op, Op::Release { .. }) {
    return request;
  }
  let _retained = request.serve(bridge, cx, names, &mut ready.ledger);
  channel
    .send(&HandleMessage::Released { id: CLOSE_TAKEN }, WAIT_MS)
    .unwrap();
  let answer = channel.recv(WAIT_MS).unwrap();
  panic!("the closing server should have died while retaining its Close, got {answer:?}");
}

/// The fixture's next request. It polls, so a role can watch its mount's outcomes between requests.
fn next_request(receive: &Receiver<Request>, mut between: impl FnMut()) -> Request {
  let started = Instant::now();
  loop {
    match receive.recv_timeout(POLL) {
      Ok(request) => return request,
      Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
        assert!(started.elapsed() < ROLE_WAIT, "the fixture's next request");
        between();
      }
      Err(error) => panic!("the fixture's request channel: {error}"),
    }
  }
}

/// The closing successor's recovery, told to the parent once it is known: how many replayed Cleanups and Closes were
/// refused. Recovery replays the captured Close through this shard, so it is known only while requests are served.
fn report_close_recovery(
  role: &str,
  channel: &Channel,
  mount: &mut MountThread,
  reported: &mut bool,
) {
  if role != "closer-successor" || *reported {
    return;
  }
  while let Some(outcome) = mount.outcome() {
    match outcome {
      Outcome::Recovered { settled, refused } => {
        eprintln!(
          "WINFSP CLOSE: the successor settled {settled} captured requests, {refused} refused"
        );
        assert!(settled > 0, "the successor must settle the captured Close");
        channel
          .send(
            &HandleMessage::Released {
              id: CLOSE_REFUSALS + refused.min(CLOSE_REFUSAL_RANGE - 1),
            },
            WAIT_MS,
          )
          .unwrap();
        *reported = true;
        return;
      }
      Outcome::Failed(error) => panic!("a receive worker failed: {error}"),
      Outcome::Stopped => panic!("the mount stopped before its recovery was told"),
      _ => {}
    }
  }
}

/// The holding successor's Cleanup and Close of the open the dead daemon made: served here, the reply sent, and told
/// to the parent, a refusal at once and the Close once served. Any other request is handed back to be served.
fn tell_if_holding(
  role: &str,
  request: Request,
  channel: &Channel,
  bridge: &mut dyn Bridge,
  cx: &OpContext,
  names: &mut OpenNames,
  ready: &mut Ready,
) -> Option<Request> {
  if role != "holder-successor" || !matches!(request.op, Op::Cleanup { .. } | Op::Release { .. }) {
    return Some(request);
  }
  let close = matches!(request.op, Op::Release { .. });
  let served = request.serve(bridge, cx, names, &mut ready.ledger);
  let refused = if let Reply::Err(status) = served.reply {
    eprintln!(
      "WINFSP HOLD: the successor refused the held open's {} with NTSTATUS {status:#x}",
      if close { "Close" } else { "Cleanup" }
    );
    true
  } else {
    false
  };
  assert!(served.send());
  let marker = if refused {
    Some(HOLD_CLOSE_REFUSED)
  } else if close {
    eprintln!("WINFSP HOLD: the successor served the held open's Close");
    Some(HOLD_CLOSED)
  } else {
    None
  };
  if let Some(id) = marker {
    channel
      .send(&HandleMessage::Released { id }, WAIT_MS)
      .unwrap();
  }
  None
}

fn count_recovery(role: &str, request: &Request, mount: &mut MountThread, counted: &mut bool) {
  if role != "successor" || *counted || !matches!(request.op, Op::Read { .. }) {
    return;
  }
  let Some(Outcome::Recovered { settled: count, .. }) = mount.outcome() else {
    panic!("a fresh read preceded the captured-request outcome");
  };
  assert!(
    count > 0,
    "the successor must settle the retained read before serving another"
  );
  *counted = true;
  eprintln!("WINFSP INFLIGHT: successor counted {count} settled captured requests");
}

/// A-212, audit step 7's profile (2026-11-06). Do: rename a file whose one open Windows holds, through the receiver's
/// durable name update, against a shard that counts its turns. Expect: the open's name is the new one, in the ledger
/// and in the names Windows' notifications use, after one turn. The update spent a second turn only to learn that no
/// open was left beneath the old name: 2.27 turns a rename on arm64 in diagnostics run 42178889880, 4.7 µs each.
#[test]
fn a_rename_with_one_open_name_takes_one_shard_turn() {
  let (send, receive) = sync_channel::<Request>(REQUEST_BOUND);
  let shard = std::thread::spawn(move || {
    let mut store = fixed_store();
    let (mut volume, cx) = fixed_volume(&mut store);
    let mut bridge = VolumeBridge::new(VolumeId { bytes: [0; 16] }, &mut volume, &mut store);
    let root = ObjectId::new(bridge.root(&cx).unwrap(), 0);
    let file = bridge.lookup(root, &cx, "file").unwrap();
    let object = ObjectId::new(file.ino, file.generation);
    let fh = bridge.open(object, &cx, 0).unwrap();
    let memory =
      SparseObject::unnamed(crate::opens::bytes(2).unwrap(), slates_mem::Words::new()).unwrap();
    let mut ledger = Ledger::create(memory, 2).unwrap();
    let context = ledger
      .opened(
        Open {
          object,
          fh,
          purge: false,
        },
        "\\file",
      )
      .unwrap();
    let mut names = OpenNames::new();
    names.opened(context, file.ino, "\\file");
    let mut turns = 0u32;
    while let Ok(request) = receive.recv() {
      turns += 1;
      assert!(
        request
          .serve(&mut bridge, &cx, &mut names, &mut ledger)
          .send()
      );
    }
    (
      turns,
      ledger.name(context).unwrap(),
      names.first_beneath("\\renamed"),
      names.first_beneath("\\file"),
      context,
      file.ino,
    )
  });
  crate::host::rename_open_names(&send, Waker::noop(), "\\file", "\\renamed", 2).unwrap();
  drop(send);
  let (turns, ledgered, renamed, left, context, inode) = shard.join().unwrap();
  assert_eq!(ledgered, "\\renamed");
  assert_eq!(renamed, Some((context, inode)));
  assert_eq!(left, None);
  assert_eq!(
    turns, 1,
    "a rename with one open name took {turns} shard turns"
  );
}

/// A-198. Do: replay an open's delete cleanup after its first cleanup unlinked the name and an SDK created a new file
/// there. Expect: the replacement still has its name and bytes; cleanup belongs to the old file's identity.
#[test]
fn a_replayed_cleanup_cannot_unlink_the_file_that_replaced_its_open() {
  let mut store = fixed_store();
  let (mut volume, cx) = fixed_volume(&mut store);
  let mut bridge = VolumeBridge::new(VolumeId { bytes: [0; 16] }, &mut volume, &mut store);
  let root = ObjectId::new(bridge.root(&cx).unwrap(), 0);
  let original = bridge.lookup(root, &cx, "file").unwrap();
  let object = ObjectId::new(original.ino, original.generation);
  let fh = bridge.open(object, &cx, 0).unwrap();
  let memory =
    SparseObject::unnamed(crate::opens::bytes(2).unwrap(), slates_mem::Words::new()).unwrap();
  let mut ledger = Ledger::create(memory, 2).unwrap();
  let context = ledger
    .opened(
      Open {
        object,
        fh,
        purge: false,
      },
      "\\file",
    )
    .unwrap();
  cleanup_once(&mut bridge, &cx, &mut ledger, context);
  let (replacement, handle) = bridge.create(root, &cx, "file", 0o644, 0).unwrap();
  bridge
    .write(
      ObjectId::new(replacement.ino, replacement.generation),
      &cx,
      0,
      b"replacement",
    )
    .unwrap();
  cleanup_once(&mut bridge, &cx, &mut ledger, context);
  let found = bridge.lookup(root, &cx, "file").unwrap();
  assert_eq!(found.ino, replacement.ino);
  let mut bytes = Vec::new();
  bridge
    .read(
      ObjectId::new(found.ino, found.generation),
      &cx,
      0,
      11,
      &mut bytes,
    )
    .unwrap();
  assert_eq!(bytes, b"replacement");
  bridge.release(object, &cx, fh).unwrap();
  bridge
    .release(ObjectId::new(found.ino, found.generation), &cx, handle)
    .unwrap();
}

fn cleanup_once(
  bridge: &mut dyn Bridge,
  cx: &OpContext,
  ledger: &mut Ledger<SparseObject>,
  context: u64,
) {
  let (reply, answer) = sync_channel(1);
  let request = Request {
    op: Op::Cleanup {
      object: context,
      delete: Some("\\file".to_owned()),
    },
    reply,
    hint: 0,
    replayed: false,
  };
  assert!(
    request
      .serve(bridge, cx, &mut OpenNames::new(), ledger)
      .send()
  );
  let _reply = answer.recv().unwrap();
}

/// GAPS, "A private purge's intent can wait up to the 1 s transaction timeout". Do: on an idle drive, purge four files
/// whose names the kernel refuses before any request reaches the host (a component one unit longer than the volume
/// allows), so each purge's Begin and End are published with nothing received between them. Expect: the round's outcome
/// within one transaction timeout. Before, every publish past the channel's two waited for the idle receive to time out.
#[test]
fn purges_the_kernel_refuses_on_an_idle_drive_do_not_wait_out_the_transaction_timeout() {
  if std::env::var_os("WINFSP_TEST_MOUNT").is_none() {
    eprintln!("SKIP: the idle purge oracle needs WINFSP_TEST_MOUNT=1 and WinFsp installed");
    return;
  }
  install_purge_client();
  let drive = free_drive();
  let mut store = fixed_store();
  let (mut volume, cx) = fixed_volume(&mut store);
  let mut bridge = VolumeBridge::new(VolumeId { bytes: [0; 16] }, &mut volume, &mut store);
  let (send, receive) = sync_channel(REQUEST_BOUND);
  let mut mount = MountThread::start(
    &drive,
    false,
    send,
    Waker::noop().clone(),
    slates_bridge_core::volume_bridge::MAX_OPEN_HANDLES,
    None,
  )
  .unwrap();
  // Declared after the mount, so dropped before it: the mount's thread joins only once the request channel's receiver
  // is gone (`MountThread::stop`), and a dispatcher waiting for a reply nobody serves would hold the join forever.
  let receive = receive;
  let mut ready = await_ready(&mut mount);
  let root = ObjectId::new(bridge.root(&cx).unwrap(), 0);
  let file = bridge.lookup(root, &cx, "file").unwrap();
  let refused = "a".repeat(usize::from(MAX_COMPONENT_LENGTH) + 1);
  let purges: Vec<crate::purge::Purge> = (0..4)
    .map(|_| crate::purge::Purge {
      object: ObjectId::new(file.ino, file.generation),
      path: format!("\\{refused}"),
      offset: 0,
      len: u64::try_from(PAGE).unwrap(),
    })
    .collect();
  let mut names = OpenNames::new();
  let started = Instant::now();
  mount.purge(purges).unwrap();
  let purged = loop {
    match mount.outcome() {
      Some(Outcome::Purged(purged)) => break purged,
      Some(Outcome::Failed(error)) => panic!("the receive worker failed: {error}"),
      Some(Outcome::Stopped) => panic!("the mount stopped during the purge round"),
      Some(_) | None => {}
    }
    match receive.recv_timeout(POLL) {
      Ok(request) => assert!(
        request
          .serve(&mut bridge, &cx, &mut names, &mut ready.ledger)
          .send()
      ),
      Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
      Err(error) => panic!("the fixture's request channel: {error}"),
    }
    assert!(started.elapsed() < ROLE_WAIT, "the purge round never ended");
  };
  let took = started.elapsed();
  eprintln!(
    "WINFSP IDLE PURGE: {} purges whose opens the kernel refused took {took:?}",
    purged.outcomes.len()
  );
  assert_eq!(purged.outcomes.len(), 4, "every purge must be made");
  let timeout = Duration::from_millis(u64::from(crate::dispatcher::TRANSACT_TIMEOUT_MS));
  assert!(
    took < timeout,
    "the purge round took {took:?}, waiting out the idle receive's {timeout:?} transaction timeout"
  );
}

/// This test binary as a mount's purge client, the role `slates winfsp-purge-client` plays for the daemon.
fn install_purge_client() {
  crate::purge_client::install(crate::purge_client::Program {
    program: std::env::current_exe().unwrap().into_os_string(),
    args: ["--ignored", "--exact", PURGE_CLIENT_TEST, "--nocapture"]
      .into_iter()
      .map(OsString::from)
      .collect(),
  });
}

/// The rule of 2026-11-06. The re-invoked purge client: its exit is the role's own.
#[test]
#[ignore = "a process role for the purge client"]
fn purge_client_role() {
  if std::env::var_os(crate::purge_client::PURGE_CLIENT_ENV).is_none() {
    return;
  }
  std::process::exit(crate::purge_client::serve_from_env());
}

/// A-198. The re-invoked mount server and reader; the parent requires the distinct ROLE_EXIT for a completed read.
#[test]
#[ignore = "a process role for the live taken-request oracle"]
fn request_role() {
  let role = std::env::var(ROLE).unwrap();
  let drive = std::env::var(DRIVE).unwrap();
  if role == "closer" || role == "holder" {
    closer(&role, &drive);
  }
  if role != "reader" && role != "probe" {
    serve(&role, &drive);
  }
  let name = if role == "probe" { "sentinel" } else { "file" };
  let mut file = std::fs::File::open(format!("{drive}\\{name}")).unwrap();
  let mut bytes = [0; PAGE];
  match file.read_exact(&mut bytes) {
    Ok(()) => {
      assert_eq!(bytes, [b'A'; PAGE]);
      eprintln!("WINFSP INFLIGHT: {role} read completed with its file's bytes");
    }
    Err(error) => {
      assert_eq!(
        role, "reader",
        "the successor's fresh read must return bytes: {error}"
      );
      assert_eq!(
        error.raw_os_error(),
        Some(i32::try_from(ERROR_OPERATION_ABORTED).unwrap()),
        "the read must return bytes or the driver's typed cancellation: {error}"
      );
      eprintln!("WINFSP INFLIGHT: reader read refused ERROR_OPERATION_ABORTED");
    }
  }
  std::process::exit(ROLE_EXIT);
}

/// A sector-aligned page for an unbuffered read, which goes to the server every time.
#[repr(align(4096))]
struct AlignedPage([u8; PAGE]);

/// The closing client: opens the fixed file without the cache, so its Cleanup and Close reach the server, reads it, and
/// closes it. CloseHandle returns once the Cleanup is answered; Windows sends the Close when the file's last reference
/// goes, at once here, since nothing caches it. The holding client sets its read event and waits for its stop event
/// before it closes, so its handle stays open across the kill. CloseHandle must succeed whatever happened to the daemon.
fn closer(role: &str, drive: &str) -> ! {
  let mut file = std::fs::OpenOptions::new()
    .read(true)
    .custom_flags(FILE_FLAG_NO_BUFFERING)
    .open(format!("{drive}\\file"))
    .unwrap();
  let mut page = AlignedPage([0; PAGE]);
  file.read_exact(&mut page.0).unwrap();
  assert!(page.0.iter().all(|byte| *byte == b'A'));
  if role == "holder" {
    let read =
      inherited_handle(u64::from_str_radix(&std::env::var(READ_DONE).unwrap(), 16).unwrap());
    let stop = inherited_handle(u64::from_str_radix(&std::env::var(STOP).unwrap(), 16).unwrap());
    // SAFETY: the inherited event is owned and live.
    assert_ne!(unsafe { SetEvent(read.as_raw_handle()) }, 0);
    let within = u32::try_from(ROLE_WAIT.as_millis()).unwrap();
    // SAFETY: the inherited event is owned and live.
    let waited = unsafe { WaitForSingleObject(stop.as_raw_handle(), within) };
    assert_eq!(
      waited, WAIT_OBJECT_0,
      "the holding client was never told to close"
    );
  }
  let handle = std::os::windows::io::IntoRawHandle::into_raw_handle(file);
  // SAFETY: the handle this process just opened and owns, closed once.
  let closed = unsafe { windows_sys::Win32::Foundation::CloseHandle(handle) };
  assert_ne!(
    closed,
    0,
    "CloseHandle failed: {}",
    std::io::Error::last_os_error()
  );
  eprintln!("WINFSP CLOSE: the {role} client's CloseHandle succeeded");
  std::process::exit(ROLE_EXIT);
}

/// A-198, A-219. Do: a client opens the file uncached, reads it and closes it; the serving process ends the open on the
/// shard when the Close arrives (after CloseHandle has returned at the Cleanup) and is killed before the Close's reply
/// reaches its worker; a successor takes over and replays the captured Close. Expect: the client's CloseHandle succeeds,
/// and the successor's recovery settles the Close with no refusal, since the open the dead daemon already ended is that
/// Close done (it was refused `Stale`, run 42165934408).
#[test]
fn a_close_served_before_the_kill_is_answered_done_after_takeover() {
  if std::env::var_os("WINFSP_TEST_MOUNT").is_none() {
    eprintln!("SKIP: the replayed-close oracle needs WINFSP_TEST_MOUNT=1 and WinFsp installed");
    return;
  }
  let drive = free_drive();
  let (anchor, daemon) = channel().unwrap();
  let inherited = duplicate_inheritable(daemon.raw()).unwrap();
  let value = format!("{:x}", handle_value(inherited.raw()));
  let mut scenario = Scenario {
    servers: Vec::with_capacity(2),
    held: Some(Held::new(1)),
    reader: None,
    probe: None,
  };
  scenario.servers.push(spawn(
    "closer-first",
    &drive,
    &[(OsStr::new(CHANNEL), OsStr::new(&value))],
    &[inherited.raw()],
  ));
  drop(inherited);
  let message = anchor
    .recv(WAIT_MS)
    .unwrap()
    .expect("the first mount's hold");
  let answer = scenario
    .held
    .as_mut()
    .unwrap()
    .apply(scenario.servers[0].process.raw_handle(), message);
  assert_eq!(answer, Some(HandleMessage::Held { id: ENTRY }));
  anchor.send(&answer.unwrap(), WAIT_MS).unwrap();
  scenario.reader = Some(spawn("closer", &drive, &[], &[]));
  assert_eq!(
    anchor.recv(WAIT_MS).unwrap(),
    Some(HandleMessage::Released { id: CLOSE_TAKEN })
  );
  eprintln!("WINFSP CLOSE: the server ended the open and holds the Close's reply; killing it");
  scenario.servers[0].kill_and_reap();
  let handoff = scenario.held.as_ref().unwrap().handoff(&daemon).unwrap();
  scenario.servers.push(spawn(
    "closer-successor",
    &drive,
    &[(OsStr::new(ENV_HANDLES), OsStr::new(handoff.env()))],
    &handoff.handles(),
  ));
  drop(handoff);
  assert_eq!(
    anchor.recv(WAIT_MS).unwrap(),
    Some(HandleMessage::Released {
      id: SUCCESSOR_READY
    })
  );
  let refused = match anchor.recv(WAIT_MS).unwrap() {
    Some(HandleMessage::Released { id })
      if (CLOSE_REFUSALS..CLOSE_REFUSALS + CLOSE_REFUSAL_RANGE).contains(&id) =>
    {
      id - CLOSE_REFUSALS
    }
    other => panic!("the successor's recovery report, got {other:?}"),
  };
  assert_eq!(
    refused, 0,
    "the successor refused a replayed Close whose open the dead daemon had already ended"
  );
  assert_eq!(
    scenario.reader.as_mut().unwrap().wait(),
    Some(ROLE_EXIT),
    "the client's CloseHandle must succeed across the takeover"
  );
}

/// An unnamed manual-reset event the test owns.
fn event() -> OwnedHandle {
  // SAFETY: an unnamed manual-reset event; no security descriptor or name is borrowed.
  let event = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
  assert!(!event.is_null(), "an event for the holding client");
  // SAFETY: CreateEventW returned this fresh handle; the value takes its ownership once.
  unsafe { OwnedHandle::from_raw_handle(event) }
}

/// A-198. Do: a client opens the file uncached and reads it under the first server; that server is killed with the
/// handle still open; a successor takes over the held volume; the client then closes the handle through it. Expect: the
/// successor serves the open's Cleanup and Close without a refusal, and the client's CloseHandle succeeds.
#[test]
fn a_handle_open_across_the_kill_closes_through_the_successor() {
  if std::env::var_os("WINFSP_TEST_MOUNT").is_none() {
    eprintln!("SKIP: the held-open close oracle needs WINFSP_TEST_MOUNT=1 and WinFsp installed");
    return;
  }
  let drive = free_drive();
  let (anchor, daemon) = channel().unwrap();
  let inherited = duplicate_inheritable(daemon.raw()).unwrap();
  let value = format!("{:x}", handle_value(inherited.raw()));
  let mut scenario = Scenario {
    servers: Vec::with_capacity(2),
    held: Some(Held::new(1)),
    reader: None,
    probe: None,
  };
  scenario.servers.push(spawn(
    "holder-first",
    &drive,
    &[(OsStr::new(CHANNEL), OsStr::new(&value))],
    &[inherited.raw()],
  ));
  drop(inherited);
  let message = anchor
    .recv(WAIT_MS)
    .unwrap()
    .expect("the first mount's hold");
  let answer = scenario
    .held
    .as_mut()
    .unwrap()
    .apply(scenario.servers[0].process.raw_handle(), message);
  assert_eq!(answer, Some(HandleMessage::Held { id: ENTRY }));
  anchor.send(&answer.unwrap(), WAIT_MS).unwrap();
  let read = event();
  let stop = event();
  let read_shared = duplicate_inheritable(read.as_raw_handle()).unwrap();
  let stop_shared = duplicate_inheritable(stop.as_raw_handle()).unwrap();
  let read_value = format!("{:x}", handle_value(read_shared.raw()));
  let stop_value = format!("{:x}", handle_value(stop_shared.raw()));
  scenario.reader = Some(spawn(
    "holder",
    &drive,
    &[
      (OsStr::new(READ_DONE), OsStr::new(&read_value)),
      (OsStr::new(STOP), OsStr::new(&stop_value)),
    ],
    &[read_shared.raw(), stop_shared.raw()],
  ));
  drop((read_shared, stop_shared));
  // SAFETY: the event is owned and live.
  let waited = unsafe { WaitForSingleObject(read.as_raw_handle(), WAIT_MS) };
  assert_eq!(
    waited, WAIT_OBJECT_0,
    "the holding client's read never returned"
  );
  eprintln!("WINFSP HOLD: the client holds an open it read through; killing the server");
  scenario.servers[0].kill_and_reap();
  let handoff = scenario.held.as_ref().unwrap().handoff(&daemon).unwrap();
  scenario.servers.push(spawn(
    "holder-successor",
    &drive,
    &[(OsStr::new(ENV_HANDLES), OsStr::new(handoff.env()))],
    &handoff.handles(),
  ));
  drop(handoff);
  assert_eq!(
    anchor.recv(WAIT_MS).unwrap(),
    Some(HandleMessage::Released {
      id: SUCCESSOR_READY
    })
  );
  // SAFETY: the event is owned and live.
  assert_ne!(unsafe { SetEvent(stop.as_raw_handle()) }, 0);
  assert_eq!(
    anchor.recv(WAIT_MS).unwrap(),
    Some(HandleMessage::Released { id: HOLD_CLOSED }),
    "the successor must serve the held open's Cleanup and Close without a refusal"
  );
  assert_eq!(
    scenario.reader.as_mut().unwrap().wait(),
    Some(ROLE_EXIT),
    "the client's CloseHandle must succeed through the successor"
  );
}
