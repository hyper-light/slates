//! pjdfstest's TAP output, parsed per test file (Part 6 "Conformance"). Each `tests/**/*.t` is a
//! shell script that prints a plan (`1..N`) and one line per case: `ok N`, `ok N # TODO msg`,
//! `not ok N - tried '<pjdfstest args>', expected <e>, got <r>`, `not ok N # TODO msg`, or the
//! `requires_root` guard's `not ok N not root` (`tests/misc.sh` at the pinned commit). The parser
//! runs one file at a time so every case is attributed to its file without `prove`, keeps each
//! detail bounded, never panics on any line, and counts the lines it could not read instead of
//! guessing at them.
//!
//! Privilege is part of the classification: pjdfstest's README requires root, and a case that only
//! root could pass fails as an ordinary user for that reason alone, so an unprivileged run counts
//! such a failure as needs-root, not as a failure of the transport. A root run classifies nothing
//! that way. The rules are decided from the case's own line and the caller's identity, never from
//! what came before it in the file:
//!
//! - the command switches uid or gid (`-u`/`-g`), which needs root to `setuid`;
//! - the command makes a block or character node (`mknod(2)` refuses those to an ordinary user on
//!   every filesystem; a fifo through `mknod` is allowed, measured on this host's APFS 2026-09-15);
//! - the command changes an owner to a uid that is not the caller's, or a group the caller is not
//!   in (POSIX.1-2024 `chown()`: with `_POSIX_CHOWN_RESTRICTED` in effect, as it is on every macOS
//!   and Linux filesystem, only a process with appropriate privileges may change a file's owner,
//!   and an owner may change the group only to one of its own);
//! - the expectation asserts an owner that only such a change could have produced (`expect
//!   65534,65534 lstat f uid,gid`), which can hold only after the root-only change above.
//!
//! What stays a failure in an unprivileged run is what would fail as root too, plus the
//! consequences of the rules above that a later case's own line does not show (a rename that never
//! happened leaves the source in place); those are counted and shaped by cause, never hidden.

use crate::record::{Privilege, detail};

/// A case's identity: the test file and the case number within it.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct TestId {
  /// The test file, relative to the suite root (`tests/chmod/00.t`).
  pub file: String,
  /// The 1-based case number.
  pub number: u32,
}

impl std::fmt::Display for TestId {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    write!(f, "{}:{}", self.file, self.number)
  }
}

/// What a case reported.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CaseStatus {
  /// `ok N`.
  Pass,
  /// `not ok N ...` for a reason other than privilege.
  Fail {
    /// The suite's own message, bounded.
    detail: String,
  },
  /// `not ok N` because the run was not root (the guard, or a uid/gid switch).
  NeedsRoot {
    /// The suite's own message, bounded.
    detail: String,
  },
  /// `not ok N # TODO msg`: the suite itself expects the failure.
  TodoFail {
    /// The TODO message, bounded.
    detail: String,
  },
  /// `ok N # TODO msg`: the suite expected a failure and got a pass.
  TodoPass,
}

/// One case.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TapCase {
  /// The identity.
  pub id: TestId,
  /// The status.
  pub status: CaseStatus,
}

/// One test file's parsed output.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TapFile {
  /// The file.
  pub file: String,
  /// The plan's count, when a plan line was seen.
  pub plan: Option<u32>,
  /// The cases, in output order.
  pub cases: Vec<TapCase>,
  /// Lines that were neither a plan, a case, a diagnostic nor blank.
  pub malformed_lines: u32,
}

impl TapFile {
  /// Whether the output is whole: a plan was seen and every planned case was reported.
  pub fn complete(&self) -> bool {
    self
      .plan
      .is_some_and(|plan| usize::try_from(plan).is_ok_and(|p| p == self.cases.len()))
  }
}

/// Who ran the suite: the privilege the record carries and, for an ordinary user, the identity that
/// decides which cases root alone could pass.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Runner {
  /// Root: only the `requires_root` guard's own line is needs-root.
  Root,
  /// An ordinary user with this uid and these groups (the effective group and the supplementary
  /// ones, as `getgroups(2)` lists them and as the `AUTH_SYS` credential carries them).
  Unprivileged {
    /// The effective uid.
    uid: u32,
    /// Every group the caller is in.
    groups: Vec<u32>,
  },
}

impl Runner {
  /// The privilege the record carries.
  pub fn privilege(&self) -> Privilege {
    match self {
      Runner::Root => Privilege::Root,
      Runner::Unprivileged { .. } => Privilege::Unprivileged,
    }
  }
}

/// Format: the TODO directive TAP and pjdfstest's `misc.sh` use.
const TODO: &str = "# TODO";
/// Format: the `requires_root` guard's message (`misc.sh`).
const NOT_ROOT: &str = "not root";
/// Format: `misc.sh`'s failure message opens with the command it ran, quoted.
const TRIED: &str = "tried '";
/// Format: the quoted command closes and the expectation follows.
const EXPECTED: &str = "', expected ";
/// Format: the expectation closes and the result follows (the result may be empty, and the line
/// is trimmed before it is read, so the separator is matched without its trailing space).
const GOT: &str = ", got";
/// Format: pjdfstest runs several calls in one command when they are separated by ` : `; the
/// message compares the last call's output (`misc.sh` takes `tail -1`).
const CALL_SEPARATOR: &str = " : ";
/// Format: the prefix `misc.sh`'s `namegen` gives every name it makes (an md5 follows).
const GENERATED_NAME: &str = "pjdfstest_";
/// Format: `misc.sh`'s `test_check` compares two values the script read (a ctime before and after
/// an operation, mostly) and prints a bare `not ok N`; the shape names it, since the line cannot.
const TEST_CHECK_SHAPE: &str =
  "test_check (a comparison of two values the script read; no message)";
/// Format: a decimal this long in an expectation or a result is an inode number (pjdfstest prints
/// `st_ino`; the transports' numbers have at least this many digits, a uid or a mode never does).
const INODE_DIGITS: usize = 9;

/// The parts of `tried '<command>', expected <e>, got <r>`.
struct Tried<'a> {
  command: &'a str,
  expected: &'a str,
  got: &'a str,
}

fn tried(message: &str) -> Option<Tried<'_>> {
  let rest = message.strip_prefix(TRIED)?;
  let (command, rest) = rest.split_once(EXPECTED)?;
  let (expected, got) = rest.split_once(GOT)?;
  Some(Tried {
    command,
    expected,
    got: got.trim(),
  })
}

/// One call's tokens with the `-u N` and `-g N` switches removed.
fn call_tokens(call: &str) -> Vec<&str> {
  let mut tokens = Vec::new();
  let mut skip_value = false;
  for token in call.split_whitespace() {
    if skip_value {
      skip_value = false;
    } else if matches!(token, "-u" | "-g") {
      skip_value = true;
    } else {
      tokens.push(token);
    }
  }
  tokens
}

/// Whether a failure's message shows a uid or gid switch: a `-u` or `-g` token in the tried
/// command (`tried '-u 65534 -g 65534 chmod …'`; the first token carries the opening quote).
fn switches_identity(message: &str) -> bool {
  message
    .split_whitespace()
    .any(|token| matches!(token.trim_start_matches('\''), "-u" | "-g"))
}

/// Whether a uid token names a user other than the caller (`-1`, "leave it", never does).
fn foreign_uid(token: Option<&str>, uid: u32) -> bool {
  token
    .and_then(|t| t.parse::<u32>().ok())
    .is_some_and(|named| named != uid)
}

/// Whether a gid token names a group the caller is not in.
fn foreign_gid(token: Option<&str>, groups: &[u32]) -> bool {
  token
    .and_then(|t| t.parse::<u32>().ok())
    .is_some_and(|named| !groups.contains(&named))
}

/// Format: pjdfstest's syscall table — `chown PATH UID GID`, `fchown FD UID GID` and `lchown PATH
/// UID GID` carry the uid as the second argument.
const CHOWN_UID_AT: usize = 2;
/// Format: … and the gid as the third.
const CHOWN_GID_AT: usize = 3;
/// Format: `fchownat FD PATH UID GID FLAGS` carries the uid as the third argument.
const FCHOWNAT_UID_AT: usize = 3;
/// Format: … and the gid as the fourth.
const FCHOWNAT_GID_AT: usize = 4;
/// Format: `stat PATH FIELDS`, `lstat PATH FIELDS` and `fstat FD FIELDS` carry the fields as the
/// second argument.
const STAT_FIELDS_AT: usize = 2;
/// Format: `fstatat FD PATH FIELDS FLAGS` carries the fields as the third.
const FSTATAT_FIELDS_AT: usize = 3;

/// Whether a command changes an owner to one only root may set (the chown family, by the
/// argument positions above).
fn changes_owner_to_foreign(command: &str, uid: u32, groups: &[u32]) -> bool {
  command.split(CALL_SEPARATOR).any(|call| {
    let tokens = call_tokens(call);
    let (uid_at, gid_at) = match tokens.first().copied() {
      Some("chown" | "fchown" | "lchown") => (CHOWN_UID_AT, CHOWN_GID_AT),
      Some("fchownat") => (FCHOWNAT_UID_AT, FCHOWNAT_GID_AT),
      _ => return false,
    };
    foreign_uid(tokens.get(uid_at).copied(), uid)
      || foreign_gid(tokens.get(gid_at).copied(), groups)
  })
}

/// Whether the expectation asserts an owner only root could have set: the last call is of the
/// stat family, and a `uid` field expects another user or a `gid` field a group the caller is
/// not in.
fn expects_foreign_owner(command: &str, expected: &str, uid: u32, groups: &[u32]) -> bool {
  let last = command.rsplit(CALL_SEPARATOR).next().unwrap_or(command);
  let tokens = call_tokens(last);
  let fields = match tokens.first().copied() {
    Some("stat" | "lstat" | "fstat") => tokens.get(STAT_FIELDS_AT),
    Some("fstatat") => tokens.get(FSTATAT_FIELDS_AT),
    _ => None,
  };
  fields.is_some_and(|fields| {
    fields
      .split(',')
      .zip(expected.split(','))
      .any(|(field, value)| match field {
        "uid" => foreign_uid(Some(value), uid),
        "gid" => foreign_gid(Some(value), groups),
        _ => false,
      })
  })
}

/// Whether an ordinary user could never have passed the case, by its own line.
fn root_alone_could_pass(message: &str, uid: u32, groups: &[u32]) -> bool {
  if switches_identity(message) || makes_device_node(message) {
    return true;
  }
  tried(message).is_some_and(|t| {
    changes_owner_to_foreign(t.command, uid, groups)
      || expects_foreign_owner(t.command, t.expected, uid, groups)
  })
}

/// A name `namegen` made, or a path of them, folded to `N` (`N/N/test`); anything else kept.
fn fold_generated_names(token: &str) -> String {
  token
    .split('/')
    .map(|part| {
      if part.starts_with(GENERATED_NAME) {
        "N"
      } else {
        part
      }
    })
    .collect::<Vec<_>>()
    .join("/")
}

/// An expectation or result with inode numbers folded to `<inode>` (`ENOENT,65534,65534` stays).
fn fold_inodes(values: &str) -> String {
  values
    .split(',')
    .map(|value| {
      if value.len() >= INODE_DIGITS && value.bytes().all(|b| b.is_ascii_digit()) {
        "<inode>"
      } else {
        value
      }
    })
    .collect::<Vec<_>>()
    .join(",")
}

/// A failure's shape: its message with the generated names and the inode numbers folded away, so
/// the failures with one cause count together (`chown N 65534 65534, expected 0, got EPERM`). A
/// message in another form (a guard's line, a hostile one) is its own shape.
pub fn shape(message: &str) -> String {
  if message.is_empty() {
    return TEST_CHECK_SHAPE.to_owned();
  }
  let Some(t) = tried(message) else {
    return message.to_owned();
  };
  let command = t
    .command
    .split_whitespace()
    .map(fold_generated_names)
    .collect::<Vec<_>>()
    .join(" ");
  // An empty result (a switch that could not run printed nothing) leaves `got` bare.
  format!(
    "{command}, expected {}, got {}",
    fold_inodes(t.expected),
    fold_inodes(t.got)
  )
  .trim_end()
  .to_owned()
}

/// The failures (not the needs-root or TODO cases) counted by shape, most frequent first and by
/// shape within a count, at most `limit` rows; the second value is how many shapes there were.
pub fn shapes(cases: &[TapCase], limit: usize) -> (Vec<(String, u32)>, usize) {
  let mut counts: std::collections::BTreeMap<String, u32> = std::collections::BTreeMap::new();
  for case in cases {
    if let CaseStatus::Fail { detail } = &case.status {
      *counts.entry(shape(detail)).or_default() += 1;
    }
  }
  let total = counts.len();
  let mut rows: Vec<(String, u32)> = counts.into_iter().collect();
  rows.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
  rows.truncate(limit);
  (rows, total)
}

/// The failures counted by file, most first and by file within a count, at most `limit` rows.
pub fn failures_by_file(cases: &[TapCase], limit: usize) -> Vec<(String, u32)> {
  let mut counts: std::collections::BTreeMap<&str, u32> = std::collections::BTreeMap::new();
  for case in cases {
    if matches!(case.status, CaseStatus::Fail { .. }) {
      *counts.entry(case.id.file.as_str()).or_default() += 1;
    }
  }
  let mut rows: Vec<(String, u32)> = counts
    .into_iter()
    .map(|(file, count)| (file.to_owned(), count))
    .collect();
  rows.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
  rows.truncate(limit);
  rows
}

/// The case number and the rest of a line after `ok`/`not ok`.
fn number_and_rest(after_ok: &str) -> Option<(u32, &str)> {
  let trimmed = after_ok.trim_start();
  let end = trimmed
    .find(|c: char| !c.is_ascii_digit())
    .unwrap_or(trimmed.len());
  let number: u32 = trimmed[..end].parse().ok()?;
  Some((number, trimmed[end..].trim()))
}

/// Whether a failure's message is a device-node `mknod` (`tried 'mknod NAME b …'` / `c …`): the
/// kernel refuses block and character nodes to an ordinary user on every filesystem (`mknod(2)`),
/// so an unprivileged run cannot judge the transport by them.
fn makes_device_node(message: &str) -> bool {
  /// Format: pjdfstest's `mknod NAME TYPE MODE [MAJOR MINOR]` — the type is the third token.
  const MKNOD_TOKENS: usize = 3;
  let tokens: Vec<&str> = message.split_whitespace().collect();
  tokens
    .windows(MKNOD_TOKENS)
    .any(|w| w[0].trim_start_matches('\'') == "mknod" && matches!(w[2], "b" | "c"))
}

/// The status of a `not ok` line.
fn failure_status(rest: &str, runner: &Runner) -> CaseStatus {
  if let Some(todo) = rest.split_once(TODO) {
    return CaseStatus::TodoFail {
      detail: detail(todo.1.trim()),
    };
  }
  let message = rest.trim_start_matches('-').trim();
  let needs_root = message.starts_with(NOT_ROOT)
    || match runner {
      Runner::Root => false,
      Runner::Unprivileged { uid, groups } => root_alone_could_pass(message, *uid, groups),
    };
  if needs_root {
    CaseStatus::NeedsRoot {
      detail: detail(message),
    }
  } else {
    CaseStatus::Fail {
      detail: detail(message),
    }
  }
}

/// One line's contribution.
enum Line {
  Plan(u32),
  Case(u32, CaseStatus),
  Skip,
  Malformed,
}

fn read_line(raw: &str, runner: &Runner) -> Line {
  let line = raw.trim();
  if line.is_empty() || line.starts_with('#') {
    return Line::Skip;
  }
  if let Some(plan) = line.strip_prefix("1..") {
    return plan.trim().parse().map_or(Line::Malformed, Line::Plan);
  }
  if let Some(rest) = line.strip_prefix("not ok") {
    return match number_and_rest(rest) {
      Some((number, rest)) => Line::Case(number, failure_status(rest, runner)),
      None => Line::Malformed,
    };
  }
  if let Some(rest) = line.strip_prefix("ok") {
    return match number_and_rest(rest) {
      Some((number, rest)) if rest.contains(TODO) => Line::Case(number, CaseStatus::TodoPass),
      Some((number, _)) => Line::Case(number, CaseStatus::Pass),
      None => Line::Malformed,
    };
  }
  Line::Malformed
}

/// Parses one test file's output.
pub fn parse_file(file: &str, output: &str, runner: &Runner) -> TapFile {
  let mut parsed = TapFile {
    file: file.to_owned(),
    plan: None,
    cases: Vec::new(),
    malformed_lines: 0,
  };
  for raw in output.lines() {
    match read_line(raw, runner) {
      Line::Plan(count) => parsed.plan = Some(count),
      Line::Case(number, status) => parsed.cases.push(TapCase {
        id: TestId {
          file: file.to_owned(),
          number,
        },
        status,
      }),
      Line::Skip => {}
      Line::Malformed => parsed.malformed_lines += 1,
    }
  }
  parsed
}

#[cfg(test)]
mod tests {
  use super::*;

  const OUTPUT: &str = "1..6\nok 1\nnot ok 2 - tried 'chmod x 0644', expected 0, got EPERM\n\
    not ok 3 - tried '-u 65534 -g 65534 chmod x 0644', expected 0, got EPERM\n\
    not ok 4 # TODO Linux does not clear SGID\nok 5 # TODO passes now\nnot ok 6 not root\n";

  /// The detail of a failing status, or the status's name for a passing one.
  fn detail_of(status: &CaseStatus) -> String {
    match status {
      CaseStatus::Pass => "pass".to_owned(),
      CaseStatus::TodoPass => "todo-pass".to_owned(),
      CaseStatus::Fail { detail } => format!("fail:{detail}"),
      CaseStatus::NeedsRoot { detail } => format!("needs-root:{detail}"),
      CaseStatus::TodoFail { detail } => format!("todo-fail:{detail}"),
    }
  }

  /// The ordinary user the unprivileged tests run as: uid 501 in groups 20 and 12.
  fn user() -> Runner {
    Runner::Unprivileged {
      uid: 501,
      groups: vec![20, 12],
    }
  }

  /// The pass, fail and needs-root forms of `misc.sh` classify for an unprivileged run, the plan
  /// is met, and a case is identified by its file and number.
  #[test]
  fn pass_fail_and_uid_switch_classify_unprivileged() {
    let parsed = parse_file("tests/chmod/00.t", OUTPUT, &user());
    assert_eq!(parsed.plan, Some(6));
    assert!(parsed.complete());
    assert_eq!(parsed.malformed_lines, 0);
    let details: Vec<String> = parsed.cases.iter().map(|c| detail_of(&c.status)).collect();
    assert_eq!(details[0], "pass");
    assert_eq!(
      details[1],
      "fail:tried 'chmod x 0644', expected 0, got EPERM"
    );
    assert!(
      details[2].starts_with("needs-root:tried '-u 65534"),
      "{}",
      details[2]
    );
    assert_eq!(parsed.cases[2].id.to_string(), "tests/chmod/00.t:3");
  }

  /// The TODO forms and the root guard classify.
  #[test]
  fn todo_and_root_guard_classify() {
    let parsed = parse_file("tests/chmod/00.t", OUTPUT, &user());
    let details: Vec<String> = parsed.cases.iter().map(|c| detail_of(&c.status)).collect();
    assert_eq!(details[3], "todo-fail:Linux does not clear SGID");
    assert_eq!(details[4], "todo-pass");
    assert_eq!(details[5], "needs-root:not root");
  }

  /// A device-node `mknod` that fails is needs-root for an ordinary user (the kernel's rule on
  /// every filesystem) and a failure as root; a fifo `mknod` is a failure either way.
  #[test]
  fn device_nodes_need_root_but_fifos_do_not() {
    let output = "1..2\nnot ok 1 - tried 'mknod x b 0644 1 2', expected 0, got EPERM\n\
      not ok 2 - tried 'mknod x f 0644', expected 0, got EIO\n";
    let unprivileged = parse_file("t", output, &user());
    assert!(matches!(
      unprivileged.cases[0].status,
      CaseStatus::NeedsRoot { .. }
    ));
    assert!(matches!(
      unprivileged.cases[1].status,
      CaseStatus::Fail { .. }
    ));
    let root = parse_file("t", output, &Runner::Root);
    assert!(matches!(root.cases[0].status, CaseStatus::Fail { .. }));
  }

  /// A uid switch is found as a token even when it opens the tried command (`'-u`).
  #[test]
  fn a_leading_uid_switch_is_found() {
    let output = "1..1\nnot ok 1 - tried '-u 65534 chmod x 0600', expected 0, got EPERM\n";
    let parsed = parse_file("t", output, &user());
    assert!(matches!(
      parsed.cases[0].status,
      CaseStatus::NeedsRoot { .. }
    ));
  }

  /// As root, a uid switch that fails is a failure of the transport, not of privilege.
  #[test]
  fn root_runs_classify_uid_switches_as_failures() {
    let parsed = parse_file("t", OUTPUT, &Runner::Root);
    assert!(matches!(parsed.cases[2].status, CaseStatus::Fail { .. }));
    assert!(
      matches!(parsed.cases[5].status, CaseStatus::NeedsRoot { .. }),
      "the guard's own line stays"
    );
  }

  /// Hostile input: garbage, a huge number, a missing plan and a truncated run never panic and
  /// are counted or reported as incomplete.
  #[test]
  fn hostile_output_is_counted_not_trusted() {
    let parsed = parse_file(
      "t",
      "garbage\nok 99999999999\nok\nok 1\n1..3\nok 2\n",
      &Runner::Root,
    );
    assert_eq!(
      parsed.malformed_lines, 3,
      "garbage, the overflow and the bare ok"
    );
    assert_eq!(parsed.cases.len(), 2);
    assert!(!parsed.complete(), "two of three planned");
    let none = parse_file("t", "", &Runner::Root);
    assert!(!none.complete());
    let long = format!("not ok 1 - {}\n", "x".repeat(10_000));
    let parsed = parse_file("t", &long, &Runner::Root);
    assert!(
      matches!(&parsed.cases[0].status, CaseStatus::Fail { detail } if detail.chars().count() <= crate::record::DETAIL_CHARS + 1)
    );
  }

  /// A `chown` to another user, or to a group the caller is not in, is root-only by POSIX
  /// (`_POSIX_CHOWN_RESTRICTED`), so an ordinary user's failure there is needs-root; a `chown` to
  /// the caller's own uid and one of its groups, or `-1` for "leave it", is judged.
  #[test]
  fn a_chown_to_a_foreign_owner_needs_root_but_ones_own_is_judged() {
    let output = "1..7\n\
      not ok 1 - tried 'chown x 65534 65534', expected 0, got EPERM\n\
      not ok 2 - tried 'lchown x 501 65533', expected 0, got EPERM\n\
      not ok 3 - tried 'chown x 501 12', expected 0, got EPERM\n\
      not ok 4 - tried 'chown x -1 20', expected 0, got EPERM\n\
      not ok 5 - tried 'fchownat 0 y 0 0 AT_SYMLINK_NOFOLLOW', expected 0, got EPERM\n\
      not ok 6 - tried 'open x O_RDONLY : fchown 0 123 456', expected 0, got EPERM\n\
      not ok 7 - tried 'chown x 123 456', expected 0, got EPERM\n";
    let parsed = parse_file("t", output, &user());
    let needs_root: Vec<bool> = parsed
      .cases
      .iter()
      .map(|c| matches!(c.status, CaseStatus::NeedsRoot { .. }))
      .collect();
    assert_eq!(
      needs_root,
      [true, true, false, false, true, true, true],
      "another uid; a foreign group; own uid and group; leave the uid; fchownat to root; a chained fchown; another owner"
    );
    let root = parse_file("t", output, &Runner::Root);
    assert!(
      root
        .cases
        .iter()
        .all(|c| matches!(c.status, CaseStatus::Fail { .. })),
      "as root every one is judged"
    );
  }

  /// An expectation that names an owner only root could have set (`expect 65534,65534 lstat f
  /// uid,gid`) can hold only after a root-only `chown`, so it is needs-root for an ordinary user;
  /// an expectation naming the caller's own owner, or an errno, is judged. The fields are read
  /// from the last call of a chained command and from `fstatat`'s third argument.
  #[test]
  fn an_expectation_of_a_foreign_owner_needs_root() {
    let output = "1..7\n\
      not ok 1 - tried 'lstat x uid,gid', expected 65534,65534, got 501,20\n\
      not ok 2 - tried 'lstat x inode,uid,gid', expected ENOENT,0,0, got 281474976711391,501,20\n\
      not ok 3 - tried 'lstat x type,mode,nlink,uid,gid', expected char,0201,2,65534,65533, got ENOENT\n\
      not ok 4 - tried 'lstat x uid,gid', expected 501,20, got ENOENT\n\
      not ok 5 - tried 'stat x mode', expected 0644, got ENOENT\n\
      not ok 6 - tried 'open x O_RDONLY : fstat 0 uid', expected 65534, got 501\n\
      not ok 7 - tried 'fstatat 0 x gid AT_SYMLINK_NOFOLLOW', expected 65533, got 20\n";
    let parsed = parse_file("t", output, &user());
    let needs_root: Vec<bool> = parsed
      .cases
      .iter()
      .map(|c| matches!(c.status, CaseStatus::NeedsRoot { .. }))
      .collect();
    assert_eq!(needs_root, [true, true, true, false, false, true, true]);
  }

  /// A shape folds the generated names and the inode numbers and keeps everything else, so one
  /// cause counts as one row; the rows come most frequent first.
  #[test]
  fn shapes_fold_names_and_inodes_and_count_by_cause() {
    let output = "1..5\n\
      not ok 1 - tried 'lstat pjdfstest_6524d26bcf0e1adc9151c384cd09cbb4/pjdfstest_f228994e8e46641519a42df04b9bf180 inode', expected ENOENT, got 281474976711259\n\
      not ok 2 - tried 'lstat pjdfstest_a770c46c500b1ed4795a9b618b78ebb7/pjdfstest_0518d1f09c827a9757daa1d739728de3 inode', expected ENOENT, got 281474976711261\n\
      not ok 3 - tried 'mknod pjdfstest_1/pjdfstest_2/test f 0644 0 0', expected 0, got EPERM\n\
      not ok 4 - tried '-u 65534 -g 65534 rename pjdfstest_1 pjdfstest_2', expected 0, got\n\
      not ok 5 not root\n";
    let parsed = parse_file("t", output, &Runner::Root);
    let (rows, total) = shapes(&parsed.cases, 10);
    assert_eq!(
      total, 3,
      "two inode lstats are one shape; the guard's line is needs-root, not a failure"
    );
    assert_eq!(
      rows[0],
      (
        "lstat N/N inode, expected ENOENT, got <inode>".to_owned(),
        2
      )
    );
    assert!(
      rows
        .iter()
        .any(|r| r.0 == "mknod N/N/test f 0644 0 0, expected 0, got EPERM")
    );
    assert!(
      rows
        .iter()
        .any(|r| r.0 == "-u 65534 -g 65534 rename N N, expected 0, got")
    );
    assert_eq!(shapes(&parsed.cases, 1).0.len(), 1, "the limit holds");
    assert_eq!(
      failures_by_file(&parsed.cases, 10),
      vec![("t".to_owned(), 4)],
      "the guard's line is needs-root, not a failure"
    );
  }
}
