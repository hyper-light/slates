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
use windows_sys::Win32::System::Threading::WaitForSingleObject;

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
/// Derived: the rule's bound (§4.6, 2026-11-06): a killed daemon exits within one transaction timeout, the longest its
/// receive worker's control wait lasts (`crate::dispatcher::TRANSACT_TIMEOUT_MS`).
const EXIT_WITHIN_MS: u32 = crate::dispatcher::TRANSACT_TIMEOUT_MS;

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

fn inherited_controls(role: &str) -> (Channel, Option<Inherited>) {
  if role == "first" || role == "purger" {
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
  if role == "first" || role == "purger" {
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

/// A-198. The re-invoked mount server and reader; the parent requires the distinct ROLE_EXIT for a completed read.
#[test]
#[ignore = "a process role for the live taken-request oracle"]
fn request_role() {
  let role = std::env::var(ROLE).unwrap();
  let drive = std::env::var(DRIVE).unwrap();
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
