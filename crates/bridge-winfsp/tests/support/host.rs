//! A-198's taken-request boundary through the real WinFsp driver. The parent is the anchor, and two re-invoked
//! processes serve the same held control handle in turn. The first explicitly retains a request taken by the DLL;
//! killing it must not strand that request. A separate process performs the Windows read and must return its bytes
//! or ERROR_OPERATION_ABORTED after the successor starts. Each role has a distinct exit, and every process is reaped.
//! The small volume is reconstructed with the same fixed file in each role: this tests transport settlement, while
//! CLI's twenty-kill oracle establishes persistence and mapped-view coherence through the real daemon.
//!
//! A-215's oracle runs the same fixture under load: reader processes read their own files without the cache while the
//! parent kills the serving process again and again. Each successor first reports where the kill left the receive
//! journal, so the run shows how many kills fell inside the call that carries a response and receives.
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
/// Shape: kills in A-215's oracle, twice the takeover history's twenty, so each lane's kills fall in every part of the
/// receiver's cycle.
const OVERLAP_KILLS: u32 = 40;
/// Shape: reader processes in A-215's oracle, more than one so batches of several requests reach the receiver.
const OVERLAP_READERS: usize = 3;
/// Shape: the files each reader cycles over, so an open's wrong context reads another file's bytes.
const OVERLAP_FILES: usize = 4;
/// Shape: the steps of the pause before each kill, in POLLs, so kills fall at different points of a cycle.
const OVERLAP_PAUSES: u32 = 4;
/// Format: a private Released marker base: a successor's reading of where the kill left the journal, plus its code.
const LANDING: u64 = SUCCESSOR_READY + 1;
/// Format: the landings in code order: idle, receiving, handing off undelivered, handing off delivered.
const LANDINGS: usize = 4;
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
  if role == "first" || role == "serving" {
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
  if role == "taking" {
    let landing = journal_landing(&inherited.as_ref().unwrap().ledger);
    channel
      .send(
        &HandleMessage::Released {
          id: LANDING + landing_code(landing),
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
  if role == "first" || role == "serving" {
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
  loop {
    let request = receive
      .recv_timeout(ROLE_WAIT)
      .expect("the fixture's next request");
    count_recovery(role, &request, mount, &mut recovery_counted);
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
  if role == "hammer" {
    hammer(&drive);
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

/// Where the dead server left the held receive journal, read before this successor adopts it (A-215).
fn journal_landing(ledger: &OwnedHandle) -> crate::inflight::capture::Landing {
  let bound = slates_bridge_core::volume_bridge::MAX_OPEN_HANDLES;
  let memory = SparseObject::from_held_handle(
    ledger.try_clone().unwrap(),
    crate::opens::bytes(bound).unwrap(),
    slates_mem::Words::new(),
  )
  .unwrap();
  let mut captured = vec![0; crate::inflight::capture::BYTES];
  memory
    .read(crate::opens::capture_offset(bound).unwrap(), &mut captured)
    .unwrap();
  crate::inflight::capture::Capture::new(&mut captured)
    .unwrap()
    .landing()
    .unwrap()
}

fn landing_code(landing: crate::inflight::capture::Landing) -> u64 {
  use crate::inflight::capture::Landing;
  match landing {
    Landing::Idle => 0,
    Landing::Receiving => 1,
    Landing::HandingOffUndelivered => 2,
    Landing::HandingOffDelivered => 3,
  }
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

/// A-215's reader: reads its own files without the cache until the parent's stop event, each read reported to a
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

/// A-215. Do: keep three processes reading their own files through the held drive, each read going to the server,
/// and kill the serving process forty times, each successor taking over the held volume. Expect: every read returns
/// its own file's bytes or the driver's cancellation, none waits past WAIT, and some kills fell inside the call that
/// carries a response and receives, as each successor reports from the journal it adopts.
#[test]
fn every_request_is_answered_with_its_own_response_across_kills_inside_the_combined_call() {
  if std::env::var_os("WINFSP_TEST_MOUNT").is_none() {
    eprintln!("SKIP: the combined-call kill oracle needs WINFSP_TEST_MOUNT=1 and WinFsp installed");
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
  let mut landings = [0u32; LANDINGS];
  for kill in 1..=OVERLAP_KILLS {
    let landing = kill_and_take_over(&mut scenario, &anchor, &daemon, &drive, kill);
    landings[landing] += 1;
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
  let [idle, receiving, undelivered, delivered] = landings;
  eprintln!(
    "WINFSP OVERLAP: {OVERLAP_KILLS} kills: {idle} idle, {receiving} receiving, {undelivered} inside a combined call \
     before delivery, {delivered} after it; every reader's requests answered with their own bytes or cancelled"
  );
  assert!(
    undelivered + delivered > 0,
    "no kill fell inside a combined call, so this run proves nothing about it"
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
  let landing = match anchor.recv(WAIT_MS).unwrap() {
    Some(HandleMessage::Released { id }) if (LANDING..LANDING + 4).contains(&id) => {
      usize::try_from(id - LANDING).unwrap()
    }
    other => panic!("kill {kill}: the successor's landing, got {other:?}"),
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
