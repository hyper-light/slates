//! Windows ETW capture probe for the hermeticity tracer's Windows leg (R1 dynamic half, Part 6
//! "Hermeticity"; AUD-29-42, the Windows leg owed). The Linux leg records with `strace` and the macOS
//! leg with `eslogger` — a built-in OS tracer writes a log, a Rust parser reads it back. This proves the
//! Windows equivalent on the real runner and surfaces the real dump before a parser is finalized to a
//! guessed format: the built-in `logman` enables the `Microsoft-Windows-Kernel-File` provider to an
//! `.etl`, a known file-write workload runs, `logman` stops and `tracerpt` dumps the `.etl` to XML, and
//! the probe summarizes it — which `EventID`s fired, the `EventData` field names each carries (so the
//! handle field a write resolves its name through is known), and every field of the events that named the
//! known file. With `SLATES_ETW_DUMP=<dir>` it also persists the whole XML there for the CI artifact, so
//! the parser (`etw_write_events`'s XML front end, owed) and its golden vector are built from real bytes.
//!
//! Gated by `SLATES_ETW_CAPTURE=1` and native Windows only (`cfg(windows)`), run as the elevated runner
//! (the provider attaches only as an administrator/SYSTEM — the Linux leg's strace and macOS's eslogger
//! are likewise privileged; R10 governs the daemon, not the test tracer). It asserts only that the
//! mechanism ran and named the known file; the judge's full classification is proven once the parser is
//! built against this vector.

#![cfg(windows)]

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::process::Command;

/// The manifest provider that reports file creates, writes, renames, deletes and set-information with a
/// process id and a resolvable name (`{EDD08927-9CC4-4E65-B970-C2560FB5C289}`; evidence B, Microsoft's
/// provider manifest, and C, deployed forensic tooling that reads it).
const KERNEL_FILE_PROVIDER: &str = "{EDD08927-9CC4-4E65-B970-C2560FB5C289}";
/// Format: how the dump names our provider's events, to tell them from the session's own header events.
const KERNEL_FILE_NAME: &str = "Microsoft-Windows-Kernel-File";

/// A readable dump of a command's outcome for the CI log.
fn report(label: &str, out: &std::process::Output) {
  eprintln!(
    "--- {label}: exit={:?}\n  stdout: {}\n  stderr: {}",
    out.status.code(),
    String::from_utf8_lossy(&out.stdout).trim(),
    String::from_utf8_lossy(&out.stderr).trim(),
  );
}

/// The text between two markers, if both are present in order.
fn between<'a>(text: &'a str, open: &str, close: &str) -> Option<&'a str> {
  let start = text.find(open)? + open.len();
  let rest = text.get(start..)?;
  let end = rest.find(close)?;
  rest.get(..end)
}

/// The value of an XML attribute `name="value"`, if present.
fn attribute(text: &str, name: &str) -> Option<String> {
  let marker = format!("{name}=\"");
  let start = text.find(&marker)? + marker.len();
  let rest = text.get(start..)?;
  let end = rest.find('"')?;
  rest.get(..end).map(str::to_owned)
}

/// Every `<Data Name="n">v</Data>` field in an event block, as (name, value) pairs.
fn data_fields(block: &str) -> Vec<(String, String)> {
  block
    .split("<Data Name=\"")
    .skip(1)
    .filter_map(|segment| {
      let (name, rest) = segment.split_once("\">")?;
      let (value, _) = rest.split_once("</Data>")?;
      Some((name.to_owned(), value.trim().to_owned()))
    })
    .collect()
}

#[test]
fn etw_captures_a_known_file_write_workload() {
  if std::env::var_os("SLATES_ETW_CAPTURE").is_none() {
    eprintln!("skipped: set SLATES_ETW_CAPTURE=1 to run the Windows ETW capture probe");
    return;
  }

  let pid = std::process::id();
  // A scratch directory named by this process, under the system temp, removed at the end. The probe must
  // make a real disk write to have one to trace — as the hermeticity suite writes the land-target.
  let dir = std::env::temp_dir().join(format!("slates-etw-probe-{pid}"));
  let _ = std::fs::remove_dir_all(&dir);
  std::fs::create_dir_all(&dir).expect("create scratch dir");
  let session = format!("slates-etw-probe-{pid}");
  let etl = dir.join("trace.etl");
  let xml = dir.join("trace.xml");
  // A distinctive basename so the dump's lines for our writes are greppable among the machine's file I/O.
  let marker = format!("slates-known-{pid}.txt");
  let short = marker.trim_end_matches(".txt").to_owned();
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

  // Summarize the provider's events: how many of each EventID, the EventData field names each id carries
  // (so the name field and the handle field are known), and every field of the events that named our file.
  let mut count_by_id: BTreeMap<u32, usize> = BTreeMap::new();
  let mut fields_by_id: BTreeMap<u32, BTreeSet<String>> = BTreeMap::new();
  let mut ours: Vec<String> = Vec::new();
  for block in dump.split("</Event>") {
    if !block.contains(KERNEL_FILE_NAME) {
      continue;
    }
    let Some(id) =
      between(block, "<EventID>", "</EventID>").and_then(|s| s.trim().parse::<u32>().ok())
    else {
      continue;
    };
    *count_by_id.entry(id).or_default() += 1;
    let fields = data_fields(block);
    for (name, _) in &fields {
      fields_by_id.entry(id).or_default().insert(name.clone());
    }
    if fields
      .iter()
      .any(|(_, value)| value.contains(short.as_str()))
    {
      let process = attribute(block, "ProcessID").unwrap_or_default();
      let time = attribute(block, "SystemTime").unwrap_or_default();
      let pairs: Vec<String> = fields
        .iter()
        .map(|(name, value)| format!("{name}={value}"))
        .collect();
      ours.push(format!(
        "EventID={id} ProcessID={process} SystemTime={time} | {}",
        pairs.join("  ")
      ));
    }
  }
  eprintln!("=== Microsoft-Windows-Kernel-File EventID histogram (id: count  fields) ===");
  for (id, count) in &count_by_id {
    let names: Vec<String> = fields_by_id
      .get(id)
      .map(|set| set.iter().cloned().collect())
      .unwrap_or_default();
    eprintln!("  EventID {id}: {count}  fields={names:?}");
  }
  eprintln!(
    "=== full events naming the known file ({} of them) ===",
    ours.len()
  );
  for line in ours.iter().take(60) {
    eprintln!("{line}");
  }

  // Persist the whole dump for the CI artifact (built-against-real-bytes parser and golden vector).
  if let Some(dest) = std::env::var_os("SLATES_ETW_DUMP") {
    let dest = std::path::Path::new(&dest);
    let _ = std::fs::create_dir_all(dest);
    match std::fs::copy(&xml, dest.join("etw-trace.xml")) {
      Ok(_) => eprintln!(
        "persisted the dump to {}",
        dest.join("etw-trace.xml").display()
      ),
      Err(error) => eprintln!("could not persist the dump: {error}"),
    }
  }

  // Tidy the scratch tree (the session is already stopped).
  let _ = std::fs::remove_dir_all(&dir);

  // The mechanism must have produced a dump that names our known file and classified its events as the
  // provider's. (Tightened to full write-event classification once the parser is built against this dump.)
  assert!(start.status.success(), "logman started the session");
  assert!(!dump.is_empty(), "tracerpt produced a non-empty dump");
  assert!(
    dump.contains(short.as_str()),
    "the dump names the known file written during the capture"
  );
  assert!(
    !count_by_id.is_empty(),
    "the dump carries Microsoft-Windows-Kernel-File events"
  );
  assert!(
    !ours.is_empty(),
    "at least one provider event named the known file"
  );
}
