//! Two runs of one suite compared case by case: a plain volume's and a live-shipped one's (A-200; the supervisor's
//! task of 2026-11-05, "a live-shipped volume must conform exactly as a plain one does"). A reviewed list judges a run
//! against what a transport claims; this judges two runs against each other on the same host, so it holds where no
//! list applies (an unprivileged pjdfstest run on a developer's Mac fails cases root alone could pass, the same in both
//! arms). A case compares by its verdict's kind alone: its message names the run's own generated files
//! (`pjdfstest_<hash>`, winfstest's counter names), which differ between any two runs.

use std::collections::BTreeMap;

use crate::tap::{CaseStatus, TapCase};

/// A case's verdict, without its message.
pub fn kind(status: &CaseStatus) -> &'static str {
  match status {
    CaseStatus::Pass => "pass",
    CaseStatus::Fail { .. } => "fail",
    CaseStatus::NeedsRoot { .. } => "needs-root",
    CaseStatus::TodoFail { .. } => "todo-fail",
    CaseStatus::TodoPass => "todo-pass",
  }
}

/// One case whose verdicts differ, or that one run reported and the other did not.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Difference {
  /// The case, `file:number`.
  pub id: String,
  /// Its verdict in the plain run, if reported.
  pub plain: Option<&'static str>,
  /// Its verdict in the live-shipped run, if reported.
  pub live: Option<&'static str>,
}

impl std::fmt::Display for Difference {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    write!(
      f,
      "{}: plain {}, live {}",
      self.id,
      self.plain.unwrap_or("absent"),
      self.live.unwrap_or("absent")
    )
  }
}

fn verdicts(cases: &[TapCase]) -> BTreeMap<String, &'static str> {
  cases
    .iter()
    .map(|case| (case.id.to_string(), kind(&case.status)))
    .collect()
}

/// Every case whose verdict differs between the plain run and the live-shipped one, in case order; empty when the live
/// run conforms exactly as the plain one does.
pub fn compare(plain: &[TapCase], live: &[TapCase]) -> Vec<Difference> {
  let plain = verdicts(plain);
  let live = verdicts(live);
  let mut ids: Vec<&String> = plain.keys().chain(live.keys()).collect();
  ids.sort_unstable();
  ids.dedup();
  ids
    .into_iter()
    .filter_map(|id| {
      let (a, b) = (plain.get(id).copied(), live.get(id).copied());
      (a != b).then(|| Difference {
        id: id.clone(),
        plain: a,
        live: b,
      })
    })
    .collect()
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::tap::{Runner, parse_file};

  /// A-200's conformance: two runs whose cases agree in kind compare equal though their messages name different
  /// generated files; a case that fails in one and passes in the other, and one the other did not report, are
  /// differences, each named with both verdicts.
  #[test]
  fn runs_compare_by_verdict_kind_and_report_every_difference() {
    let plain = parse_file(
      "tests/rename/00.t",
      "1..3\nok 1\nnot ok 2 - tried 'rename pjdfstest_aa pjdfstest_bb', expected 0, got EXDEV\nok 3\n",
      &Runner::Root,
    );
    let same = parse_file(
      "tests/rename/00.t",
      "1..3\nok 1\nnot ok 2 - tried 'rename pjdfstest_cc pjdfstest_dd', expected 0, got EXDEV\nok 3\n",
      &Runner::Root,
    );
    assert!(compare(&plain.cases, &same.cases).is_empty());
    let differs = parse_file(
      "tests/rename/00.t",
      "1..3\nnot ok 1 - tried 'x', expected 0, got EIO\nnot ok 2 - tried 'y', expected 0, got EXDEV\n",
      &Runner::Root,
    );
    let found = compare(&plain.cases, &differs.cases);
    assert_eq!(
      found.iter().map(ToString::to_string).collect::<Vec<_>>(),
      vec![
        "tests/rename/00.t:1: plain pass, live fail",
        "tests/rename/00.t:3: plain pass, live absent"
      ]
    );
  }
}
