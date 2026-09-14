//! pjdfstest's TAP output, parsed per test file (Part 6 "Conformance"). Each `tests/**/*.t` is a
//! shell script that prints a plan (`1..N`) and one line per case: `ok N`, `ok N # TODO msg`,
//! `not ok N - tried '<pjdfstest args>', expected <e>, got <r>`, `not ok N # TODO msg`, or the
//! `requires_root` guard's `not ok N not root` (`tests/misc.sh` at the pinned commit). The parser
//! runs one file at a time so every case is attributed to its file without `prove`, keeps each
//! detail bounded, never panics on any line, and counts the lines it could not read instead of
//! guessing at them.
//!
//! Privilege is part of the classification: pjdfstest's README requires root, and a case whose
//! command switches uid or gid (`-u`/`-g`) fails as an ordinary user for that reason alone, so an
//! unprivileged run counts such a failure as needs-root, not as a failure of the transport. A root
//! run classifies nothing that way.

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

/// Format: the TODO directive TAP and pjdfstest's `misc.sh` use.
const TODO: &str = "# TODO";
/// Format: the `requires_root` guard's message (`misc.sh`).
const NOT_ROOT: &str = "not root";

/// Whether a failure's message shows a uid or gid switch: a `-u` or `-g` token in the tried
/// command (`tried '-u 65534 -g 65534 chmod …'`; the first token carries the opening quote).
fn switches_identity(message: &str) -> bool {
  message
    .split_whitespace()
    .any(|token| matches!(token.trim_start_matches('\''), "-u" | "-g"))
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
fn failure_status(rest: &str, privilege: Privilege) -> CaseStatus {
  if let Some(todo) = rest.split_once(TODO) {
    return CaseStatus::TodoFail {
      detail: detail(todo.1.trim()),
    };
  }
  let message = rest.trim_start_matches('-').trim();
  let needs_root = message.starts_with(NOT_ROOT)
    || (privilege == Privilege::Unprivileged
      && (switches_identity(message) || makes_device_node(message)));
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

fn read_line(raw: &str, privilege: Privilege) -> Line {
  let line = raw.trim();
  if line.is_empty() || line.starts_with('#') {
    return Line::Skip;
  }
  if let Some(plan) = line.strip_prefix("1..") {
    return plan.trim().parse().map_or(Line::Malformed, Line::Plan);
  }
  if let Some(rest) = line.strip_prefix("not ok") {
    return match number_and_rest(rest) {
      Some((number, rest)) => Line::Case(number, failure_status(rest, privilege)),
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
pub fn parse_file(file: &str, output: &str, privilege: Privilege) -> TapFile {
  let mut parsed = TapFile {
    file: file.to_owned(),
    plan: None,
    cases: Vec::new(),
    malformed_lines: 0,
  };
  for raw in output.lines() {
    match read_line(raw, privilege) {
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

  /// The pass, fail and needs-root forms of `misc.sh` classify for an unprivileged run, the plan
  /// is met, and a case is identified by its file and number.
  #[test]
  fn pass_fail_and_uid_switch_classify_unprivileged() {
    let parsed = parse_file("tests/chmod/00.t", OUTPUT, Privilege::Unprivileged);
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
    let parsed = parse_file("tests/chmod/00.t", OUTPUT, Privilege::Unprivileged);
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
    let unprivileged = parse_file("t", output, Privilege::Unprivileged);
    assert!(matches!(
      unprivileged.cases[0].status,
      CaseStatus::NeedsRoot { .. }
    ));
    assert!(matches!(
      unprivileged.cases[1].status,
      CaseStatus::Fail { .. }
    ));
    let root = parse_file("t", output, Privilege::Root);
    assert!(matches!(root.cases[0].status, CaseStatus::Fail { .. }));
  }

  /// A uid switch is found as a token even when it opens the tried command (`'-u`).
  #[test]
  fn a_leading_uid_switch_is_found() {
    let output = "1..1\nnot ok 1 - tried '-u 65534 chmod x 0600', expected 0, got EPERM\n";
    let parsed = parse_file("t", output, Privilege::Unprivileged);
    assert!(matches!(
      parsed.cases[0].status,
      CaseStatus::NeedsRoot { .. }
    ));
  }

  /// As root, a uid switch that fails is a failure of the transport, not of privilege.
  #[test]
  fn root_runs_classify_uid_switches_as_failures() {
    let parsed = parse_file("t", OUTPUT, Privilege::Root);
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
      Privilege::Root,
    );
    assert_eq!(
      parsed.malformed_lines, 3,
      "garbage, the overflow and the bare ok"
    );
    assert_eq!(parsed.cases.len(), 2);
    assert!(!parsed.complete(), "two of three planned");
    let none = parse_file("t", "", Privilege::Root);
    assert!(!none.complete());
    let long = format!("not ok 1 - {}\n", "x".repeat(10_000));
    let parsed = parse_file("t", &long, Privilege::Root);
    assert!(
      matches!(&parsed.cases[0].status, CaseStatus::Fail { detail } if detail.chars().count() <= crate::record::DETAIL_CHARS + 1)
    );
  }
}
