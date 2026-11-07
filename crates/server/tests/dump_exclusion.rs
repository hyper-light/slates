//! A crashed process's Windows Error Reporting dump carries none of the private mappings (A-239; GAPS `8a-win-dumps`;
//! §4.2 "RAM-only guarantees", R1), read from a real dump of a real crash.
//!
//! The test binary is re-invoked as the child (an environment variable selects the role and carries a seed). The
//! child writes a pattern derived from the seed, a byte at a time so it is never held whole anywhere else, into each
//! mapping the daemon keeps out of dumps: a private content region of the chunk arena (`Region::map`), a view of a
//! shared memory object (`Region::shared` over a `SharedObject`), and a key slot of hyper-seal's key region (reserved
//! as the daemon reserves it, `seal_keys::reserve`). A fourth pattern goes into an ordinary heap allocation, which is
//! not excluded: the control that proves the dump carries memory at all, so an empty or minidump-only dump cannot pass.
//! Then the child aborts, which Windows reports through WER, and WER writes its dump where the runner's `LocalDumps`
//! setting says (`DumpType` 2, full memory). The parent reads the dump: the control must be in it, and no excluded
//! pattern may be.
//!
//! Gated on `SLATES_WER_DUMP_DIR`, the folder the runner's `LocalDumps` writes to (CI's Windows job sets both); without
//! it the test skips loudly. Unix is covered by `slates-cli`'s `dumps` (the whole process out of core dumps).
#![cfg(windows)]
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::cast_possible_truncation
)]

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

/// Shape: the environment variable that makes the re-invoked binary the crashing child, carrying the seed.
const CHILD_SEED: &str = "SLATES_DUMP_EXCLUSION_CHILD_SEED";
/// Shape: the folder WER's `LocalDumps` writes to on this runner; unset, the test skips.
const DUMP_DIR: &str = "SLATES_WER_DUMP_DIR";
/// Shape: the tag on the child's one line, found anywhere in a line (libtest's banner comes first, unended).
const TAG: &str = "dump-exclusion-child:";
/// Shape: a pattern's length, a key slot's: long enough that a chance match in a dump of hundreds of megabytes is
/// out of the question (2^-256 a position).
const PATTERN_BYTES: usize = 32;
/// Bound: how long WER may take to finish writing the child's dump after the child is gone. WerFault writes it while
/// the process is held, so it is there at the child's exit; the bound covers a slow runner's disk.
const DUMP_WAIT: Duration = Duration::from_secs(60);

/// Where each pattern was written.
#[derive(Clone, Copy, Debug)]
enum Place {
  /// A private content region of the chunk arena.
  ContentRegion,
  /// A view of a shared memory object (the anchor segment's, the content object's and the copies' kind).
  SharedView,
  /// A slot of hyper-seal's key region.
  KeySlot,
  /// An ordinary heap allocation, not excluded: the control.
  Heap,
}

const EXCLUDED: [Place; 3] = [Place::ContentRegion, Place::SharedView, Place::KeySlot];

/// Byte `index` of the pattern for `place` under `seed`: SplitMix64's finalizer over the three, so a pattern exists
/// whole only where it is written, never in the seed or the code.
fn pattern_byte(seed: u64, place: Place, index: usize) -> u8 {
  let mut z = seed
    ^ (place as u64 + 1).wrapping_mul(0x9E37_79B9_7F4A_7C15)
    ^ (index as u64).wrapping_mul(0xBF58_476D_1CE4_E5B9);
  z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
  z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
  (z ^ (z >> 31)) as u8
}

fn fill(bytes: &mut [u8], seed: u64, place: Place) {
  for (index, byte) in bytes.iter_mut().take(PATTERN_BYTES).enumerate() {
    *byte = pattern_byte(seed, place, index);
  }
}

/// What a child holds while WER dumps it: each pattern's mapping or allocation.
struct Held {
  region: slates_mem::Region,
  shared: slates_mem::Region,
  secret: hyper_seal::Secret32,
  heap: Vec<u8>,
}

/// Every pattern written where it belongs.
fn write_patterns(seed: u64) -> Held {
  let page = usize::try_from(slates_machine::facts::Facts::page_bytes()).unwrap();
  let mut region = slates_mem::Region::map(page, page, false).unwrap();
  fill(region.bytes_mut(), seed, Place::ContentRegion);

  let granule = slates_mem::mapping_granule().unwrap();
  let name = format!("slates-dump-exclusion-{}", std::process::id());
  let object = slates_mem::SharedObject::create(&name, granule, slates_mem::Words::new()).unwrap();
  // SAFETY: the object was made here and no other process or value reaches it.
  let mut shared =
    slates_mem::Region::shared(unsafe { slates_mem::ExclusiveObject::new(object) }, page);
  fill(shared.bytes_mut(), seed, Place::SharedView);

  assert!(
    slates_server::seal_keys::reserve(1),
    "the key region could not be locked"
  );
  let mut key = [0u8; PATTERN_BYTES];
  fill(&mut key, seed, Place::KeySlot);
  let secret = hyper_seal::Secret32::from_bytes(&key).unwrap();
  for byte in key.iter_mut() {
    // SAFETY: a live local byte; the volatile store is kept, so the stack copy is gone before the crash.
    unsafe { std::ptr::write_volatile(byte, 0) };
  }

  let mut heap = vec![0u8; page];
  fill(&mut heap, seed, Place::Heap);
  Held {
    region,
    shared,
    secret,
    heap,
  }
}

/// The crashing child: every pattern written, the exclusions' refusals reported, then a crash WER reports.
fn crash_with_patterns(seed: u64) -> ! {
  let held = write_patterns(seed);
  let (error_mode, job_flags) = clear_what_keeps_wer_away();
  let mut out = std::io::stdout().lock();
  writeln!(
    out,
    "{TAG} refused={} inherited_error_mode={error_mode:#x} job_limit_flags={job_flags:#x?}",
    slates_mem::dump_exclusion::refused()
  )
  .unwrap();
  out.flush().unwrap();
  drop(out);
  std::hint::black_box((&held.region, &held.shared, &held.secret, &held.heap));
  std::process::abort()
}

/// The reporting child: every pattern written, then a WER report of this process with a heap dump, queued (the dump WER
/// makes for an error report, which is what the registration governs: `WerRegisterExcludedMemoryBlock`), then a clean
/// exit. Its line says the submit's answer.
fn report_with_patterns(seed: u64) -> ! {
  use windows_sys::Win32::System::ErrorReporting::{
    HREPORT, WER_SUBMIT_OUTOFPROCESS, WER_SUBMIT_QUEUE, WER_SUBMIT_RESULT, WerConsentApproved,
    WerDumpTypeHeapDump, WerReportAddDump, WerReportCloseHandle, WerReportCreate,
    WerReportNonCritical, WerReportSubmit,
  };
  use windows_sys::Win32::System::Threading::GetCurrentProcess;
  let held = write_patterns(seed);
  let event: Vec<u16> = "SlatesDumpExclusionTest"
    .encode_utf16()
    .chain(std::iter::once(0))
    .collect();
  let mut report: HREPORT = std::ptr::null_mut();
  // SAFETY: a NUL-terminated wide event name, no report information, and a live local for the handle.
  let created = unsafe {
    WerReportCreate(
      event.as_ptr(),
      WerReportNonCritical,
      std::ptr::null(),
      &mut report,
    )
  };
  // SAFETY: the report just made, this process's pseudo-handle, no thread, exception or custom options.
  let added = unsafe {
    WerReportAddDump(
      report,
      GetCurrentProcess(),
      std::ptr::null_mut(),
      WerDumpTypeHeapDump,
      std::ptr::null(),
      std::ptr::null(),
      0,
    )
  };
  let mut result: WER_SUBMIT_RESULT = 0;
  // SAFETY: the report just made, and a live local for its result.
  let submitted = unsafe {
    WerReportSubmit(
      report,
      WerConsentApproved,
      WER_SUBMIT_QUEUE | WER_SUBMIT_OUTOFPROCESS,
      &mut result,
    )
  };
  // SAFETY: the report just made, closed once.
  let _closed = unsafe { WerReportCloseHandle(report) };
  let mut out = std::io::stdout().lock();
  writeln!(
    out,
    "{TAG} refused={} created={created:#x} added={added:#x} submitted={submitted:#x} result={result}",
    slates_mem::dump_exclusion::refused()
  )
  .unwrap();
  out.flush().unwrap();
  drop(out);
  std::hint::black_box((&held.region, &held.shared, &held.secret, &held.heap));
  std::process::exit(0)
}

/// The folders WER keeps reports in, the machine's and this user's: queued and archived.
fn report_folders() -> Vec<PathBuf> {
  let mut folders = Vec::new();
  for root in [
    std::env::var_os("ProgramData"),
    std::env::var_os("LOCALAPPDATA"),
  ]
  .into_iter()
  .flatten()
  {
    for kind in ["ReportQueue", "ReportArchive"] {
      folders.push(
        PathBuf::from(&root)
          .join("Microsoft")
          .join("Windows")
          .join("WER")
          .join(kind),
      );
    }
  }
  folders
}

/// Every file under `folders`, recursively, with its length: what WER kept.
fn files_under(folders: &[PathBuf]) -> std::collections::BTreeMap<PathBuf, u64> {
  let mut files = std::collections::BTreeMap::new();
  let mut pending: Vec<PathBuf> = folders.to_vec();
  while let Some(folder) = pending.pop() {
    let Ok(entries) = std::fs::read_dir(&folder) else {
      continue;
    };
    for entry in entries.filter_map(Result::ok) {
      let path = entry.path();
      match entry.metadata() {
        Ok(meta) if meta.is_dir() => pending.push(path),
        Ok(meta) => {
          files.insert(path, meta.len());
        }
        Err(_) => {}
      }
    }
  }
  files
}

/// Each pattern's occurrences in `bytes`: the heap control's, then each excluded place's.
fn scan(bytes: &[u8], seed: u64) -> (usize, Vec<(Place, usize)>) {
  let pattern = |place| -> Vec<u8> {
    (0..PATTERN_BYTES)
      .map(|index| pattern_byte(seed, place, index))
      .collect()
  };
  let control = occurrences(bytes, &pattern(Place::Heap));
  let found = EXCLUDED
    .iter()
    .map(|place| (*place, occurrences(bytes, &pattern(*place))))
    .collect();
  (control, found)
}

/// A seed no earlier run used.
fn fresh_seed() -> u64 {
  std::time::SystemTime::now()
    .duration_since(std::time::UNIX_EPOCH)
    .unwrap()
    .as_nanos() as u64
    ^ u64::from(std::process::id())
}

/// Runs this binary's `test` as a child under `role` with `seed`, out of this process's job where it allows (a job that
/// ends a process on an unhandled exception keeps WER from it); in it otherwise. Its tagged line and its status.
fn run_child(test: &str, role: &str, seed: u64) -> (String, std::process::ExitStatus, u32) {
  let spawn = |flags: u32| {
    use std::os::windows::process::CommandExt;
    Command::new(std::env::current_exe().unwrap())
      .args([test, "--exact", "--nocapture", "--test-threads=1"])
      .env(role, seed.to_string())
      .stdout(std::process::Stdio::piped())
      .creation_flags(flags)
      .spawn()
  };
  let child = spawn(windows_sys::Win32::System::Threading::CREATE_BREAKAWAY_FROM_JOB)
    .or_else(|refused| {
      eprintln!("the job refused a breakaway ({refused}); the child runs in it");
      spawn(0)
    })
    .unwrap();
  let pid = child.id();
  let output = child.wait_with_output().unwrap();
  let stdout = String::from_utf8_lossy(&output.stdout);
  let line = stdout
    .lines()
    .find_map(|line| line.split_once(TAG).map(|(_, rest)| rest.trim().to_owned()))
    .unwrap_or_else(|| panic!("the child never reached its report: {stdout}"));
  eprintln!("the child: {line}");
  assert!(
    line.starts_with("refused=0 "),
    "an exclusion was refused in the child: {line}"
  );
  (line, output.status, pid)
}

/// A file WER kept, scanned: its path, its length, the heap control's occurrences, and each excluded place's.
type Scanned = (PathBuf, u64, usize, Vec<(Place, usize)>);

/// Every new file WER kept since `before`, scanned.
fn scan_new_reports(before: &std::collections::BTreeMap<PathBuf, u64>, seed: u64) -> Vec<Scanned> {
  files_under(&report_folders())
    .into_iter()
    .filter(|(path, _)| !before.contains_key(path))
    .filter_map(|(path, len)| {
      let bytes = std::fs::read(&path).ok()?;
      let (control, found) = scan(&bytes, seed);
      eprintln!(
        "report file {} of {len} bytes: heap control {control}; excluded {found:?}",
        path.display()
      );
      Some((path, len, control, found))
    })
    .collect()
}

/// What could keep WER from this child's crash, read and (where the child can) cleared: the error mode it inherited
/// (`SEM_NOGPFAULTERRORBOX` keeps WER from a crash; the child sets it to none) and its job's limit flags (`None` outside
/// a job; `JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION` ends the process with no WER, which the parent's breakaway
/// spawn avoids where the job allows one). Printed, so a run that wrote no dump says which held.
fn clear_what_keeps_wer_away() -> (u32, Option<u32>) {
  use windows_sys::Win32::System::Diagnostics::Debug::{GetErrorMode, SetErrorMode};
  use windows_sys::Win32::System::JobObjects::{
    IsProcessInJob, JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
    QueryInformationJobObject,
  };
  use windows_sys::Win32::System::Threading::GetCurrentProcess;
  // SAFETY: reads this process's error mode; no argument.
  let inherited = unsafe { GetErrorMode() };
  // SAFETY: sets this process's error mode to the system default; no pointer.
  unsafe { SetErrorMode(0) };
  let mut in_job = 0;
  // SAFETY: the pseudo-handle of this process, no job named (any job), and a live local for the answer.
  let asked = unsafe { IsProcessInJob(GetCurrentProcess(), std::ptr::null_mut(), &mut in_job) };
  if asked == 0 || in_job == 0 {
    return (inherited, None);
  }
  let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
  // SAFETY: a null job handle names this process's job; the answer goes to a live local of the size passed.
  let queried = unsafe {
    QueryInformationJobObject(
      std::ptr::null_mut(),
      JobObjectExtendedLimitInformation,
      (&raw mut limits).cast(),
      u32::try_from(size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>()).unwrap(),
      std::ptr::null_mut(),
    )
  };
  (
    inherited,
    Some(if queried == 0 {
      u32::MAX
    } else {
      limits.BasicLimitInformation.LimitFlags
    }),
  )
}

/// The dump WER wrote for process `pid`: `<image>.<pid>.dmp` in `dir`, waited for within [`DUMP_WAIT`].
fn dump_of(dir: &Path, pid: u32) -> PathBuf {
  let suffix = format!(".{pid}.dmp");
  let deadline = Instant::now() + DUMP_WAIT;
  loop {
    let found = std::fs::read_dir(dir)
      .unwrap()
      .filter_map(Result::ok)
      .map(|entry| entry.path())
      .find(|path| path.to_string_lossy().ends_with(&suffix));
    if let Some(found) = found {
      return found;
    }
    assert!(
      Instant::now() < deadline,
      "WER wrote no dump for process {pid} in {} within {DUMP_WAIT:?}: is LocalDumps set?",
      dir.display()
    );
    #[allow(clippy::disallowed_methods)] // a test may sleep (the lint's stated exception)
    std::thread::sleep(Duration::from_millis(100));
  }
}

fn occurrences(haystack: &[u8], needle: &[u8]) -> usize {
  haystack
    .windows(needle.len())
    .filter(|window| *window == needle)
    .count()
}

/// Shape: the environment variable that makes the re-invoked binary the reporting child, carrying the seed.
const REPORT_SEED: &str = "SLATES_DUMP_EXCLUSION_REPORT_SEED";
/// Shape: set (to anything) on a runner with no `LocalDumps`, so the default crash report's files are scanned.
const SCAN_DEFAULT: &str = "SLATES_WER_SCAN_DEFAULT_REPORT";

/// A-239, 8a-win-dumps: WER's error report, the dump the registration governs. Do: a child writes known patterns into a
/// content region, a shared object's view and a key slot (each excluded) and into the heap (not), then has WER queue a
/// report of itself with a heap dump; scan every file WER kept for it. Expect: the heap's pattern in a dump, none of
/// the excluded ones, and no exclusion refused.
#[test]
fn a_wer_reports_heap_dump_carries_no_content_region_shared_view_or_key_slot() {
  if let Ok(seed) = std::env::var(REPORT_SEED) {
    report_with_patterns(seed.parse().unwrap());
  }
  let seed = fresh_seed();
  let before = files_under(&report_folders());
  let (line, status, _) = run_child(
    "a_wer_reports_heap_dump_carries_no_content_region_shared_view_or_key_slot",
    REPORT_SEED,
    seed,
  );
  assert!(status.success(), "the reporting child failed: {status:?}");
  assert!(
    line.contains("submitted=0x0 "),
    "WER refused the report: {line}"
  );
  let reports = scan_new_reports(&before, seed);
  assert!(
    reports.iter().any(|(_, _, control, _)| *control > 0),
    "no file WER kept carries the heap, so nothing is proven: {} new files",
    reports.len()
  );
  for (path, _, _, found) in &reports {
    for (place, count) in found {
      assert_eq!(*count, 0, "{place:?}'s pattern is in {}", path.display());
    }
  }
}

/// A-239's residual, measured: a crash's report under WER's defaults (no `LocalDumps`). Do: crash a child that wrote the
/// patterns, then scan every file WER kept for it. Expect: no excluded pattern in any of them. Runs only where
/// [`SCAN_DEFAULT`] says the runner has no `LocalDumps`, skipping loudly elsewhere; it prints what WER kept either way.
#[test]
fn a_default_crash_report_carries_no_content_region_shared_view_or_key_slot() {
  if let Ok(seed) = std::env::var(CHILD_SEED) {
    crash_with_patterns(seed.parse().unwrap());
  }
  if std::env::var_os(SCAN_DEFAULT).is_none() {
    eprintln!(
      "SKIP: {SCAN_DEFAULT} is unset, so the runner's WER defaults are not known to be in force"
    );
    return;
  }
  let seed = fresh_seed();
  let before = files_under(&report_folders());
  let (_, status, _) = run_child(
    "a_default_crash_report_carries_no_content_region_shared_view_or_key_slot",
    CHILD_SEED,
    seed,
  );
  assert!(!status.success(), "the child exited cleanly: {status:?}");
  let deadline = Instant::now() + DUMP_WAIT;
  let reports = loop {
    let reports = scan_new_reports(&before, seed);
    if !reports.is_empty() || Instant::now() >= deadline {
      break reports;
    }
    #[allow(clippy::disallowed_methods)] // a test may sleep (the lint's stated exception)
    std::thread::sleep(Duration::from_millis(500));
  };
  eprintln!("the default crash report kept {} files", reports.len());
  for (path, _, _, found) in &reports {
    for (place, count) in found {
      assert_eq!(*count, 0, "{place:?}'s pattern is in {}", path.display());
    }
  }
}

/// A-239's residual, measured: an administrator's `LocalDumps` full dump. Do: crash a child that wrote the patterns and
/// read the full dump `LocalDumps` wrote. Expect: the heap's pattern in it, and the excluded ones too, since `LocalDumps`
/// does not consult the registration (run 42355310061: each once). The test holds the residual as stated, so a Windows
/// that starts honouring the registration there turns it red and the record is corrected. Gated on [`DUMP_DIR`].
#[test]
fn an_administrators_local_full_dump_carries_every_mapping() {
  if let Ok(seed) = std::env::var(CHILD_SEED) {
    crash_with_patterns(seed.parse().unwrap());
  }
  let Some(dir) = std::env::var_os(DUMP_DIR).map(PathBuf::from) else {
    eprintln!(
      "SKIP: {DUMP_DIR} is unset, so no WER LocalDumps folder is known to read a crash dump from"
    );
    return;
  };
  let seed = fresh_seed();
  let (_, status, pid) = run_child(
    "an_administrators_local_full_dump_carries_every_mapping",
    CHILD_SEED,
    seed,
  );
  assert!(!status.success(), "the child exited cleanly: {status:?}");
  let path = dump_of(&dir, pid);
  let dump = std::fs::read(&path).unwrap();
  let (control, found) = scan(&dump, seed);
  eprintln!(
    "dump {} of {} bytes: heap control found {control} times; excluded {found:?}",
    path.display(),
    dump.len()
  );
  // The dump is WER's file, in the runner's LocalDumps folder: removed so this run's patterns do not stay on its disk.
  #[allow(clippy::disallowed_methods)]
  std::fs::remove_file(&path).unwrap();
  assert!(
    control > 0,
    "the dump carries no heap at all, so it proves nothing: is DumpType 2?"
  );
  for (place, count) in found {
    assert!(
      count > 0,
      "{place:?}'s pattern is not in the LocalDumps dump: correct the residual"
    );
  }
}
