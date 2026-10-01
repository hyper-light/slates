//! The hermeticity tracer's judgement (R1; Part 6 example 8 "Run the whole suite under a
//! filesystem-write tracer (Linux fanotify / macOS fs_usage / Windows ETW). Expect: zero writes
//! by slates processes outside the documented exceptions"; D-26: the exception is "inside a
//! granted target during that landing, matched to a `Written` outcome"). The static half of R1 is
//! `cargo xtask structural` (no write-capable syscall links outside `slates-land`); this is the
//! dynamic half: every write-capable system call the traced slates processes made, read back from
//! a tracer's log and placed in a closed taxonomy — inside the granted target, a RAM-only kernel
//! object (a memfd, an `shm_open` object, a socket, a pipe, an event descriptor, the FUSE device),
//! the process's own standard streams, unresolved (the tracer printed no path for the descriptor),
//! or outside: a violation. An unclassified path is a violation, never silence.
//!
//! Two log formats. **strace** (Linux; `strace -f -y -e trace=%file,...`): one call per line,
//! `pid  name(args) = ret`, descriptors decorated `N<path>` (`strace(1)`, evidence B), with
//! `<unfinished ...>` / `<... name resumed>` pairs joined per pid. **fs_usage** (macOS, root; `-w -f
//! filesys -f network PID`): `timestamp  call  [F=fd] [[errno]] [(flags)] [B=..] path  elapsed[ W]
//! proc.tid` (Apple's `fs_usage.c`, `print_open` and `format_print`; evidence C). fs_usage prints an
//! `openat` family path as `[dirfd]/name`, so the parser keeps a descriptor-to-path table from the
//! process's own `open`/`openat` results — which is why the macOS tracer is scoped to one process
//! (the daemon, the only writer by design), and why an unknown descriptor is `unresolved`, counted
//! and shown, rather than trusted either way. **eslogger** (macOS, root and Full Disk Access; Apple's
//! Endpoint Security events as JSON lines, one event per line: `event_type`, `event: {<kind>: {...}}`,
//! `process: {audit_token: {pid}, executable: {path}}`; `eslogger(1)`, evidence B): the kernel attaches the
//! full path to every file an event names, so nothing is resolved through a descriptor table; an event of
//! a write-capable kind whose path fields are absent or truncated is `unresolved`, never dropped. DTrace's
//! syscall provider is absent under SIP ("probe description syscall::open*:entry does not match any
//! probes. System Integrity Protection is on", 2026-09-26), and fs_usage cannot attribute descriptors
//! it saw duplicated, which is why the macOS tracer is eslogger.

use std::collections::HashMap;

use crate::record::Counts;

/// One write-capable call as the tracer saw it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WriteEvent {
  /// The 1-based log line.
  pub line: usize,
  /// The system call.
  pub call: String,
  /// The path the call named, resolved as far as the tracer allows.
  pub path: String,
  /// The descriptor the call operated on, for descriptor-based calls.
  pub descriptor: Option<i64>,
  /// The process that made the call, when the tracer names it.
  pub pid: Option<u32>,
  /// When the call was made, in nanoseconds since the Unix epoch, when the tracer stamps it.
  pub at_ns: Option<u64>,
}

/// Where a write landed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Placement {
  /// Inside the granted landing target.
  InsideTarget,
  /// A RAM-only kernel object; the reason names the class.
  RamOnly(&'static str),
  /// The process's own standard output or error, on a pipe, terminal or null device. A regular file
  /// behind descriptor 1 or 2 is a file like any other.
  StandardStream,
  /// A write inside the target that no granted landing accounts for; the reason says why (AUD-29-42).
  Unauthorized(&'static str),
  /// The tracer printed no path the parser could resolve.
  Unresolved,
  /// A path outside every allowed class: a violation.
  Outside,
}

/// What the judgement allows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Policy<'a> {
  /// The granted landing target, canonical (no trailing slash).
  pub target: &'a str,
  /// The traced process's working directory, for relative paths under `AT_FDCWD`.
  pub working_directory: &'a str,
  /// The granted landing, which alone may write inside the target: `None` when the lifecycle granted
  /// none, and then any write there is unauthorized.
  pub landing: Option<Landing<'a>>,
  /// The tracer's mount table: a written path's backing filesystem is its longest-prefix mount's, so a
  /// write to a kernel control file (`/proc/self/coredump_filter`) is told from a disk write by what is
  /// mounted there, never by its spelling (audit §9.1). Empty where the tracer names no mounts (macOS).
  pub mounts: &'a [Mount<'a>],
}

/// One mount of the tracer's namespace, as the judge needs it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Mount<'a> {
  /// Where it is mounted, canonical.
  pub point: &'a str,
  /// Its filesystem type (`mountinfo`'s field after the separator).
  pub fstype: &'a str,
}

/// Format: the filesystems whose files are the kernel's own state presented as files — memory, never a
/// disk (`proc(5)`, `sysfs(5)`, `cgroups(7)`). A write to one configures the kernel (a core filter, a
/// cgroup limit) and stores nothing. `pstore` is not one: it persists to firmware storage.
const KERNEL_VIRTUAL_FILESYSTEMS: &[&str] = &["proc", "sysfs", "cgroup", "cgroup2"];

/// Format: `mountinfo`'s mount-point field, zero-based (`proc(5)`: mount id, parent id, major:minor, root,
/// mount point, …).
const MOUNT_POINT_FIELD: usize = 4;
/// Format: an escape's length — a backslash and three octal digits.
const ESCAPE_LEN: usize = 4;
/// Format: the radix of an escape's digits (a digit 8 or 9 fails to parse, and the text is kept as written).
const ESCAPE_RADIX: u32 = 8;

/// The mounts `mountinfo` (`proc(5)`: `/proc/self/mountinfo`) lists, as (mount point, filesystem type):
/// the fifth field, with the kernel's octal escapes (`\040` for a space) decoded, and the first field after
/// the ` - ` separator. A line that does not have both is skipped.
pub fn parse_mountinfo(text: &str) -> Vec<(String, String)> {
  text
    .lines()
    .filter_map(|line| {
      let (head, tail) = line.split_once(" - ")?;
      let point = head.split(' ').nth(MOUNT_POINT_FIELD)?;
      let fstype = tail.split(' ').next()?;
      Some((unescape_mount_point(point), fstype.to_owned()))
    })
    .collect()
}

/// Decodes `mountinfo`'s three-digit octal escapes (`\040` space, `\011` tab, `\012` newline, `\134`
/// backslash); anything else is kept as written.
fn unescape_mount_point(point: &str) -> String {
  let bytes = point.as_bytes();
  let mut out = Vec::with_capacity(bytes.len());
  let mut at = 0usize;
  while let Some(byte) = bytes.get(at).copied() {
    let octal = bytes
      .get(at.saturating_add(1)..at.saturating_add(ESCAPE_LEN))
      .filter(|digits| byte == b'\\' && digits.iter().all(u8::is_ascii_digit))
      .and_then(|digits| u8::from_str_radix(std::str::from_utf8(digits).ok()?, ESCAPE_RADIX).ok());
    match octal {
      Some(decoded) => {
        out.push(decoded);
        at = at.saturating_add(ESCAPE_LEN);
      }
      None => {
        out.push(byte);
        at = at.saturating_add(1);
      }
    }
  }
  String::from_utf8_lossy(&out).into_owned()
}

/// Whether `path`'s longest-prefix mount is a kernel virtual filesystem.
fn on_kernel_virtual_mount(path: &str, mounts: &[Mount<'_>]) -> bool {
  mounts
    .iter()
    .filter(|mount| mount.point == "/" || inside(path, mount.point))
    .max_by_key(|mount| mount.point.len())
    .is_some_and(|mount| KERNEL_VIRTUAL_FILESYSTEMS.contains(&mount.fstype))
}

/// A granted landing as the trace must see it (D-26: "inside a granted target during that landing"): the
/// processes that execute it and the interval it ran in, on the tracer's clock.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Landing<'a> {
  /// The processes that execute the landing (the daemon).
  pub writers: &'a [u32],
  /// When the landing was requested, under its grant, in nanoseconds since the Unix epoch.
  pub from_ns: u64,
  /// When its outcome was returned.
  pub until_ns: u64,
}

/// A violation and its reason.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Violation {
  /// The call.
  pub event: WriteEvent,
  /// Why it is a violation.
  pub reason: &'static str,
}

/// Shape: violations kept in the report (the first ones; the count carries the rest).
pub const VIOLATION_SAMPLE: usize = 32;

/// The judgement of a trace.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Hermeticity {
  /// Write-capable calls seen.
  pub write_calls: u64,
  /// Calls inside the target.
  pub inside_target: u64,
  /// Calls on RAM-only objects.
  pub ram_only: u64,
  /// Writes to standard streams.
  pub standard_streams: u64,
  /// Calls without a resolvable path.
  pub unresolved: u64,
  /// Violations.
  pub outside: u64,
  /// The first violations, with their reasons.
  pub violations: Vec<Violation>,
  /// The first unresolved calls.
  pub unresolved_sample: Vec<WriteEvent>,
  /// Paths written inside the target, relative to it, unique and sorted.
  pub written_inside: Vec<String>,
}

impl Hermeticity {
  /// The counts for a record, given how many written paths the landing report matched.
  pub fn counts(&self, written_matched: u32, written_unmatched: u32) -> Counts {
    Counts::Hermeticity {
      write_calls: self.write_calls,
      inside_target: self.inside_target,
      ram_only: self.ram_only,
      standard_streams: self.standard_streams,
      unresolved: self.unresolved,
      outside: self.outside,
      written_matched,
      written_unmatched,
    }
  }
}

/// Format: the descriptor-path prefixes strace prints for kernel objects (`strace -y`).
const KERNEL_OBJECT_PREFIXES: &[&str] = &[
  "socket:",
  "UNIX:",
  "UNIX-STREAM:",
  "TCP:",
  "UDP:",
  "pipe:",
  "anon_inode:",
  "/memfd:",
  "shm:",
];
/// Format: the character devices a daemon's streams and null sinks reach.
/// `/dev/dtracehelper` is the DTrace helper device macOS's dynamic loader opens read-write in every process
/// it starts (seen once per slates process in the 2026-09-26 eslogger run).
const CHARACTER_DEVICES: &[&str] = &[
  "/dev/null",
  "/dev/zero",
  "/dev/tty",
  "/dev/ptmx",
  "/dev/dtracehelper",
];
/// Format: the prefix of the pseudo-path the parsers give a descriptor they could not resolve.
const UNRESOLVED_PREFIX: &str = "<fd ";

/// Whether `path` is `target` or below it.
fn inside(path: &str, target: &str) -> bool {
  let target = target.trim_end_matches('/');
  !target.is_empty()
    && (path == target
      || path
        .strip_prefix(target)
        .is_some_and(|rest| rest.starts_with('/')))
}

/// Whether `path` names a RAM-only object a standard stream may be: a kernel object (a pipe, a socket),
/// a terminal or a null device.
fn ram_object(path: &str) -> Option<&'static str> {
  if KERNEL_OBJECT_PREFIXES.iter().any(|p| path.starts_with(p)) {
    return Some("kernel object (socket, pipe, anon inode, memfd, shm)");
  }
  if path == "/dev/fuse" {
    return Some("the FUSE device");
  }
  if CHARACTER_DEVICES.contains(&path)
    || path.starts_with("/dev/pts/")
    || path.starts_with("/dev/ttys")
  {
    return Some("a character device");
  }
  None
}

/// Whether the granted landing accounts for a write inside the target: its own process, during it.
fn authorized(event: &WriteEvent, landing: Option<&Landing<'_>>) -> Result<(), &'static str> {
  let landing = landing.ok_or("inside the target with no granted landing")?;
  let pid = event
    .pid
    .ok_or("inside the target by a process the tracer did not name")?;
  if !landing.writers.contains(&pid) {
    return Err("inside the target by a process that is not the landing's");
  }
  let at = event
    .at_ns
    .ok_or("inside the target at a time the tracer did not stamp")?;
  if at < landing.from_ns || at > landing.until_ns {
    return Err("inside the target outside the granted landing's interval");
  }
  Ok(())
}

/// Places one event.
pub fn classify(event: &WriteEvent, policy: &Policy<'_>) -> Placement {
  let path = event.path.as_str();
  let object = ram_object(path);
  if matches!(event.descriptor, Some(1 | 2)) && is_stream_write(&event.call) && object.is_some() {
    return Placement::StandardStream;
  }
  if inside(path, policy.target) {
    return match authorized(event, policy.landing.as_ref()) {
      Ok(()) => Placement::InsideTarget,
      Err(reason) => Placement::Unauthorized(reason),
    };
  }
  if let Some(class) = object {
    return Placement::RamOnly(class);
  }
  if on_kernel_virtual_mount(path, policy.mounts) {
    return Placement::RamOnly("a kernel control file (its mount is a kernel virtual filesystem)");
  }
  if path.starts_with(UNRESOLVED_PREFIX) {
    return Placement::Unresolved;
  }
  Placement::Outside
}

/// Whether a call writes bytes to a descriptor (the calls a standard stream receives).
fn is_stream_write(call: &str) -> bool {
  matches!(
    call,
    "write"
      | "write_nocancel"
      | "pwrite"
      | "pwrite64"
      | "pwrite_nocancel"
      | "writev"
      | "writev_nocancel"
      | "pwritev"
      | "pwritev2"
  )
}

/// Judges every event under the policy.
pub fn judge(events: &[WriteEvent], policy: &Policy<'_>) -> Hermeticity {
  let mut out = Hermeticity::default();
  for event in events {
    out.write_calls += 1;
    match classify(event, policy) {
      Placement::InsideTarget => {
        out.inside_target += 1;
        note_written(&mut out.written_inside, &event.path, policy.target);
      }
      Placement::RamOnly(_) => out.ram_only += 1,
      Placement::StandardStream => out.standard_streams += 1,
      Placement::Unresolved => {
        out.unresolved += 1;
        keep_sample(&mut out.unresolved_sample, event);
      }
      Placement::Outside => {
        out.outside += 1;
        keep_violation(&mut out.violations, event, "outside every allowed class");
      }
      Placement::Unauthorized(reason) => {
        out.outside += 1;
        keep_violation(&mut out.violations, event, reason);
      }
    }
  }
  out.written_inside.sort();
  out.written_inside.dedup();
  out
}

fn keep_violation(sample: &mut Vec<Violation>, event: &WriteEvent, reason: &'static str) {
  if sample.len() < VIOLATION_SAMPLE {
    sample.push(Violation {
      event: event.clone(),
      reason,
    });
  }
}

fn keep_sample(sample: &mut Vec<WriteEvent>, event: &WriteEvent) {
  if sample.len() < VIOLATION_SAMPLE {
    sample.push(event.clone());
  }
}

fn note_written(written: &mut Vec<String>, path: &str, target: &str) {
  let relative = path
    .strip_prefix(target.trim_end_matches('/'))
    .map(|rest| rest.trim_start_matches('/'))
    .unwrap_or(path);
  if !relative.is_empty() {
    written.push(relative.to_owned());
  }
}

// --- strace ---------------------------------------------------------------------------------

/// Format: the open flags that make an `open` write-capable (`open(2)`).
const WRITE_FLAGS: &[&str] = &[
  "O_WRONLY",
  "O_RDWR",
  "O_CREAT",
  "O_TRUNC",
  "O_APPEND",
  "O_TMPFILE",
];

/// Whether an `open`-family flags text names a write-capable flag.
fn write_capable_flags(flags: &str) -> bool {
  flags
    .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
    .any(|word| WRITE_FLAGS.contains(&word))
}

/// The pid and body of a strace line (`123  body`, `[pid 123] body`, or a bare body).
fn split_pid(raw: &str) -> (String, &str) {
  if let Some((pid, body)) = raw
    .strip_prefix("[pid ")
    .and_then(|rest| rest.split_once(']'))
  {
    return (pid.trim().to_owned(), body.trim_start());
  }
  let digits = raw.len() - raw.trim_start_matches(|c: char| c.is_ascii_digit()).len();
  if digits > 0 && raw[digits..].starts_with(' ') {
    return (raw[..digits].to_owned(), raw[digits..].trim_start());
  }
  (String::new(), raw)
}

/// Format: strace's markers for a call interrupted by another thread's and its continuation.
const UNFINISHED: &str = " <unfinished ...>";
/// Format: the continuation marker's tail (`<... openat resumed>`).
const RESUMED: &str = " resumed>";

/// Format: nanoseconds per second and the digits of strace's `-ttt` fraction (microseconds).
const NANOS_PER_SECOND: u64 = 1_000_000_000;
/// Format: the nanoseconds one digit of a fraction is worth at each place, for a fraction of up to nine
/// digits (`-ttt` prints six).
const FRACTION_PLACES: usize = 9;

/// A `seconds.fraction` stamp (strace `-ttt`) as nanoseconds since the Unix epoch.
fn epoch_stamp_ns(token: &str) -> Option<u64> {
  let (seconds, fraction) = token.split_once('.')?;
  if seconds.is_empty()
    || fraction.is_empty()
    || fraction.len() > FRACTION_PLACES
    || !seconds.bytes().all(|b| b.is_ascii_digit())
    || !fraction.bytes().all(|b| b.is_ascii_digit())
  {
    return None;
  }
  let whole = seconds.parse::<u64>().ok()?.checked_mul(NANOS_PER_SECOND)?;
  let padded = format!("{fraction:0<FRACTION_PLACES$}");
  whole.checked_add(padded.parse::<u64>().ok()?)
}

/// The leading `-ttt` stamp of a strace body, and the rest.
fn split_stamp(body: &str) -> (Option<u64>, &str) {
  match body.split_once(' ') {
    Some((token, rest)) => match epoch_stamp_ns(token) {
      Some(at) => (Some(at), rest.trim_start()),
      None => (None, body),
    },
    None => (None, body),
  }
}

/// One whole strace call: its 1-based line, its process, when it began, and its text.
struct Joined {
  line: usize,
  pid: Option<u32>,
  at_ns: Option<u64>,
  body: String,
}

/// Joins `<unfinished ...>` / `resumed>` pairs per pid; yields whole call lines with their line numbers,
/// processes and (with `-ttt`) the time each call began.
fn joined_lines(log: &str) -> Vec<Joined> {
  let mut pending: HashMap<String, (Option<u64>, String)> = HashMap::new();
  let mut out = Vec::new();
  for (index, raw) in log.lines().enumerate() {
    let (pid_text, rest) = split_pid(raw);
    let (at_ns, body) = split_stamp(rest);
    let pid = pid_text.parse::<u32>().ok();
    if let Some(head) = body.strip_suffix(UNFINISHED) {
      pending.insert(pid_text, (at_ns, head.to_owned()));
      continue;
    }
    if body.starts_with("<...") {
      if let Some(position) = body.find(RESUMED) {
        let tail = &body[position + RESUMED.len()..];
        if let Some((began, head)) = pending.remove(&pid_text) {
          out.push(Joined {
            line: index + 1,
            pid,
            at_ns: began,
            body: format!("{head}{tail}"),
          });
        }
      }
      continue;
    }
    out.push(Joined {
      line: index + 1,
      pid,
      at_ns,
      body: body.to_owned(),
    });
  }
  out
}

/// The call name and its argument list from a whole strace line.
fn call_and_args(body: &str) -> Option<(&str, Vec<&str>)> {
  let open = body.find('(')?;
  let name = &body[..open];
  if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
    return None;
  }
  let close = body.rfind(") = ").or_else(|| body.rfind(')'))?;
  if close < open {
    return None;
  }
  Some((name, split_top_level(&body[open + 1..close])))
}

/// Splits an argument list at top-level commas (quotes, brackets and braces respected).
fn split_top_level(args: &str) -> Vec<&str> {
  let mut out = Vec::new();
  let mut depth = 0i32;
  let mut in_string = false;
  let mut escaped = false;
  let mut start = 0;
  for (index, c) in args.char_indices() {
    if in_string {
      match c {
        '\\' if !escaped => escaped = true,
        '"' if !escaped => in_string = false,
        _ => escaped = false,
      }
      continue;
    }
    match c {
      '"' => in_string = true,
      '[' | '{' | '(' => depth += 1,
      ']' | '}' | ')' => depth -= 1,
      ',' if depth == 0 => {
        out.push(args[start..index].trim());
        start = index + 1;
      }
      _ => {}
    }
  }
  if !args[start..].trim().is_empty() {
    out.push(args[start..].trim());
  }
  out
}

/// A quoted strace string, unescaped (`\"`, `\\`, `\n`, `\t`, octal); `NULL` and bare words are `None`.
fn quoted(arg: &str) -> Option<String> {
  let inner = arg.strip_prefix('"')?;
  let end = inner.rfind('"')?;
  let mut out = String::with_capacity(end);
  let mut chars = inner[..end].chars().peekable();
  while let Some(c) = chars.next() {
    if c != '\\' {
      out.push(c);
      continue;
    }
    match chars.next() {
      Some('n') => out.push('\n'),
      Some('t') => out.push('\t'),
      Some(d) if d.is_digit(OCTAL) => out.push(octal_char(d, &mut chars)),
      Some(other) => out.push(other),
      None => {}
    }
  }
  Some(out)
}

/// Format: strace prints non-printable bytes as up to three octal digits.
const OCTAL_DIGITS_AFTER_FIRST: usize = 2;
/// Format: the octal radix of those escapes.
const OCTAL: u32 = 8;

fn octal_char(first: char, chars: &mut std::iter::Peekable<std::str::Chars<'_>>) -> char {
  let mut value = first.to_digit(OCTAL).unwrap_or(0);
  for _ in 0..OCTAL_DIGITS_AFTER_FIRST {
    match chars.peek().and_then(|c| c.to_digit(OCTAL)) {
      Some(digit) => {
        value = value * OCTAL + digit;
        chars.next();
      }
      None => break,
    }
  }
  char::from_u32(value).unwrap_or('?')
}

/// A descriptor argument: its number and its decoration (`5</path>` → `(5, Some("/path"))`).
fn descriptor(arg: &str) -> (Option<i64>, Option<String>) {
  // strace can put the deletion annotation after the closing decoration bracket. It is
  // metadata about the descriptor, not part of its path or an exemption from containment.
  let arg = arg.strip_suffix("(deleted)").unwrap_or(arg);
  let digits = arg.len()
    - arg
      .trim_start_matches(|c: char| c.is_ascii_digit() || c == '-')
      .len();
  let number: Option<i64> = arg[..digits].parse().ok();
  let decoration = arg[digits..]
    .strip_prefix('<')
    .and_then(|rest| rest.strip_suffix('>'))
    .map(|inner| inner.trim_end_matches(" (deleted)").to_owned());
  (number, decoration)
}

/// The path of a descriptor argument, or the unresolved pseudo-path.
fn descriptor_path(arg: &str) -> (Option<i64>, String) {
  match descriptor(arg) {
    (number, Some(decoration)) => (number, decoration),
    (number, None) => (number, format!("{UNRESOLVED_PREFIX}{arg} undecoded>")),
  }
}

/// Descriptor paths returned by successful `O_TMPFILE` opens. These unnamed inodes belong
/// to their containing directory, but have no manifest name until `linkat` publishes them.
/// A `#number` basename alone never proves that a path is an unnamed temporary.
pub fn strace_unnamed_paths(log: &str) -> Vec<String> {
  joined_lines(log)
    .into_iter()
    .filter_map(|Joined { body, .. }| {
      let (name, args) = call_and_args(&body)?;
      let flags = match name {
        "open" => args.get(1)?,
        "openat" | "openat2" => args.get(2)?,
        _ => return None,
      };
      if !flags
        .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .any(|word| word == "O_TMPFILE")
      {
        return None;
      }
      let (_, returned) = body.rsplit_once(") = ")?;
      let (number, path) = descriptor(returned);
      number.filter(|number| *number >= 0).and(path)
    })
    .collect()
}

/// A path relative to a directory descriptor (`AT_FDCWD` → the working directory).
fn at_path(dirfd: &str, path: Option<&str>, cwd: &str) -> String {
  let (_, base) = if dirfd == "AT_FDCWD" {
    (None, cwd.to_owned())
  } else {
    descriptor_path(dirfd)
  };
  match path {
    Some(p) if p.starts_with('/') => p.to_owned(),
    Some("" | ".") | None => base,
    Some(p) => format!("{}/{p}", base.trim_end_matches('/')),
  }
}

/// Format: `renameat(olddirfd, oldpath, newdirfd, newpath)` and `linkat(olddirfd, oldpath,
/// newdirfd, newpath, flags)` name the new entry in their third and fourth arguments
/// (`renameat(2)`, `linkat(2)`).
const NEW_NAME_DIRFD: usize = 2;
/// Format: the new name's path argument of `renameat`/`linkat` (see [`NEW_NAME_DIRFD`]).
const NEW_NAME_PATH: usize = 3;

fn event(line: usize, call: &str, path: String, descriptor: Option<i64>) -> WriteEvent {
  WriteEvent {
    line,
    call: call.to_owned(),
    path,
    descriptor,
    pid: None,
    at_ns: None,
  }
}

/// The events one strace call contributes (a rename contributes two).
fn strace_events(line: usize, name: &str, args: &[&str], cwd: &str) -> Vec<WriteEvent> {
  let arg = |i: usize| args.get(i).copied().unwrap_or("");
  let path_arg = |i: usize| quoted(arg(i));
  let absolute = |p: Option<String>| match p {
    Some(p) if p.starts_with('/') => p,
    Some(p) => format!("{}/{p}", cwd.trim_end_matches('/')),
    None => format!("{UNRESOLVED_PREFIX}path missing>"),
  };
  match name {
    "open" if write_capable_flags(arg(1)) => vec![event(line, name, absolute(path_arg(0)), None)],
    "creat" => vec![event(line, name, absolute(path_arg(0)), None)],
    "openat" | "openat2" if write_capable_flags(arg(2)) => {
      vec![event(
        line,
        name,
        at_path(arg(0), path_arg(1).as_deref(), cwd),
        None,
      )]
    }
    "mkdir" | "rmdir" | "unlink" | "truncate" | "chmod" | "chown" | "lchown" | "utimes"
    | "utime" | "mknod" => {
      vec![event(line, name, absolute(path_arg(0)), None)]
    }
    "mkdirat" | "unlinkat" | "fchmodat" | "fchmodat2" | "fchownat" | "utimensat" | "mknodat" => {
      vec![event(
        line,
        name,
        at_path(arg(0), path_arg(1).as_deref(), cwd),
        None,
      )]
    }
    "rename" => vec![
      event(line, name, absolute(path_arg(0)), None),
      event(line, name, absolute(path_arg(1)), None),
    ],
    "renameat" | "renameat2" => vec![
      event(
        line,
        name,
        at_path(arg(0), path_arg(1).as_deref(), cwd),
        None,
      ),
      event(
        line,
        name,
        at_path(arg(NEW_NAME_DIRFD), path_arg(NEW_NAME_PATH).as_deref(), cwd),
        None,
      ),
    ],
    "link" | "symlink" => vec![event(line, name, absolute(path_arg(1)), None)],
    "linkat" => vec![event(
      line,
      name,
      at_path(arg(NEW_NAME_DIRFD), path_arg(NEW_NAME_PATH).as_deref(), cwd),
      None,
    )],
    "symlinkat" => vec![event(
      line,
      name,
      at_path(arg(1), path_arg(2).as_deref(), cwd),
      None,
    )],
    _ => strace_descriptor_events(line, name, args),
  }
}

/// The events of the descriptor-based write calls.
fn strace_descriptor_events(line: usize, name: &str, args: &[&str]) -> Vec<WriteEvent> {
  let arg = |i: usize| args.get(i).copied().unwrap_or("");
  let index = match name {
    "write" | "pwrite" | "pwrite64" | "writev" | "pwritev" | "pwritev2" | "ftruncate"
    | "fchmod" | "fchown" | "fsync" | "fdatasync" | "fallocate" | "sendfile" | "vmsplice"
    | "futimens" => 0,
    "copy_file_range" | "splice" => 2,
    _ => return Vec::new(),
  };
  let (number, path) = descriptor_path(arg(index));
  vec![event(line, name, path, number)]
}

/// Parses an strace log (`-f -y`, one call per line) into its write-capable events.
pub fn parse_strace(log: &str) -> Vec<WriteEvent> {
  parse_strace_with_cwd(log, ".")
}

/// Parses an strace log with the traced process's working directory for `AT_FDCWD` paths. Each event
/// carries its process and, when the log was written with `-ttt`, the time its call began.
pub fn parse_strace_with_cwd(log: &str, cwd: &str) -> Vec<WriteEvent> {
  let mut events = Vec::new();
  for joined in joined_lines(log) {
    if let Some((name, args)) = call_and_args(&joined.body) {
      for mut event in strace_events(joined.line, name, &args, cwd) {
        event.pid = joined.pid;
        event.at_ns = joined.at_ns;
        events.push(event);
      }
    }
  }
  events
}

/// Format: the OS-shipped mount brokers a Linux mount runs (R10: "the only privileged pieces are
/// OS-shipped brokers installed once"): `fusermount3`, the setuid helper libfuse ships, and util-linux's
/// `mount`, which `fusermount3` runs for an `allow_other` mount. Their writes are the operating system's
/// own mount bookkeeping (libmount's runtime directory `/run/mount`, seen in the 2026-10-01 container-lane
/// trace as `/bin/mount`'s `mkdirat /run/mount`), the counterpart of macOS's `mount_nfs`, whose events the
/// eslogger leg never judges because it keeps only the slates executable's. The paths are Debian's and
/// Ubuntu's (`/bin` is `/usr/bin` there; both spellings, since strace prints the one `execve` named).
pub const OS_MOUNT_BROKERS: &[&str] = &[
  "/usr/bin/fusermount3",
  "/bin/fusermount3",
  "/usr/bin/mount",
  "/bin/mount",
];

/// Splits an strace log's events into those to judge and those an OS mount broker made
/// ([`OS_MOUNT_BROKERS`]). An event is the broker's when its process's last successful `execve` before
/// the event's line named a broker image; a process that never ran one, a call before its `execve`, and a
/// failed `execve` all stay judged, so only a broker's own image is set aside, never a slates process.
pub fn strace_split_broker_events(
  log: &str,
  events: Vec<WriteEvent>,
) -> (Vec<WriteEvent>, Vec<WriteEvent>) {
  let mut image_is_broker: HashMap<u32, bool> = HashMap::new();
  let mut broker_lines: std::collections::HashSet<usize> = std::collections::HashSet::new();
  for joined in joined_lines(log) {
    let Some(pid) = joined.pid else {
      continue;
    };
    if let Some(("execve", args)) = call_and_args(&joined.body)
      && joined.body.trim_end().ends_with(") = 0")
    {
      let image = args.first().copied().and_then(quoted);
      image_is_broker.insert(
        pid,
        image.is_some_and(|image| OS_MOUNT_BROKERS.contains(&image.as_str())),
      );
    }
    if image_is_broker.get(&pid).copied().unwrap_or(false) {
      broker_lines.insert(joined.line);
    }
  }
  events
    .into_iter()
    .partition(|event| !broker_lines.contains(&event.line))
}

// --- fs_usage --------------------------------------------------------------------------------

/// Format: the calls fs_usage names that create or open (`fs_usage.c`'s syscall table).
const FS_USAGE_OPENS: &[&str] = &[
  "open",
  "open_nocancel",
  "openat",
  "openat_nocancel",
  "open_extended",
  "open_dprotected",
  "creat",
  "guarded_open_np",
];
/// Format: fs_usage's names of the calls that mutate a path.
const FS_USAGE_PATH_WRITES: &[&str] = &[
  "rename",
  "renameat",
  "renamex_np",
  "renameatx_np",
  "unlink",
  "unlinkat",
  "mkdir",
  "mkdirat",
  "rmdir",
  "link",
  "linkat",
  "symlink",
  "symlinkat",
  "truncate",
  "chmod",
  "chmod_extended",
  "fchmodat",
  "chown",
  "lchown",
  "fchownat",
  "utimes",
  "utimensat",
  "mknod",
  "mkfifo",
  "clonefile",
  "clonefileat",
  "exchangedata",
  "setattrlist",
  "setxattr",
  "removexattr",
];
/// Format: fs_usage's names of the calls that mutate through a descriptor.
const FS_USAGE_DESCRIPTOR_WRITES: &[&str] = &[
  "write",
  "write_nocancel",
  "pwrite",
  "pwrite_nocancel",
  "writev",
  "writev_nocancel",
  "pwritev",
  "ftruncate",
  "fchmod",
  "fchmod_extended",
  "fchown",
  "fsync",
  "fdatasync",
  "fsetattrlist",
  "fsetxattr",
  "fremovexattr",
  "futimes",
  "futimens",
  "fclonefileat",
];
/// Format: fs_usage's names of the calls whose result descriptor is a kernel object.
const FS_USAGE_OBJECT_MAKERS: &[(&str, &str)] = &[
  ("socket", "socket:"),
  ("accept", "socket:"),
  ("accept_nocancel", "socket:"),
  ("socketpair", "socket:"),
  ("pipe", "pipe:"),
  ("kqueue", "anon_inode:kqueue"),
  ("shm_open", "shm:"),
];

/// One fs_usage row, tokenized.
struct Row<'a> {
  call: &'a str,
  descriptor: Option<i64>,
  failed: bool,
  flags: Option<&'a str>,
  paths: Vec<&'a str>,
}

/// Format: fs_usage's timestamp is `HH:MM:SS` (three two-digit fields), with microseconds in wide mode.
const TIMESTAMP_FIELDS: usize = 3;
/// Format: the fewest tokens a row has: the timestamp, the call, the elapsed time, the process.
const MIN_ROW_TOKENS: usize = 4;
/// Format: the trailing tokens of a row: the elapsed time and the process name (`format_print`).
const TRAILING_TOKENS: usize = 2;
/// Format: the trailing tokens when the call waited: the elapsed time, `W`, the process name.
const TRAILING_TOKENS_WAITED: usize = 3;

/// Whether a token is fs_usage's timestamp (`HH:MM:SS` or `HH:MM:SS.uuuuuu`).
fn is_timestamp(token: &str) -> bool {
  let clock = token.split('.').next().unwrap_or("");
  let parts: Vec<&str> = clock.split(':').collect();
  parts.len() == TIMESTAMP_FIELDS
    && parts
      .iter()
      .all(|p| p.len() == 2 && p.chars().all(|c| c.is_ascii_digit()))
}

fn read_row(raw: &str) -> Option<Row<'_>> {
  let tokens: Vec<&str> = raw.split_whitespace().collect();
  if tokens.len() < MIN_ROW_TOKENS || !is_timestamp(tokens[0]) {
    return None;
  }
  let mut row = Row {
    call: tokens[1],
    descriptor: None,
    failed: false,
    flags: None,
    paths: Vec::new(),
  };
  // The last tokens are the elapsed time, an optional `W`, and the process name; skip them.
  let waited = tokens[tokens.len() - TRAILING_TOKENS] == "W";
  let body_end = tokens.len().saturating_sub(if waited {
    TRAILING_TOKENS_WAITED
  } else {
    TRAILING_TOKENS
  });
  let elapsed = tokens.get(body_end)?.parse::<f64>().ok()?;
  if !elapsed.is_finite() || elapsed < 0.0 {
    return None;
  }
  for token in tokens.get(2..body_end)? {
    read_token(token, &mut row);
  }
  Some(row)
}

/// Whether a complete fs_usage event has arrived, including read-only activity. Startup
/// banners, diagnostics and a partially flushed final row do not establish attachment.
pub fn fs_usage_has_activity(log: &str) -> bool {
  log
    .split_inclusive('\n')
    .any(|line| line.ends_with('\n') && read_row(line).is_some())
}

fn read_token<'a>(token: &'a str, row: &mut Row<'a>) {
  if let Some(fd) = token.strip_prefix("F=") {
    row.descriptor = fd.parse().ok();
  } else if token.starts_with('[') && !token.contains('/')
    || token.ends_with(']') && !token.contains('/')
  {
    row.failed = true;
  } else if token.starts_with('(') && token.ends_with(')') {
    row.flags = Some(&token[1..token.len() - 1]);
  } else if token.starts_with('/') || (token.starts_with('[') && token.contains("]/")) {
    row.paths.push(token);
  }
}

/// Whether fs_usage's open flags (`print_open`: position 1 is `W`, then `C`, `A`, `T`) allow a write.
fn fs_usage_write_flags(flags: &str) -> bool {
  flags.contains('W') || flags.contains('C') || flags.contains('A') || flags.contains('T')
}

/// Resolves an fs_usage path token through the descriptor table (`[5]/name` → `<path of 5>/name`).
fn fs_usage_path(token: &str, table: &HashMap<i64, String>) -> String {
  let Some(rest) = token.strip_prefix('[') else {
    return token.to_owned();
  };
  let Some((fd, name)) = rest.split_once("]/") else {
    return token.to_owned();
  };
  match fd.parse::<i64>().ok().and_then(|fd| table.get(&fd)) {
    Some(base) => format!("{}/{name}", base.trim_end_matches('/')),
    None => format!("{UNRESOLVED_PREFIX}{fd} unknown>/{name}"),
  }
}

/// Parses an fs_usage log (`-w -f filesys -f network PID`, one process) into its write events.
pub fn parse_fs_usage(log: &str) -> Vec<WriteEvent> {
  let mut table: HashMap<i64, String> = HashMap::new();
  let mut events = Vec::new();
  for (index, raw) in log.lines().enumerate() {
    let Some(row) = read_row(raw) else {
      continue;
    };
    fs_usage_row_events(index + 1, &row, &mut table, &mut events);
  }
  events
}

/// An open-family row: the descriptor it returned is recorded, and a write-capable open is an event.
fn fs_usage_open_event(
  line: usize,
  row: &Row<'_>,
  table: &mut HashMap<i64, String>,
  events: &mut Vec<WriteEvent>,
) {
  let path = row.paths.first().map_or_else(
    || format!("{UNRESOLVED_PREFIX}open without a path>"),
    |p| fs_usage_path(p, table),
  );
  if let (Some(fd), false) = (row.descriptor, row.failed) {
    table.insert(fd, path.clone());
  }
  if row.flags.is_some_and(fs_usage_write_flags) {
    events.push(event(line, row.call, path, None));
  }
}

fn fs_usage_row_events(
  line: usize,
  row: &Row<'_>,
  table: &mut HashMap<i64, String>,
  events: &mut Vec<WriteEvent>,
) {
  if FS_USAGE_OPENS.contains(&row.call) {
    fs_usage_open_event(line, row, table, events);
  } else if let Some((_, class)) = FS_USAGE_OBJECT_MAKERS
    .iter()
    .find(|(name, _)| *name == row.call)
  {
    if let (Some(fd), false) = (row.descriptor, row.failed) {
      table.insert(fd, (*class).to_owned());
    }
  } else if row.call == "close" {
    if let Some(fd) = row.descriptor {
      table.remove(&fd);
    }
  } else if FS_USAGE_PATH_WRITES.contains(&row.call) {
    for path in &row.paths {
      events.push(event(line, row.call, fs_usage_path(path, table), None));
    }
  } else if FS_USAGE_DESCRIPTOR_WRITES.contains(&row.call) {
    events.push(fs_usage_descriptor_event(line, row, table));
  }
}

fn fs_usage_descriptor_event(
  line: usize,
  row: &Row<'_>,
  table: &HashMap<i64, String>,
) -> WriteEvent {
  let path = match (
    row.paths.first(),
    row.descriptor.and_then(|fd| table.get(&fd)),
  ) {
    (Some(p), _) => fs_usage_path(p, table),
    (None, Some(known)) => known.clone(),
    (None, None) => format!(
      "{UNRESOLVED_PREFIX}{} unknown>",
      row.descriptor.map_or("?".to_owned(), |fd| fd.to_string())
    ),
  };
  event(line, row.call, path, row.descriptor)
}

// --- eslogger --------------------------------------------------------------------------------

/// Format: the kernel open-file flags (`fflag`, `<sys/fcntl.h>`) that make an `open` event write-capable:
/// `FWRITE` (0x2), `O_APPEND` (0x8), `O_CREAT` (0x200), `O_TRUNC` (0x400).
const ES_WRITE_FFLAGS: i64 = 0x2 | 0x8 | 0x200 | 0x400;

/// The path an Endpoint Security `es_file_t` names, `None` when absent or truncated by the kernel.
fn es_file(value: &serde_json::Value) -> Option<String> {
  if value
    .get("path_truncated")
    .and_then(serde_json::Value::as_bool)
    == Some(true)
  {
    return None;
  }
  value
    .get("path")
    .and_then(serde_json::Value::as_str)
    .map(str::to_owned)
}

/// A directory `es_file_t` and a name in it, joined.
fn es_in_dir(dir: &serde_json::Value, name: Option<&serde_json::Value>) -> Option<String> {
  let dir = es_file(dir)?;
  let name = name?.as_str()?;
  Some(format!("{}/{name}", dir.trim_end_matches('/')))
}

/// A destination union (`create`, `rename`): an existing file, or a new name in a directory.
fn es_destination(destination: &serde_json::Value) -> Option<String> {
  if let Some(existing) = destination.get("existing_file") {
    return es_file(existing);
  }
  let new_path = destination.get("new_path")?;
  es_in_dir(new_path.get("dir")?, new_path.get("filename"))
}

/// The write-capable paths one Endpoint Security event names, `None` when the kind is not
/// write-capable (or an `open` without write flags, a `close` of an unmodified file); a path the kernel
/// did not give is `Some(None)`, which the caller records unresolved.
fn es_paths(kind: &str, event: &serde_json::Value) -> Option<Vec<Option<String>>> {
  let file = |key: &str| event.get(key).and_then(es_file);
  Some(match kind {
    "open" => {
      let flags = event.get("fflag").and_then(serde_json::Value::as_i64)?;
      if flags & ES_WRITE_FFLAGS == 0 {
        return None;
      }
      vec![file("file")]
    }
    "close" => {
      if event.get("modified").and_then(serde_json::Value::as_bool) != Some(true) {
        return None;
      }
      vec![file("target")]
    }
    "write" | "truncate" | "unlink" | "setextattr" | "deleteextattr" | "setmode" | "setowner"
    | "setflags" | "utimes" | "setattrlist" | "setacl" => vec![file("target")],
    "create" => vec![event.get("destination").and_then(es_destination)],
    "rename" => vec![
      file("source"),
      event.get("destination").and_then(es_destination),
    ],
    "link" => vec![
      event
        .get("target_dir")
        .and_then(|dir| es_in_dir(dir, event.get("target_filename"))),
    ],
    "clone" | "copyfile" => vec![file("target_file").or_else(|| {
      event
        .get("target_dir")
        .and_then(|dir| es_in_dir(dir, event.get("target_name")))
    })],
    "exchangedata" => vec![file("file1"), file("file2")],
    _ => return None,
  })
}

/// Format: seconds per day, days per 400-year Gregorian era, and the day count from 0000-03-01 to the
/// Unix epoch — the constants of the civil-to-days conversion (Howard Hinnant, "chrono-Compatible
/// Low-Level Date Algorithms", `days_from_civil`; evidence C).
const SECONDS_PER_DAY: u64 = 86_400;
/// Format: days in a 400-year Gregorian era.
const DAYS_PER_ERA: i64 = 146_097;
/// Format: days from 0000-03-01 to 1970-01-01.
const EPOCH_DAYS: i64 = 719_468;

/// Format: the months of a year and the most days of a month (a date outside them is refused).
const MONTHS: i64 = 12;
/// Format: the most days in a month.
const MOST_DAYS: i64 = 31;
/// Format: the years of a Gregorian era (its leap-year rule repeats every 400 years).
const YEARS_PER_ERA: i64 = 400;
/// Format: the months before March, which the algorithm counts at the end of the previous year (so the
/// leap day ends the shifted year): January and February.
const MONTHS_BEFORE_MARCH: i64 = 2;
/// Format: the shift that makes March month 0 (`month - 3`) and January and February months 10 and 11
/// (`month + 9`).
const MARCH: i64 = 3;
/// Format: see [`MARCH`].
const JANUARY_SHIFT: i64 = 9;
/// Format: the day-of-year of a shifted month is `(153 × month + 2) / 5` (the 30/31-day rhythm from March).
const MONTH_RHYTHM_DAYS: i64 = 153;
/// Format: see [`MONTH_RHYTHM_DAYS`].
const MONTH_RHYTHM_OFFSET: i64 = 2;
/// Format: see [`MONTH_RHYTHM_DAYS`].
const MONTH_RHYTHM_MONTHS: i64 = 5;
/// Format: the days of a common year.
const DAYS_PER_YEAR: i64 = 365;
/// Format: a leap year every four years, none every hundredth.
const LEAP_EVERY: i64 = 4;
/// Format: see [`LEAP_EVERY`].
const NO_LEAP_EVERY: i64 = 100;
/// Format: an RFC 3339 date and clock each have three fields.
const RFC3339_FIELDS: usize = 3;

/// Days since the Unix epoch of a civil date (proleptic Gregorian).
fn days_from_civil(year: i64, month: i64, day: i64) -> Option<i64> {
  if !(1..=MONTHS).contains(&month) || !(1..=MOST_DAYS).contains(&day) {
    return None;
  }
  let year = if month <= MONTHS_BEFORE_MARCH {
    year - 1
  } else {
    year
  };
  let era = year.div_euclid(YEARS_PER_ERA);
  let of_era = year - era * YEARS_PER_ERA;
  let shifted = if month > MONTHS_BEFORE_MARCH {
    month - MARCH
  } else {
    month + JANUARY_SHIFT
  };
  let of_year = (MONTH_RHYTHM_DAYS * shifted + MONTH_RHYTHM_OFFSET) / MONTH_RHYTHM_MONTHS + day - 1;
  let of_era_days = of_era * DAYS_PER_YEAR + of_era / LEAP_EVERY - of_era / NO_LEAP_EVERY + of_year;
  Some(era * DAYS_PER_ERA + of_era_days - EPOCH_DAYS)
}

/// The three `separator`-joined numbers of an RFC 3339 date or clock.
fn three_fields<T: std::str::FromStr>(text: &str, separator: char) -> Option<(T, T, T)> {
  let mut fields = text
    .splitn(RFC3339_FIELDS, separator)
    .map(|part| part.parse::<T>().ok());
  Some((fields.next()??, fields.next()??, fields.next()??))
}

/// The seconds since the Unix epoch of an RFC 3339 date and clock (`2026-09-26`, `08:20:01`).
fn civil_seconds(date: &str, clock: &str) -> Option<u64> {
  let (year, month, day) = three_fields::<i64>(date, '-')?;
  let (hour, minute, second) = three_fields::<u64>(clock, ':')?;
  let days = u64::try_from(days_from_civil(year, month, day)?).ok()?;
  days
    .checked_mul(SECONDS_PER_DAY)?
    .checked_add(hour.checked_mul(SECONDS_PER_HOUR)?)?
    .checked_add(minute.checked_mul(SECONDS_PER_MINUTE)?)?
    .checked_add(second)
}

/// Format: seconds per hour and per minute.
const SECONDS_PER_HOUR: u64 = 3_600;
/// Format: seconds per minute.
const SECONDS_PER_MINUTE: u64 = 60;

/// An RFC 3339 UTC time (`2026-09-26T08:20:01.123456789Z`, as eslogger prints `time`) as nanoseconds
/// since the Unix epoch; `None` for anything else.
fn iso_ns(text: &str) -> Option<u64> {
  let (date, time) = text.strip_suffix('Z')?.split_once('T')?;
  let (clock, fraction) = time
    .split_once('.')
    .map_or((time, None), |(c, f)| (c, Some(f)));
  let whole = civil_seconds(date, clock)?.checked_mul(NANOS_PER_SECOND)?;
  match fraction {
    None => Some(whole),
    Some(fraction) => epoch_stamp_ns(&format!("0.{fraction}")).and_then(|f| whole.checked_add(f)),
  }
}

/// Format: the prefix of every hidden sibling a landing creates inside its target, and the forms that
/// follow it (`slates-land`'s engine: `.slates-{id:016x}-{n}`, `.slates-{id:016x}-aside-{hash:016x}`).
const HIDDEN_PREFIX: &str = ".slates-";
/// Format: the aside mark in a hidden name.
const ASIDE_MARK: &str = "aside-";
/// Format: the hex digits of a landing id and of an aside path hash (`{:016x}`).
const HIDDEN_HEX_DIGITS: usize = 16;

/// Whether `name` is one of the hidden names the landing `landing_id` creates: its counter form or its
/// aside form, with exactly the engine's digits. Any other `.slates-*` name is not the landing's.
pub fn is_landing_hidden_name(name: &str, landing_id: u64) -> bool {
  let own = format!("{HIDDEN_PREFIX}{landing_id:016x}-");
  let Some(rest) = name.strip_prefix(&own) else {
    return false;
  };
  if let Some(hash) = rest.strip_prefix(ASIDE_MARK) {
    return hash.len() == HIDDEN_HEX_DIGITS && hash.bytes().all(|b| b.is_ascii_hexdigit());
  }
  !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit())
}

/// Judges the hidden siblings a landing wrote inside its target (AUD-29-42): each must be one of this
/// landing's own names, and none may remain on disk once it ends. Returns how many it wrote, or the
/// first name that fails with its reason. `written` and `remaining` are paths relative to the target.
pub fn judge_hidden(
  written: &[String],
  remaining: &[String],
  landing_id: u64,
) -> Result<u32, String> {
  let base = |path: &str| path.rsplit('/').next().unwrap_or(path).to_owned();
  let mut count = 0u32;
  for path in written {
    let name = base(path);
    if !name.starts_with(HIDDEN_PREFIX) {
      continue;
    }
    if !is_landing_hidden_name(&name, landing_id) {
      return Err(format!(
        "{path}: a hidden name that is not landing {landing_id:016x}'s"
      ));
    }
    count = count.saturating_add(1);
  }
  if let Some(left) = remaining
    .iter()
    .find(|path| base(path).starts_with(HIDDEN_PREFIX))
  {
    return Err(format!(
      "{left}: a hidden sibling left behind after the landing"
    ));
  }
  Ok(count)
}

/// Whether an eslogger log holds at least one complete event (the tracer has attached).
pub fn eslogger_has_activity(log: &str) -> bool {
  log.lines().any(|line| {
    serde_json::from_str::<serde_json::Value>(line).is_ok_and(|v| v.get("event").is_some())
  })
}

/// The write-capable events of an eslogger JSON-lines log. A line that is not a complete event is an
/// unresolved event (a torn or foreign line cannot hide a write); an event of a write-capable kind
/// without its path is unresolved.
pub fn parse_eslogger(log: &str) -> Vec<WriteEvent> {
  let mut out = Vec::new();
  for (index, line) in log.lines().enumerate() {
    let number = index + 1;
    if line.trim().is_empty() {
      continue;
    }
    let Some((kind, body)) = serde_json::from_str::<serde_json::Value>(line)
      .ok()
      .and_then(|value| {
        let object = value.get("event")?.as_object()?;
        let (kind, body) = object.iter().next()?;
        Some((kind.clone(), body.clone()))
      })
    else {
      out.push(event(
        number,
        "unparsed",
        format!("{UNRESOLVED_PREFIX}line>"),
        None,
      ));
      continue;
    };
    let value = serde_json::from_str::<serde_json::Value>(line).ok();
    let pid = value
      .as_ref()
      .and_then(|v| v.pointer("/process/audit_token/pid"))
      .and_then(serde_json::Value::as_u64)
      .and_then(|pid| u32::try_from(pid).ok());
    let at_ns = value
      .as_ref()
      .and_then(|v| v.get("time"))
      .and_then(serde_json::Value::as_str)
      .and_then(iso_ns);
    for path in es_paths(&kind, &body).unwrap_or_default() {
      let path = path.unwrap_or_else(|| format!("{UNRESOLVED_PREFIX}{kind}>"));
      let mut written = event(number, &kind, path, None);
      written.pid = pid;
      written.at_ns = at_ns;
      out.push(written);
    }
  }
  out
}

#[cfg(test)]
mod tests {
  use super::*;

  /// AC-4.5: only a complete event establishes tracer readiness, including read-only activity.
  #[test]
  fn fs_usage_readiness_requires_a_complete_event() {
    let read = "08:20:01.000001 read F=5 B=0x4 0.000001 W slates.123";
    for incomplete in [
      "",
      "fs_usage: buffer overrun\n",
      "08:20:01.000001 write W slates.123\n",
      read,
    ] {
      assert!(!fs_usage_has_activity(incomplete), "{incomplete}");
    }
    assert!(fs_usage_has_activity(&format!("{read}\n")));
    assert!(parse_fs_usage(&format!("{read}\n")).is_empty());
  }

  const TARGET: &str = "/scratch/land-target";

  /// Format: the processes the fixtures' landing runs in (the strace and eslogger fixtures' pids).
  const LANDERS: &[u32] = &[1, 7, 91, 100, 101, 102, 103, 77040];
  /// Format: the stamp the fixtures' calls carry (strace `-ttt`), and the landing window around it.
  const STAMP: &str = "1.000001";
  /// Format: the stamp as eslogger's RFC 3339 `time`.
  const ES_TIME: &str = "1970-01-01T00:00:01.000001Z";
  /// Format: the fixtures' landing window, in nanoseconds since the epoch: one second to two.
  const WINDOW: (u64, u64) = (1_000_000_000, 2_000_000_000);

  fn landing() -> Landing<'static> {
    Landing {
      writers: LANDERS,
      from_ns: WINDOW.0,
      until_ns: WINDOW.1,
    }
  }

  fn policy() -> Policy<'static> {
    Policy {
      target: TARGET,
      working_directory: "/scratch/cwd",
      landing: Some(landing()),
      mounts: &[],
    }
  }

  /// An strace log as `-ttt` writes it: every call line stamped at [`STAMP`].
  fn stamped(log: &str) -> String {
    log
      .lines()
      .map(|line| {
        let (pid, body) = split_pid(line);
        if pid.is_empty() {
          line.to_owned()
        } else {
          format!("{pid} {STAMP} {body}")
        }
      })
      .collect::<Vec<_>>()
      .join("\n")
  }

  fn paths(violations: &[Violation]) -> Vec<&str> {
    violations.iter().map(|v| v.event.path.as_str()).collect()
  }

  /// Events in the shape `eslogger` prints (one JSON object per line, `event: {<kind>: {...}}`, each
  /// file an `es_file_t` with its full path): the landing's writes inside the target, the daemon's
  /// stderr log, a read-only open, an unmodified close, and violations of every write-capable kind.
  #[test]
  fn eslogger_events_are_placed_by_their_kernel_paths() {
    let file = |path: &str| format!(r#"{{"path":"{path}","path_truncated":false}}"#);
    let line = |kind: &str, body: String| {
      format!(
        r#"{{"event_type":0,"time":"{ES_TIME}","event":{{"{kind}":{body}}},"process":{{"audit_token":{{"pid":7}}}}}}"#
      )
    };
    let log = [
      line("open", format!(r#"{{"fflag":514,"file":{}}}"#, file("/scratch/land-target/.slates-tmp-1"))),
      line("write", format!(r#"{{"target":{}}}"#, file("/scratch/land-target/.slates-tmp-1"))),
      line("rename", format!(r#"{{"source":{},"destination_type":1,"destination":{{"new_path":{{"dir":{},"filename":"a.txt"}}}}}}"#, file("/scratch/land-target/.slates-tmp-1"), file("/scratch/land-target"))),
      line("write", format!(r#"{{"target":{}}}"#, file("/scratch/anchor.log"))),
      line("open", format!(r#"{{"fflag":1,"file":{}}}"#, file("/usr/lib/dyld"))),
      line("close", format!(r#"{{"modified":false,"target":{}}}"#, file("/etc/hosts"))),
      line("create", format!(r#"{{"destination_type":1,"destination":{{"new_path":{{"dir":{},"filename":"evil","mode":420}}}}}}"#, file("/etc"))),
      line("unlink", format!(r#"{{"target":{},"parent_dir":{}}}"#, file("/Users/u/x"), file("/Users/u"))),
      line("setextattr", format!(r#"{{"target":{},"extattr":"k"}}"#, file("/Users/u/y"))),
      line("truncate", r#"{"target":{"path":"/scratch/land-target/b","path_truncated":true}}"#.to_owned()),
      line("mmap", format!(r#"{{"source":{}}}"#, file("/usr/lib/libSystem.B.dylib"))),
      "not json".to_owned(),
    ]
    .join("\n");
    let events = parse_eslogger(&log);
    let judged = judge(&events, &policy());
    assert_eq!(
      judged.inside_target, 4,
      "open, write, rename's two names: {events:?}"
    );
    assert_eq!(
      judged.written_inside,
      vec![".slates-tmp-1".to_owned(), "a.txt".to_owned()]
    );
    assert_eq!(
      judged.standard_streams, 0,
      "a log file is a file, whatever writes to it"
    );
    assert_eq!(
      judged.outside, 4,
      "the stderr log file, create, unlink, setextattr: {:?}",
      judged.violations
    );
    assert_eq!(judged.unresolved, 2, "the truncated path and the torn line");
    assert_eq!(
      judged.write_calls, 10,
      "read-only opens, clean closes and mmaps are not writes"
    );
    assert!(eslogger_has_activity(&log));
    assert!(!eslogger_has_activity("not json\n{}\n"));
  }

  const STRACE: &str = "\
100 openat(AT_FDCWD, \"/proc/self/status\", O_RDONLY|O_CLOEXEC) = 3</proc/100/status>
100 openat(5</scratch/land-target>, \".slates-tmp-1\", O_WRONLY|O_CREAT|O_EXCL|O_CLOEXEC, 0600) = 6</scratch/land-target/.slates-tmp-1>
100 write(6</scratch/land-target/.slates-tmp-1>, \"hello\", 5) = 5
100 fdatasync(6</scratch/land-target/.slates-tmp-1>) = 0
100 renameat(5</scratch/land-target>, \".slates-tmp-1\", 5</scratch/land-target>, \"a.txt\") = 0
100 write(2</dev/null>, \"log line\", 8) = 8
100 write(1</home/runner/daemon.log>, \"log line\", 8) = 8
101 write(7<socket:[12345]>, \"\\1\\2\", 2) = 2
101 write(8</memfd:slates-seg (deleted)>, \"x\", 1) = 1
101 openat(AT_FDCWD, \"/dev/shm/slates-con\", O_RDWR|O_CREAT, 0600) = 9</dev/shm/slates-con>
102 mkdir(\"/etc/evil\", 0755) = -1 EACCES (Permission denied)
102 openat(AT_FDCWD, \"relative.txt\", O_WRONLY|O_CREAT, 0644 <unfinished ...>
103 write(4<pipe:[99]>, \"k\", 1) = 1
102 <... openat resumed>) = 10</scratch/cwd/relative.txt>
100 +++ exited with 0 +++
--- SIGCHLD {si_signo=SIGCHLD} ---
";

  /// Do: split a trace where the daemon runs `fusermount3`, which runs `/bin/mount`, each writing, beside
  /// the daemon's own write outside every class, a `mount` that failed to exec, and a child's write before
  /// its `execve`. Expect: only the two calls made under a broker image are set aside; the daemon's write,
  /// the failed exec's process and the pre-exec write stay judged (a slates process is never set aside).
  #[test]
  fn only_calls_under_an_os_mount_brokers_image_are_set_aside() {
    let log = "\
200 execve(\"/usr/bin/fusermount3\", [...], 0x1 /* 20 vars */) = 0
200 openat(AT_FDCWD</>, \"/etc/mtab\", O_WRONLY|O_CREAT, 0644) = 3</etc/mtab>
201 mkdirat(AT_FDCWD</>, \"/run/early\", 0755) = 0
201 execve(\"/bin/mount\", [...], 0x2 /* 0 vars */) = 0
201 mkdirat(AT_FDCWD</>, \"/run/mount\", 0755) = 0
100 mkdirat(AT_FDCWD</>, \"/home/runner/evil\", 0755) = 0
202 execve(\"/usr/bin/mount\", [...], 0x3 /* 0 vars */) = -1 ENOENT (No such file or directory)
202 mkdirat(AT_FDCWD</>, \"/home/runner/also\", 0755) = 0
";
    let events = parse_strace_with_cwd(&stamped(log), "/");
    let (judged, set_aside) = strace_split_broker_events(&stamped(log), events);
    let paths =
      |events: &[WriteEvent]| -> Vec<String> { events.iter().map(|e| e.path.clone()).collect() };
    assert_eq!(paths(&set_aside), vec!["/etc/mtab", "/run/mount"]);
    assert_eq!(
      paths(&judged),
      vec!["/run/early", "/home/runner/evil", "/home/runner/also"]
    );
  }

  /// The strace parser finds every write-capable call in order, joining the interrupted `openat`
  /// with its `resumed` continuation and skipping the read-only open, the exit and the signal lines.
  #[test]
  fn strace_events_are_found_in_order_with_interrupted_calls_joined() {
    let events = parse_strace_with_cwd(&stamped(STRACE), "/scratch/cwd");
    let calls: Vec<&str> = events.iter().map(|e| e.call.as_str()).collect();
    assert_eq!(
      calls,
      vec![
        "openat",
        "write",
        "fdatasync",
        "renameat",
        "renameat",
        "write",
        "write",
        "write",
        "write",
        "openat",
        "mkdir",
        "write",
        "openat"
      ]
    );
    assert_eq!(
      events[12].path, "/scratch/cwd/relative.txt",
      "the joined call's path"
    );
  }

  /// The judgement places each event: inside the target during the granted landing (including the temp
  /// name and the rename's both names), RAM-only objects, a standard stream on the null device, and the
  /// four violations: a log file behind descriptor 1 (a regular file is a file whatever descriptor reaches
  /// it — AUD-29-42), a file under `/dev/shm` (a tmpfs is a filesystem, not RAM-only — A-50),
  /// `/etc/evil`, and the relative file under the working directory.
  #[test]
  fn strace_events_are_placed_in_the_closed_taxonomy() {
    let events = parse_strace_with_cwd(&stamped(STRACE), "/scratch/cwd");
    let judged = judge(&events, &policy());
    assert_eq!(judged.write_calls, 13);
    assert_eq!(judged.inside_target, 5);
    assert_eq!(judged.standard_streams, 1);
    assert_eq!(judged.ram_only, 3, "socket, memfd, pipe");
    assert_eq!(judged.outside, 4);
    assert_eq!(
      paths(&judged.violations),
      [
        "/home/runner/daemon.log",
        "/dev/shm/slates-con",
        "/etc/evil",
        "/scratch/cwd/relative.txt"
      ]
    );
    assert_eq!(
      judged.written_inside,
      vec![".slates-tmp-1".to_owned(), "a.txt".to_owned()]
    );
    assert_eq!(judged.unresolved, 0);
  }

  /// AC-9.4 / T-9.1: a landing target's writes are observable and matched, an unnamed file included.
  /// The kernel's unnamed inode is temporary only because an O_TMPFILE open returned it.
  #[test]
  fn an_unnamed_landing_file_is_attributed_to_its_target() {
    let log = "91 openat(4</work/target>, \".\", O_WRONLY|O_TMPFILE, 0600) = 5</work/target/#9>(deleted)\n\
      91 pwrite64(5</work/target/#9>(deleted), \"x\", 1, 0) = 1\n\
      91 linkat(AT_FDCWD, \"/proc/self/fd/5\", 4</work/target>, \"file\", AT_SYMLINK_FOLLOW) = 0\n";
    let policy = Policy {
      target: "/work/target",
      working_directory: "/scratch",
      landing: Some(landing()),
      mounts: &[],
    };
    let judged = judge(&parse_strace(&stamped(log)), &policy);
    assert_eq!(judged.inside_target, 3);
    assert_eq!(judged.written_inside, ["#9", "file"]);
    assert_eq!(strace_unnamed_paths(log), ["/work/target/#9"]);
    assert!(
      strace_unnamed_paths(
        "91 openat(4</target>, \"#9\", O_WRONLY, 0600) = 5</target/#9>(deleted)\n"
      )
      .is_empty()
    );
  }

  /// AC-9.4 / T-9.1: replay the deleted-descriptor form emitted by the Linux CI tracer.
  /// A deleted memfd remains RAM-only; deletion never exempts a disk file from its grant.
  #[test]
  fn deleted_descriptor_annotations_preserve_the_write_destination() {
    let events = parse_strace(&stamped(
      "77040 ftruncate(82</memfd:slates-segment>(deleted), 536576) = 0\n\
       77040 ftruncate(83</outside/file>(deleted), 0) = 0\n\
       77040 ftruncate(84</scratch/land-target/file>(deleted), 0) = 0\n\
       77040 ftruncate(85</unknown>unrecognized, 0) = 0\n",
    ));
    let judged = judge(&events, &policy());
    assert_eq!(judged.write_calls, 4);
    assert_eq!(judged.ram_only, 1);
    assert_eq!(judged.inside_target, 1);
    assert_eq!(judged.outside, 1);
    assert_eq!(judged.violations[0].event.path, "/outside/file");
    assert_eq!(
      judged.unresolved, 1,
      "unknown decorations still fail closed"
    );
  }

  /// A read-only open is not a write; an undecorated descriptor is unresolved, not trusted.
  #[test]
  fn reads_are_ignored_and_bare_descriptors_are_unresolved() {
    let events = parse_strace(
      "1 openat(AT_FDCWD, \"/etc/passwd\", O_RDONLY) = 3</etc/passwd>\n1 write(9, \"x\", 1) = 1\n",
    );
    assert_eq!(events.len(), 1);
    let judged = judge(&events, &policy());
    assert_eq!(judged.unresolved, 1);
    assert_eq!(judged.outside, 0);
  }

  /// Hostile input: garbage, unbalanced parentheses, a lone resumed line, and escapes never panic.
  #[test]
  fn hostile_strace_input_never_panics() {
    let garbage = "((((\n) = \n1 <... write resumed>) = 1\nwrite(\n[pid ] x\n1 write(1</a>, \"\\303\\251\\\"\", 3) = 3\n\"\n";
    let events = parse_strace(garbage);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].path, "/a");
    assert_eq!(
      quoted("\"a\\303\\251b\\\"c\\n\"").unwrap(),
      "a\u{c3}\u{a9}b\"c\n"
    );
  }

  /// Rows in the shape Apple's `fs_usage.c` prints (`format_print`, `print_open`): an `open` with
  /// write flags, an `openat` under a tracked directory descriptor, descriptor writes resolved
  /// through the table, a socket write, and a write on a descriptor the log never opened.
  #[test]
  fn fs_usage_rows_resolve_descriptors_through_the_table() {
    let log = "\
08:20:01.000001  open              F=5        (R_______________)  /scratch/land-target    0.000010   slates.123
08:20:01.000002  openat            F=6        (_WC_E__________X)  [5]/.slates-tmp-1        0.000020   slates.123
08:20:01.000003  write             F=6   B=0x5                    /scratch/land-target/.slates-tmp-1   0.000030   slates.123
08:20:01.000004  fsync             F=6                                                     0.000040 W slates.123
08:20:01.000005  renameat                                         [5]/.slates-tmp-1  [5]/a.txt   0.000050   slates.123
08:20:01.000006  accept            F=7                                                     0.000060   slates.123
08:20:01.000007  write             F=7   B=0x10                                             0.000070   slates.123
08:20:01.000008  write             F=9   B=0x10                                             0.000080   slates.123
08:20:01.000009  close             F=6                                                      0.000090   slates.123
08:20:01.000010  open                   [  2] (_WC_____________)  /etc/evil                0.000011   slates.123
08:20:01.000011  mkdir                                            /etc/evil2               0.000012   slates.123
not a row
";
    let events = parse_fs_usage(log);
    let judged = judge(&events, &policy());
    assert_eq!(
      judged.write_calls, 9,
      "openat, write, fsync, two rename names, socket write, unknown write, open /etc/evil, mkdir /etc/evil2: {events:?}"
    );
    assert_eq!(judged.ram_only, 1);
    assert_eq!(judged.unresolved, 1);
    // fs_usage names neither the process nor the date, so no write inside the target can be placed in a
    // granted landing: each is refused for that reason, its path resolved through the descriptor table.
    assert_eq!(judged.inside_target, 0);
    assert_eq!(judged.outside, 7, "{:?}", judged.violations);
    let target: Vec<&Violation> = judged
      .violations
      .iter()
      .filter(|v| v.reason == "inside the target by a process the tracer did not name")
      .collect();
    assert_eq!(target.len(), 5);
    assert!(
      target
        .iter()
        .any(|v| v.event.path == "/scratch/land-target/.slates-tmp-1")
    );
    assert!(
      target
        .iter()
        .any(|v| v.event.path == "/scratch/land-target/a.txt")
    );
  }

  /// AC-4.5/T-9.1: truncated or malformed tracer rows cannot crash the judgment, and a valid
  /// violation after them is still reported. In particular, `W` needs an elapsed-time token
  /// before it; counting it as a suffix must not move the row's end before its call name.
  #[test]
  fn malformed_fs_usage_wait_suffixes_do_not_hide_a_following_violation() {
    let malformed = [
      "08:20:01.000001 write W slates.123",
      "08:20:01.000001 W slates.123",
      "08:20:01.000001 write",
    ];
    let valid = "08:20:01.000002 mkdir /outside/proof 0.000010 W slates.123\n";
    for prefix in malformed {
      let log = format!("{prefix}\n{valid}");
      let judged = judge(&parse_fs_usage(&log), &policy());
      assert_eq!(
        judged.outside, 1,
        "the complete violation survives: {prefix}"
      );
      assert_eq!(judged.violations[0].event.path, "/outside/proof");
    }
  }

  /// AUD-29-42 acceptance. Do: take a trace of one granted landing (the daemon, pid 100, writing a file
  /// inside the target during the landing's window) and mutate it one way at a time: the write before the
  /// grant's landing began, after it ended, by another process, with no landing granted at all; a log
  /// written to a regular file through descriptor 2; and a file under `/dev/shm`. Expect: the unmutated
  /// trace is clean, and each mutation is a violation for its own reason.
  #[test]
  fn each_mutation_of_a_granted_trace_fails_for_its_own_reason() {
    let line = |pid: u32, stamp: &str, body: &str| format!("{pid} {stamp} {body}\n");
    let landing_write = |pid: u32, stamp: &str| {
      line(
        pid,
        stamp,
        "write(6</scratch/land-target/a.txt>, \"x\", 1) = 1",
      )
    };
    let judged = |log: &str, landing: Option<Landing<'static>>| {
      let policy = Policy {
        target: TARGET,
        working_directory: "/scratch/cwd",
        landing,
        mounts: &[],
      };
      judge(&parse_strace(log), &policy)
    };
    let reasons = |log: &str, landing: Option<Landing<'static>>| -> Vec<&'static str> {
      judged(log, landing)
        .violations
        .iter()
        .map(|v| v.reason)
        .collect()
    };
    let only_daemon = Landing {
      writers: &[100],
      from_ns: WINDOW.0,
      until_ns: WINDOW.1,
    };

    let clean = landing_write(100, STAMP);
    let verdict = judged(&clean, Some(only_daemon.clone()));
    assert_eq!((verdict.inside_target, verdict.outside), (1, 0));

    let interval = "inside the target outside the granted landing's interval";
    assert_eq!(
      reasons(&landing_write(100, "0.999999"), Some(only_daemon.clone())),
      [interval],
      "a write before the grant's landing began"
    );
    assert_eq!(
      reasons(&landing_write(100, "2.000001"), Some(only_daemon.clone())),
      [interval],
      "a write after the landing ended (its grant consumed or revoked)"
    );
    assert_eq!(
      reasons(&landing_write(200, STAMP), Some(only_daemon.clone())),
      ["inside the target by a process that is not the landing's"],
      "a write by another process"
    );
    assert_eq!(
      reasons(&landing_write(100, STAMP), None),
      ["inside the target with no granted landing"],
      "a lifecycle that granted nothing"
    );
    assert_eq!(
      reasons(
        &line(100, STAMP, "write(2</scratch/anchor.log>, \"log\", 3) = 3"),
        Some(only_daemon.clone())
      ),
      ["outside every allowed class"],
      "a log file behind standard error"
    );
    assert_eq!(
      reasons(
        &line(
          100,
          STAMP,
          "openat(AT_FDCWD, \"/dev/shm/other\", O_RDWR|O_CREAT, 0600) = 9</dev/shm/other>"
        ),
        Some(only_daemon.clone())
      ),
      ["outside every allowed class"],
      "an unproved shared-memory path"
    );
    let unstamped = "100 write(6</scratch/land-target/a.txt>, \"x\", 1) = 1\n";
    assert_eq!(
      reasons(unstamped, Some(only_daemon)),
      ["inside the target at a time the tracer did not stamp"]
    );
  }

  /// AUD-29-42 acceptance (hidden siblings). Do: judge the hidden names a landing wrote — its own counter
  /// and aside forms, then another landing's name, a malformed one, an arbitrary `.slates-` name, and a
  /// clean run that left one behind. Expect: only the landing's own forms pass, and nothing may remain.
  #[test]
  fn only_the_landings_own_hidden_names_pass_and_none_may_remain() {
    let id = 0x0123_4567_89ab_cdef_u64;
    let own = format!(".slates-{id:016x}-0");
    let aside = format!(".slates-{id:016x}-aside-{:016x}", 7u64);
    let written = vec![own.clone(), format!("d/{aside}"), "a.txt".to_owned()];
    assert_eq!(judge_hidden(&written, &["a.txt".to_owned()], id), Ok(2));
    for foreign in [
      format!(".slates-{:016x}-0", id + 1),
      format!(".slates-{id:016x}-aside-7"),
      format!(".slates-{id:016x}-"),
      ".slates-anything".to_owned(),
      ".slates-kept-x".to_owned(),
    ] {
      assert!(
        judge_hidden(std::slice::from_ref(&foreign), &[], id).is_err(),
        "{foreign} is not the landing's"
      );
    }
    assert!(
      judge_hidden(std::slice::from_ref(&own), std::slice::from_ref(&own), id)
        .is_err_and(|why| why.contains("left behind")),
      "a hidden sibling must not remain"
    );
  }

  /// Golden vectors for the clocks the judge reads: strace `-ttt` and eslogger's RFC 3339 `time`.
  #[test]
  fn tracer_times_parse_to_epoch_nanoseconds() {
    assert_eq!(epoch_stamp_ns("1.000001"), Some(1_000_001_000));
    assert_eq!(
      epoch_stamp_ns("1696159123.123456"),
      Some(1_696_159_123_123_456_000)
    );
    for bad in ["", "1", ".5", "1.", "x.1", "1.1234567890", "-1.0"] {
      assert_eq!(epoch_stamp_ns(bad), None, "{bad}");
    }
    assert_eq!(iso_ns("1970-01-01T00:00:01.000001Z"), Some(1_000_001_000));
    assert_eq!(
      iso_ns("2000-03-01T00:00:00Z"),
      Some(951_868_800_000_000_000)
    );
    assert_eq!(
      iso_ns("2026-09-26T08:20:01.123456789Z"),
      Some(1_790_410_801_123_456_789)
    );
    for bad in ["2026-09-26T08:20:01", "2026-13-01T00:00:00Z", "garbage"] {
      assert_eq!(iso_ns(bad), None, "{bad}");
    }
  }

  /// The containment rule: the target itself and its descendants are inside; a sibling with the
  /// target as a prefix of its name is not; an empty target matches nothing.
  #[test]
  fn containment_is_by_path_component() {
    assert!(inside("/t/a", "/t"));
    assert!(inside("/t", "/t/"));
    assert!(!inside("/target/a", "/t"));
    assert!(!inside("/t/a", ""));
  }
}

#[cfg(test)]
mod mount_class_tests {
  use super::*;

  /// AUD-29-41/42 (golden). Do: parse a `mountinfo` excerpt — a disk root, procfs, a cgroup2 tree, a sysfs,
  /// a disk mounted beneath `/proc` (escaped space in its point), and a malformed line. Expect: each mount's
  /// point and type, the escape decoded, the malformed line skipped.
  #[test]
  fn mountinfo_yields_each_mount_point_and_its_type() {
    let text = "\
22 1 8:1 / / rw,relatime shared:1 - ext4 /dev/sda1 rw
23 22 0:21 / /proc rw,nosuid shared:12 - proc proc rw
24 22 0:22 / /sys rw,nosuid shared:2 - sysfs sysfs rw
25 24 0:23 / /sys/fs/cgroup rw shared:4 - cgroup2 cgroup2 rw
26 23 8:2 / /proc/odd\\040disk rw shared:9 - ext4 /dev/sdb1 rw
garbage without a separator
";
    let mounts = parse_mountinfo(text);
    let pairs: Vec<(&str, &str)> = mounts
      .iter()
      .map(|(p, t)| (p.as_str(), t.as_str()))
      .collect();
    assert_eq!(
      pairs,
      [
        ("/", "ext4"),
        ("/proc", "proc"),
        ("/sys", "sysfs"),
        ("/sys/fs/cgroup", "cgroup2"),
        ("/proc/odd disk", "ext4"),
      ]
    );
  }

  fn write_to(path: &str) -> WriteEvent {
    WriteEvent {
      line: 1,
      call: "write".to_owned(),
      path: path.to_owned(),
      descriptor: Some(3),
      pid: Some(7),
      at_ns: Some(1),
    }
  }

  /// AUD-29-41/42. Do: judge writes to a process's core filter, a cgroup limit, a file on a disk mounted
  /// beneath `/proc`, a disk path spelled to look like procfs (`/procfoo`), and a disk file, against that
  /// mount table — and the core filter again with no mount table. Expect: the procfs and cgroup writes are
  /// kernel control files (their longest-prefix mount is a kernel virtual filesystem); the nested disk, the
  /// look-alike and the disk file are violations; with no mount table nothing is assumed, and the core
  /// filter is a violation — the class comes from what is mounted, never from the spelling.
  #[test]
  fn a_kernel_control_file_is_known_by_its_mount_never_its_spelling() {
    let table = [
      Mount {
        point: "/",
        fstype: "ext4",
      },
      Mount {
        point: "/proc",
        fstype: "proc",
      },
      Mount {
        point: "/sys/fs/cgroup",
        fstype: "cgroup2",
      },
      Mount {
        point: "/proc/odd disk",
        fstype: "ext4",
      },
    ];
    let with = |mounts| Policy {
      target: "/work/target",
      working_directory: "/work",
      landing: None,
      mounts,
    };
    let control =
      Placement::RamOnly("a kernel control file (its mount is a kernel virtual filesystem)");
    assert_eq!(
      classify(&write_to("/proc/42/coredump_filter"), &with(&table)),
      control
    );
    assert_eq!(
      classify(&write_to("/sys/fs/cgroup/slates/memory.max"), &with(&table)),
      control
    );
    for disk in [
      "/proc/odd disk/f",
      "/procfoo/coredump_filter",
      "/home/u/file",
    ] {
      assert_eq!(
        classify(&write_to(disk), &with(&table)),
        Placement::Outside,
        "{disk}"
      );
    }
    assert_eq!(
      classify(&write_to("/proc/42/coredump_filter"), &with(&[])),
      Placement::Outside
    );
  }
}
