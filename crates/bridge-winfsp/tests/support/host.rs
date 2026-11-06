//! A-198's taken-request boundary through the real WinFsp driver. The parent is the anchor, and two re-invoked
//! processes serve the same held control handle in turn. The first explicitly retains a request taken by the DLL;
//! killing it must not strand that request. A separate process performs the Windows read and must return its bytes
//! or ERROR_OPERATION_ABORTED after the successor starts. Each role has a distinct exit, and every process is reaped.
//! The small volume is reconstructed with the same fixed file in each role: this tests transport settlement, while
//! CLI's twenty-kill oracle establishes persistence and mapped-view coherence through the real daemon.
//!
//! A-219's oracle runs the same fixture under load, served by two receive workers (the library's tests run every mount
//! with the most, `crate::probe`): reader processes read their own files without the cache while the parent kills the
//! serving process again and again. Each successor first reports which workers' journals the kill left a batch in, so
//! the run shows that kills fell while the second worker held one.
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
/// Shape: kills in A-219's oracle, twice the takeover history's twenty, so each lane's kills fall in every part of both
/// workers' cycles.
const OVERLAP_KILLS: u32 = 40;
/// Shape: reader processes in A-219's oracle, more than one so both workers hold requests at once.
const OVERLAP_READERS: usize = 3;
/// Shape: the files each reader cycles over, so an open's wrong context reads another file's bytes.
const OVERLAP_FILES: usize = 4;
/// Shape: the steps of the pause before each kill, in POLLs, so kills fall at different points of a cycle.
const OVERLAP_PAUSES: u32 = 4;
/// Format: a private Released marker base, after the purge oracle's PURGE_TAKEN: a successor's reading of which journals
/// the kill left a batch in, plus a set with bit `n` for worker `n`'s journal.
const LANDING: u64 = PURGE_TAKEN + 1;
/// Format: every set of journals holding a request, one bit a worker.
const LANDINGS: usize = 1 << crate::inflight::capture::MAX_WORKERS;
/// Format: a private Released marker: the serving process's later worker began serving once its probe measured (A-219).
const WORKER_RELEASED: u64 = LANDING + LANDINGS as u64;
/// Format: a private Released marker: the probing role has made its probe volume and is measuring on it.
const PROBE_VOLUME_MADE: u64 = WORKER_RELEASED + 1;
/// Format: a reader's inherited stop event, as a hex handle value, and the index of its first file.
const STOP: &str = "SLATES_WINFSP_REQUEST_STOP";
/// Format: see STOP.
const FIRST_FILE: &str = "SLATES_WINFSP_REQUEST_FIRST_FILE";
/// Shape: a reader's exits when a request waited past WAIT, and when one was answered with another's bytes or an error
/// other than the driver's cancellation.
const LOST_EXIT: i32 = ROLE_EXIT + 1;
/// Shape: see LOST_EXIT.
const WRONG_EXIT: i32 = ROLE_EXIT + 2;
/// Format: this module's full libtest role name; exact filtering must execute it, never zero tests.
const ROLE_TEST: &str = "host::tests::request_role";
/// Format: a private Released marker: a purge's open reached the purging server, which retains it unanswered.
const PURGE_TAKEN: u64 = SUCCESSOR_READY + 1;
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
  readers: Vec<Child>,
}

impl Drop for Scenario {
  fn drop(&mut self) {
    for server in &mut self.servers {
      server.kill_and_reap();
    }
    self.held = None;
    for reader in self
      .reader
      .iter_mut()
      .chain(self.probe.iter_mut())
      .chain(self.readers.iter_mut())
    {
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
    readers: Vec::new(),
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
    readers: Vec::new(),
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
  for index in 0..OVERLAP_READERS * OVERLAP_FILES {
    let (file, handle) = bridge
      .create(ObjectId::new(root, 0), &cx, &overlap_name(index), 0o644, 0)
      .unwrap();
    let object = ObjectId::new(file.ino, file.generation);
    bridge
      .write(object, &cx, 0, &[overlap_byte(index); PAGE])
      .unwrap();
    bridge.release(object, &cx, handle).unwrap();
  }
  (volume, cx)
}

fn overlap_name(index: usize) -> String {
  format!("overlap{index}")
}

/// Each overlap file's byte, distinct for every file.
fn overlap_byte(index: usize) -> u8 {
  b'a' + u8::try_from(index).unwrap()
}

fn inherited_controls(role: &str) -> (Channel, Option<Inherited>) {
  if role == "first" || role == "serving" || role == "purger" {
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
  if role == "taking" {
    let landing = journals_holding_a_batch(&inherited.as_ref().unwrap().ledger);
    channel
      .send(
        &HandleMessage::Released {
          id: LANDING + landing,
        },
        WAIT_MS,
      )
      .unwrap();
  }
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
  if role == "first" || role == "serving" || role == "purger" {
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
  let mut recovery_counted = false;
  let mut release_reported = false;
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
    let request = receive
      .recv_timeout(ROLE_WAIT)
      .expect("the fixture's next request");
    count_recovery(role, &request, mount, &mut recovery_counted);
    report_release(role, channel, mount, &mut release_reported);
    if role == "purger" && matches!(request.op, Op::PurgeOpen { .. }) {
      channel
        .send(&HandleMessage::Released { id: PURGE_TAKEN }, WAIT_MS)
        .unwrap();
      // Retain the purge's open, its reply sender included, until the parent kills this process.
      let _retained = request;
      let answer = channel.recv(WAIT_MS).unwrap();
      panic!("the purger should have died while retaining the purge's open, got {answer:?}");
    }
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

/// The kill oracle's servers tell the parent when their later worker began serving (A-219), so it can kill after it.
fn report_release(role: &str, channel: &Channel, mount: &mut MountThread, reported: &mut bool) {
  if (role != "serving" && role != "taking") || *reported {
    return;
  }
  while let Some(outcome) = mount.outcome() {
    match outcome {
      Outcome::Released(workers) => {
        eprintln!("WINFSP OVERLAP SERVER: {workers} receive workers serve");
        channel
          .send(
            &HandleMessage::Released {
              id: WORKER_RELEASED,
            },
            WAIT_MS,
          )
          .unwrap();
        *reported = true;
        return;
      }
      Outcome::Failed(error) => panic!("a receive worker failed: {error}"),
      Outcome::Stopped => panic!("the mount stopped while serving the kill oracle"),
      _ => {}
    }
  }
}

fn count_recovery(role: &str, request: &Request, mount: &mut MountThread, counted: &mut bool) {
  if role != "successor" || *counted || !matches!(request.op, Op::Read { .. }) {
    return;
  }
  let Some(Outcome::Recovered(count)) = mount.outcome() else {
    panic!("a fresh read preceded the captured-request outcome");
  };
  assert!(
    count > 0,
    "the successor must settle the retained read before serving another"
  );
  *counted = true;
  eprintln!("WINFSP INFLIGHT: successor counted {count} settled captured requests");
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
  };
  assert!(
    request
      .serve(bridge, cx, &mut OpenNames::new(), ledger)
      .send()
  );
  let _reply = answer.recv().unwrap();
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
  if role == "hammer" {
    hammer(&drive);
  }
  if role == "probing" {
    probing();
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

/// Which workers' journals the dead server left holding a request, read from a copy before this successor adopts them
/// (A-219): bit `n` for worker `n`. A journal waiting in a receive with nothing delivered holds none.
fn journals_holding_a_batch(ledger: &OwnedHandle) -> u64 {
  let bound = slates_bridge_core::volume_bridge::MAX_OPEN_HANDLES;
  let memory = SparseObject::from_held_handle(
    ledger.try_clone().unwrap(),
    crate::opens::bytes(bound).unwrap(),
    slates_mem::Words::new(),
  )
  .unwrap();
  let mut holding = 0;
  for worker in 0..crate::inflight::capture::MAX_WORKERS {
    let mut captured = vec![0; crate::inflight::capture::BYTES];
    memory
      .read(
        crate::opens::journal_offset(bound, worker).unwrap(),
        &mut captured,
      )
      .unwrap();
    if crate::inflight::capture::Capture::new(&mut captured)
      .unwrap()
      .resume()
      .unwrap()
      .is_some_and(|batch| !batch.hints().is_empty())
    {
      holding |= 1 << worker;
    }
  }
  holding
}

/// A sector-aligned page for an unbuffered read, which goes to the server every time.
#[repr(align(4096))]
struct AlignedPage([u8; PAGE]);

/// What one read of a reader's own file came to.
enum Answer {
  Own,
  Cancelled,
  Wrong(String),
  Stopped,
}

fn read_own(path: &str, expected: u8) -> Answer {
  let aborted = |error: &std::io::Error| {
    error.raw_os_error() == Some(i32::try_from(ERROR_OPERATION_ABORTED).unwrap())
  };
  let mut file = match std::fs::OpenOptions::new()
    .read(true)
    .custom_flags(FILE_FLAG_NO_BUFFERING)
    .open(path)
  {
    Ok(file) => file,
    Err(error) if aborted(&error) => return Answer::Cancelled,
    Err(error) => return Answer::Wrong(format!("open {path}: {error}")),
  };
  let mut page = AlignedPage([0; PAGE]);
  match file.read(&mut page.0) {
    Ok(PAGE) if page.0.iter().all(|byte| *byte == expected) => Answer::Own,
    Ok(PAGE) => Answer::Wrong(format!(
      "{path} read another file's bytes, first {:#x}",
      page.0[0]
    )),
    Ok(short) => Answer::Wrong(format!("{path} read {short} bytes")),
    Err(error) if aborted(&error) => Answer::Cancelled,
    Err(error) => Answer::Wrong(format!("read {path}: {error}")),
  }
}

/// A-219's reader: reads its own files without the cache until the parent's stop event, each read reported to a
/// watcher that ends the process LOST_EXIT if one waits past WAIT.
fn hammer(drive: &str) -> ! {
  let stop = inherited_handle(u64::from_str_radix(&std::env::var(STOP).unwrap(), 16).unwrap());
  let first: usize = std::env::var(FIRST_FILE).unwrap().parse().unwrap();
  let drive = drive.to_owned();
  let (report, reports) = sync_channel(1);
  let _reader = std::thread::spawn(move || {
    let mut turn = 0usize;
    loop {
      // SAFETY: the stop event stays owned by this thread for the whole poll.
      if unsafe { WaitForSingleObject(stop.as_raw_handle(), 0) } == WAIT_OBJECT_0 {
        let _ = report.send(Answer::Stopped);
        return;
      }
      let index = first + turn % OVERLAP_FILES;
      turn += 1;
      let path = format!("{drive}\\{}", overlap_name(index));
      if report.send(read_own(&path, overlap_byte(index))).is_err() {
        return;
      }
    }
  });
  let (mut own, mut cancelled) = (0u64, 0u64);
  loop {
    match reports.recv_timeout(WAIT) {
      Ok(Answer::Own) => own += 1,
      Ok(Answer::Cancelled) => cancelled += 1,
      Ok(Answer::Stopped) => break,
      Ok(Answer::Wrong(why)) => {
        eprintln!("WINFSP OVERLAP READER {first}: WRONG after {own} reads: {why}");
        std::process::exit(WRONG_EXIT);
      }
      Err(_) => {
        eprintln!("WINFSP OVERLAP READER {first}: a read waited past {WAIT:?} after {own} reads");
        std::process::exit(LOST_EXIT);
      }
    }
  }
  eprintln!(
    "WINFSP OVERLAP READER {first}: {own} reads returned their own bytes, {cancelled} were cancelled"
  );
  std::process::exit(ROLE_EXIT);
}

/// A-219. Do: keep three processes reading their own files through the held drive, each read going to the server's
/// receive workers, and kill the serving process forty times, each successor taking over the held volume and both
/// journals. Each server's second worker serves only once its probe has measured, so every other kill waits for that
/// release first and the rest fall before it, mostly during the probe. Expect: every read returns its own file's bytes
/// or the driver's cancellation, none waits past WAIT, and some kills after a release left the second worker's journal
/// holding a request, as each successor reports from the journals it adopts.
#[test]
fn every_request_is_answered_with_its_own_response_across_kills_of_two_receive_workers() {
  if std::env::var_os("WINFSP_TEST_MOUNT").is_none() {
    eprintln!("SKIP: the two-worker kill oracle needs WINFSP_TEST_MOUNT=1 and WinFsp installed");
    return;
  }
  let drive = free_drive();
  let (anchor, daemon) = channel().unwrap();
  let inherited = duplicate_inheritable(daemon.raw()).unwrap();
  let value = format!("{:x}", handle_value(inherited.raw()));
  let mut scenario = Scenario {
    servers: Vec::new(),
    held: Some(Held::new(1)),
    reader: None,
    probe: None,
    readers: Vec::new(),
  };
  scenario.servers.push(spawn(
    "serving",
    &drive,
    &[(OsStr::new(CHANNEL), OsStr::new(&value))],
    &[inherited.raw()],
  ));
  drop(inherited);
  hold_first_mount(&mut scenario, &anchor);
  // SAFETY: an unnamed manual-reset event; no security descriptor or name is borrowed.
  let event = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
  assert!(!event.is_null(), "the readers' stop event");
  // SAFETY: CreateEventW returned this fresh handle; the value takes its ownership once.
  let stop = unsafe { OwnedHandle::from_raw_handle(event) };
  start_readers(&mut scenario, &stop, &drive);
  // By whether the killed server's later worker had been released, and by the journals the kill left holding a request.
  let mut landings = [[0u32; LANDINGS]; 2];
  for kill in 1..=OVERLAP_KILLS {
    let released = kill % 2 == 0;
    if released {
      await_release(&anchor, kill);
    }
    let landing = kill_and_take_over(&mut scenario, &anchor, &daemon, &drive, kill);
    landings[usize::from(released)][landing] += 1;
  }
  // SAFETY: the event is owned and live.
  assert_ne!(unsafe { SetEvent(stop.as_raw_handle()) }, 0);
  for reader in &mut scenario.readers {
    assert_eq!(
      reader.wait(),
      Some(ROLE_EXIT),
      "a reader saw a request lost (exit {LOST_EXIT}) or answered wrong (exit {WRONG_EXIT})"
    );
  }
  let [early, late] = landings.map(|by_set| by_set.iter().copied().enumerate().collect::<Vec<_>>());
  eprintln!(
    "WINFSP OVERLAP: {OVERLAP_KILLS} kills by the journals left holding a request (bit n: worker n): before the later \
     worker's release was awaited {early:?}, after it {late:?}; every reader's requests answered with their own bytes \
     or cancelled"
  );
  let second = landings[1]
    .iter()
    .enumerate()
    .filter(|(set, _)| set & 2 != 0)
    .map(|(_, kills)| *kills)
    .sum::<u32>();
  assert!(
    second > 0,
    "no kill left the second worker holding a request, so this run proves nothing about it"
  );
}

/// Waits until the current server's later worker serves (A-219): the first server's, or the last successor's, whose
/// readiness was the last message.
fn await_release(anchor: &Channel, kill: u32) {
  assert_eq!(
    anchor.recv(WAIT_MS).unwrap(),
    Some(HandleMessage::Released {
      id: WORKER_RELEASED
    }),
    "kill {kill}: the server's later worker must serve once its probe has measured"
  );
}

fn hold_first_mount(scenario: &mut Scenario, anchor: &Channel) {
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
}

fn start_readers(scenario: &mut Scenario, stop: &OwnedHandle, drive: &str) {
  for reader in 0..OVERLAP_READERS {
    let shared = duplicate_inheritable(stop.as_raw_handle()).unwrap();
    let stop_value = format!("{:x}", handle_value(shared.raw()));
    let first = (reader * OVERLAP_FILES).to_string();
    scenario.readers.push(spawn(
      "hammer",
      drive,
      &[
        (OsStr::new(STOP), OsStr::new(&stop_value)),
        (OsStr::new(FIRST_FILE), OsStr::new(&first)),
      ],
      &[shared.raw()],
    ));
  }
}

/// Kills the serving process after a pause that varies with the kill, starts its successor on the held handles, and
/// returns the landing code the successor read from the journal it adopts.
fn kill_and_take_over(
  scenario: &mut Scenario,
  anchor: &Channel,
  daemon: &Channel,
  drive: &str,
  kill: u32,
) -> usize {
  #[allow(clippy::disallowed_methods)]
  std::thread::sleep(POLL * (kill % OVERLAP_PAUSES));
  scenario.servers.last_mut().unwrap().kill_and_reap();
  let handoff = scenario.held.as_ref().unwrap().handoff(daemon).unwrap();
  scenario.servers.push(spawn(
    "taking",
    drive,
    &[(OsStr::new(ENV_HANDLES), OsStr::new(handoff.env()))],
    &handoff.handles(),
  ));
  drop(handoff);
  // A release the killed server reported unawaited comes first, before its successor's landing.
  let landing = loop {
    match anchor.recv(WAIT_MS).unwrap() {
      Some(HandleMessage::Released {
        id: WORKER_RELEASED,
      }) => {}
      Some(HandleMessage::Released { id })
        if (LANDING..LANDING + LANDINGS as u64).contains(&id) =>
      {
        break usize::try_from(id - LANDING).unwrap();
      }
      other => panic!("kill {kill}: the successor's landing, got {other:?}"),
    }
  };
  assert_eq!(
    anchor.recv(WAIT_MS).unwrap(),
    Some(HandleMessage::Released {
      id: SUCCESSOR_READY
    }),
    "kill {kill}: the successor must serve"
  );
  landing
}

/// A-219's probing role: makes a probe volume, tells the parent, and measures on it until the parent kills it.
fn probing() -> ! {
  let value = u64::from_str_radix(&std::env::var(CHANNEL).unwrap(), 16).unwrap();
  let channel = Channel::adopt(inherited_handle(value));
  let (_sender, mut intents) = crate::purge_intents::channel(1).unwrap();
  let volume = crate::probe::ProbeVolume::new(crate::ffi::api().unwrap()).unwrap();
  channel
    .send(
      &HandleMessage::Released {
        id: PROBE_VOLUME_MADE,
      },
      WAIT_MS,
    )
    .unwrap();
  // A whole probe runs far past the parent's kill; one that ended first would leave this process to exit on its own.
  loop {
    crate::probe::measure_on(&volume, &volume.probe_path(), &mut intents[0], &|| false).unwrap();
  }
}

/// A-219. Do: start a process that makes a receive-worker probe volume and measures on it, and kill it mid-probe.
/// Expect: the volume's device appears while it probes and is gone after the kill, since a WinFsp volume lives on its
/// control handle, which the process's death closes.
#[test]
fn a_probe_killed_mid_measure_leaves_no_volume() {
  if std::env::var_os("WINFSP_TEST_MOUNT").is_none() {
    eprintln!("SKIP: the killed-probe oracle needs WINFSP_TEST_MOUNT=1 and WinFsp installed");
    return;
  }
  let before = crate::probe::tests::device_entries();
  let (anchor, daemon) = channel().unwrap();
  let inherited = duplicate_inheritable(daemon.raw()).unwrap();
  let value = format!("{:x}", handle_value(inherited.raw()));
  let mut child = spawn(
    "probing",
    "",
    &[(OsStr::new(CHANNEL), OsStr::new(&value))],
    &[inherited.raw()],
  );
  drop(inherited);
  assert_eq!(
    anchor.recv(WAIT_MS).unwrap(),
    Some(HandleMessage::Released {
      id: PROBE_VOLUME_MADE
    })
  );
  let made: Vec<String> = crate::probe::tests::device_entries()
    .difference(&before)
    .cloned()
    .collect();
  assert!(!made.is_empty(), "the probe's volume device must appear");
  #[allow(clippy::disallowed_methods)]
  std::thread::sleep(POLL * 2);
  assert!(
    child.process.try_wait().unwrap().is_none(),
    "the probe must still be measuring when killed"
  );
  child.kill_and_reap();
  let started = Instant::now();
  loop {
    let left: Vec<&String> = made
      .iter()
      .filter(|entry| crate::probe::tests::device_entries().contains(*entry))
      .collect();
    if left.is_empty() {
      break;
    }
    assert!(
      started.elapsed() < WAIT,
      "{left:?} outlived the killed probe"
    );
    #[allow(clippy::disallowed_methods)]
    std::thread::sleep(POLL);
  }
  eprintln!(
    "WINFSP PROBE: {made:?} appeared while the probe measured and went within {:?} of its kill",
    started.elapsed()
  );
}
