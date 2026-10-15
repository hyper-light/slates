//! Windows ETW capture probe for the hermeticity tracer's Windows leg (R1 dynamic half, Part 6
//! "Hermeticity"; AUD-29-42, the Windows leg owed). The Linux leg records with `strace` and the macOS
//! leg with `eslogger` — a built-in OS tracer writes a log, a Rust parser reads it back. This proves the
//! Windows equivalent on the real runner before a parser is written to a guessed format: the built-in
//! `logman` enables the `Microsoft-Windows-Kernel-File` provider to an `.etl`, a known file-write
//! workload runs, `logman` stops and `tracerpt` dumps the `.etl` to XML, and the probe surfaces that XML
//! so the parser (`parse_etw`, owed) is built against a real vector — its event shapes, its process-id
//! and time fields, and how a write event names its file (by a key resolved from an earlier create, as
//! strace decorates a descriptor and eslogger keeps a descriptor table).
//!
//! Gated by `SLATES_ETW_CAPTURE=1` and native Windows only (the whole file is `cfg(windows)`), run as the
//! elevated runner (the provider attaches only as an administrator/SYSTEM — the Linux leg's strace and
//! macOS's eslogger are likewise privileged; R10 is untouched, it governs the daemon, not the test
//! tracer). This is a probe: it prints the mechanism's real output for the next step to read, and asserts
//! only that the mechanism ran and named the known file.

#![cfg(windows)]

use std::io::Write;
use std::process::Command;

/// The manifest provider that reports file creates, writes, renames, deletes and set-information with a
/// process id and a resolvable name (`{EDD08927-9CC4-4E65-B970-C2560FB5C289}`; evidence B, Microsoft's
/// provider manifest, and C, deployed forensic tooling that reads it).
const KERNEL_FILE_PROVIDER: &str = "{EDD08927-9CC4-4E65-B970-C2560FB5C289}";

/// A readable dump of a command's outcome for the CI log.
fn report(label: &str, out: &std::process::Output) {
  eprintln!(
    "--- {label}: exit={:?}\n  stdout: {}\n  stderr: {}",
    out.status.code(),
    String::from_utf8_lossy(&out.stdout).trim(),
    String::from_utf8_lossy(&out.stderr).trim(),
  );
}

#[test]
fn etw_captures_a_known_file_write_workload() {
  if std::env::var_os("SLATES_ETW_CAPTURE").is_none() {
    eprintln!("skipped: set SLATES_ETW_CAPTURE=1 to run the Windows ETW capture probe");
    return;
  }

  let pid = std::process::id();
  // A scratch directory named by this process, under the system temp, removed at the end. The probe
  // must make a real disk write to have one to trace — as the hermeticity suite writes the land-target.
  let dir = std::env::temp_dir().join(format!("slates-etw-probe-{pid}"));
  let _ = std::fs::remove_dir_all(&dir);
  std::fs::create_dir_all(&dir).expect("create scratch dir");
  let session = format!("slates-etw-probe-{pid}");
  let etl = dir.join("trace.etl");
  let xml = dir.join("trace.xml");
  // A distinctive basename so the dump's lines for our writes are greppable among the machine's file I/O.
  let marker = format!("slates-known-{pid}.txt");
  let target = dir.join(&marker);
  let renamed = dir.join(format!("slates-known-{pid}-renamed.txt"));

  // Clear any stale session of this name, then start a real-time event-trace session over the provider,
  // all keywords and levels, straight to the etl (`-ets`: a session with no data-collector set).
  let _ = Command::new("logman")
    .args(["stop", &session, "-ets"])
    .output();
  let start = Command::new("logman")
    .args([
      "start",
      &session,
      "-p",
      KERNEL_FILE_PROVIDER,
      "0xffffffffffffffff",
      "0xff",
      "-ets",
      "-o",
    ])
    .arg(&etl)
    .output()
    .expect("run logman start");
  report("logman start", &start);

  // The known workload, in this process (its pid is known): create+write, append, truncate, rename,
  // delete — the write-capable operations the judge classifies.
  std::fs::write(&target, b"hello").expect("create+write");
  {
    let mut appended = std::fs::OpenOptions::new()
      .append(true)
      .open(&target)
      .expect("open for append");
    appended.write_all(b" more").expect("append");
    appended.flush().expect("flush");
  }
  {
    let mut truncated = std::fs::OpenOptions::new()
      .write(true)
      .truncate(true)
      .open(&target)
      .expect("open for truncate");
    truncated.write_all(b"x").expect("write after truncate");
    truncated.flush().expect("flush");
  }
  std::fs::rename(&target, &renamed).expect("rename");
  std::fs::remove_file(&renamed).expect("delete");

  // Stop the session (which flushes its buffers to the etl), then dump the etl to XML.
  let stop = Command::new("logman")
    .args(["stop", &session, "-ets"])
    .output()
    .expect("run logman stop");
  report("logman stop", &stop);

  let convert = Command::new("tracerpt")
    .arg(&etl)
    .args(["-o"])
    .arg(&xml)
    .args(["-of", "XML", "-y"])
    .output()
    .expect("run tracerpt");
  report("tracerpt", &convert);

  let dump = std::fs::read_to_string(&xml).unwrap_or_default();
  eprintln!(
    "=== tracerpt XML: {} bytes, {} lines ===",
    dump.len(),
    dump.lines().count()
  );
  eprintln!("=== first 260 lines (event shape) ===");
  for line in dump.lines().take(260) {
    eprintln!("{line}");
  }
  eprintln!("=== lines naming the known file (up to 80) ===");
  let short = marker.trim_end_matches(".txt");
  for line in dump.lines().filter(|l| l.contains(short)).take(80) {
    eprintln!("{line}");
  }
  eprintln!("=== lines carrying this process id {pid} (up to 40) ===");
  let pid_hex = format!("0x{pid:x}");
  for line in dump
    .lines()
    .filter(|l| l.contains(&pid.to_string()) || l.contains(&pid_hex))
    .take(40)
  {
    eprintln!("{line}");
  }

  // Tidy up: the session is already stopped; remove the scratch tree.
  let _ = std::fs::remove_dir_all(&dir);

  // The mechanism must have produced a dump that names our known file. (Tightened to full event
  // classification once the parser is built against this vector.)
  assert!(start.status.success(), "logman started the session");
  assert!(!dump.is_empty(), "tracerpt produced a non-empty dump");
  assert!(
    dump.contains(short),
    "the dump names the known file written during the capture"
  );
}
