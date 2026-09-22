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
//! and shown, rather than trusted either way.

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
}

/// Where a write landed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Placement {
  /// Inside the granted landing target.
  InsideTarget,
  /// A RAM-only kernel object; the reason names the class.
  RamOnly(&'static str),
  /// The process's own standard output or error.
  StandardStream,
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
  /// The first violations.
  pub violations: Vec<WriteEvent>,
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
const CHARACTER_DEVICES: &[&str] = &["/dev/null", "/dev/zero", "/dev/tty", "/dev/ptmx"];
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

/// Places one event.
pub fn classify(event: &WriteEvent, policy: &Policy<'_>) -> Placement {
  if matches!(event.descriptor, Some(1 | 2)) && is_stream_write(&event.call) {
    return Placement::StandardStream;
  }
  let path = event.path.as_str();
  if inside(path, policy.target) {
    return Placement::InsideTarget;
  }
  if KERNEL_OBJECT_PREFIXES.iter().any(|p| path.starts_with(p)) {
    return Placement::RamOnly("kernel object (socket, pipe, anon inode, memfd, shm)");
  }
  if path.starts_with("/dev/shm/") {
    return Placement::RamOnly("tmpfs shared-memory object under /dev/shm");
  }
  if path == "/dev/fuse" {
    return Placement::RamOnly("the FUSE device");
  }
  if CHARACTER_DEVICES.contains(&path)
    || path.starts_with("/dev/pts/")
    || path.starts_with("/dev/ttys")
  {
    return Placement::RamOnly("a character device");
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
        keep_sample(&mut out.violations, event);
      }
    }
  }
  out.written_inside.sort();
  out.written_inside.dedup();
  out
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

/// Joins `<unfinished ...>` / `resumed>` pairs per pid; yields whole call lines with their line numbers.
fn joined_lines(log: &str) -> Vec<(usize, String)> {
  let mut pending: HashMap<String, String> = HashMap::new();
  let mut out = Vec::new();
  for (index, raw) in log.lines().enumerate() {
    let (pid, body) = split_pid(raw);
    if let Some(head) = body.strip_suffix(UNFINISHED) {
      pending.insert(pid, head.to_owned());
      continue;
    }
    if body.starts_with("<...") {
      if let Some(position) = body.find(RESUMED) {
        let tail = &body[position + RESUMED.len()..];
        if let Some(head) = pending.remove(&pid) {
          out.push((index + 1, format!("{head}{tail}")));
        }
      }
      continue;
    }
    out.push((index + 1, body.to_owned()));
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
    .filter_map(|(_, body)| {
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
  let mut events = Vec::new();
  for (line, body) in joined_lines(log) {
    if let Some((name, args)) = call_and_args(&body) {
      events.extend(strace_events(line, name, &args, "."));
    }
  }
  events
}

/// Parses an strace log with the traced process's working directory for `AT_FDCWD` paths.
pub fn parse_strace_with_cwd(log: &str, cwd: &str) -> Vec<WriteEvent> {
  let mut events = Vec::new();
  for (line, body) in joined_lines(log) {
    if let Some((name, args)) = call_and_args(&body) {
      events.extend(strace_events(line, name, &args, cwd));
    }
  }
  events
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
  for token in tokens.get(2..body_end)? {
    read_token(token, &mut row);
  }
  Some(row)
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

#[cfg(test)]
mod tests {
  use super::*;

  const TARGET: &str = "/scratch/land-target";

  fn policy() -> Policy<'static> {
    Policy {
      target: TARGET,
      working_directory: "/scratch/cwd",
    }
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

  /// The strace parser finds every write-capable call in order, joining the interrupted `openat`
  /// with its `resumed` continuation and skipping the read-only open, the exit and the signal lines.
  #[test]
  fn strace_events_are_found_in_order_with_interrupted_calls_joined() {
    let events = parse_strace_with_cwd(STRACE, "/scratch/cwd");
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

  /// The judgement places each event: inside the target (including the temp name and the rename's
  /// both names), RAM-only objects, the standard streams, and the two violations (`/etc/evil`, the
  /// relative file under the working directory).
  #[test]
  fn strace_events_are_placed_in_the_closed_taxonomy() {
    let events = parse_strace_with_cwd(STRACE, "/scratch/cwd");
    let judged = judge(&events, &policy());
    assert_eq!(judged.write_calls, 13);
    assert_eq!(judged.inside_target, 5);
    assert_eq!(judged.standard_streams, 2);
    assert_eq!(judged.ram_only, 4, "socket, memfd, /dev/shm, pipe");
    assert_eq!(judged.outside, 2);
    assert_eq!(judged.violations[0].path, "/etc/evil");
    assert_eq!(judged.violations[1].path, "/scratch/cwd/relative.txt");
    assert_eq!(
      judged.written_inside,
      vec![".slates-tmp-1".to_owned(), "a.txt".to_owned()]
    );
    assert_eq!(judged.unresolved, 0);
  }

  /// AC-9.4 / T-9.1: RAM-backed landing targets still need observable, matched writes.
  /// The kernel's unnamed inode is temporary only because an O_TMPFILE open returned it.
  #[test]
  fn an_unnamed_landing_file_is_attributed_to_its_ram_backed_target() {
    let log = "91 openat(4</dev/shm/target>, \".\", O_WRONLY|O_TMPFILE, 0600) = 5</dev/shm/target/#9>(deleted)\n\
      91 pwrite64(5</dev/shm/target/#9>(deleted), \"x\", 1, 0) = 1\n\
      91 linkat(AT_FDCWD, \"/proc/self/fd/5\", 4</dev/shm/target>, \"file\", AT_SYMLINK_FOLLOW) = 0\n";
    let policy = Policy {
      target: "/dev/shm/target",
      working_directory: "/scratch",
    };
    let judged = judge(&parse_strace(log), &policy);
    assert_eq!(judged.inside_target, 3);
    assert_eq!(judged.written_inside, ["#9", "file"]);
    assert_eq!(strace_unnamed_paths(log), ["/dev/shm/target/#9"]);
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
    let events = parse_strace(
      "77040 ftruncate(82</memfd:slates-segment>(deleted), 536576) = 0\n\
       77040 ftruncate(83</outside/file>(deleted), 0) = 0\n\
       77040 ftruncate(84</scratch/land-target/file>(deleted), 0) = 0\n\
       77040 ftruncate(85</unknown>unrecognized, 0) = 0\n",
    );
    let judged = judge(&events, &policy());
    assert_eq!(judged.write_calls, 4);
    assert_eq!(judged.ram_only, 1);
    assert_eq!(judged.inside_target, 1);
    assert_eq!(judged.outside, 1);
    assert_eq!(judged.violations[0].path, "/outside/file");
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
    assert_eq!(judged.inside_target, 5);
    assert_eq!(judged.ram_only, 1);
    assert_eq!(judged.unresolved, 1);
    assert_eq!(judged.outside, 2, "{:?}", judged.violations);
    assert_eq!(
      judged.written_inside,
      vec![".slates-tmp-1".to_owned(), "a.txt".to_owned()]
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
      assert_eq!(judged.violations[0].path, "/outside/proof");
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
