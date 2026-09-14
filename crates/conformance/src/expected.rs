//! The reviewed expected-failure list and the rule that it only shrinks (Part 6 "Conformance":
//! "All tests pass except the reviewed expected-failure list for that bridge, and the list only
//! shrinks"; example 3: "any newly passing case must be removed from the list"). One list per
//! transport lives under `docs/wip/conformance/expected-failures/`; a line names a pjdfstest case
//! by file and number and the reviewed reason it fails. The judgement of a run is closed: a
//! failure the list does not name is unexpected (a regression or an unreviewed case) and a listed
//! case that now passes must be struck — both fail the run, so the list can only get shorter by a
//! deliberate edit and can only get longer under review. The list's BLAKE3 goes into the record,
//! so a matrix cell names exactly the list it was judged against.

use crate::tap::{CaseStatus, TapCase};

/// Derived: the most bytes a list file may hold before it is parsed: pjdfstest has 3,581 `expect`
/// cases at the pinned commit; a list naming every one with a 200-character reason is under 1 MiB,
/// so the cap is 1 MiB (Part 6 "Hostile input": check the length before allocating).
pub const MAX_LIST_BYTES: usize = 1024 * 1024;

/// One reviewed expectation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExpectedFailure {
  /// `file:number`, as [`crate::tap::TestId`] prints it.
  pub id: String,
  /// Why the case fails on this transport, reviewed.
  pub reason: String,
}

/// The reviewed list.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ExpectedFailures {
  entries: Vec<ExpectedFailure>,
}

/// Why a list could not be read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ListError {
  /// The file is past [`MAX_LIST_BYTES`].
  TooLarge {
    /// The file's length.
    len: usize,
    /// The cap.
    cap: usize,
  },
  /// A line is neither a comment, blank, nor `file:number reason`.
  Malformed {
    /// The 1-based line.
    line: usize,
    /// The line's text.
    text: String,
  },
  /// A case is listed twice.
  Duplicate {
    /// The 1-based line of the second listing.
    line: usize,
    /// The case.
    id: String,
  },
}

impl std::fmt::Display for ListError {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      ListError::TooLarge { len, cap } => write!(f, "list of {len} bytes exceeds the cap of {cap}"),
      ListError::Malformed { line, text } => {
        write!(f, "line {line} is not `file:number reason`: {text}")
      }
      ListError::Duplicate { line, id } => write!(f, "line {line} lists {id} a second time"),
    }
  }
}

/// Whether `id` has the `file:number` shape.
fn well_formed(id: &str) -> bool {
  match id.rsplit_once(':') {
    Some((file, number)) => {
      !file.is_empty()
        && !file.chars().any(char::is_whitespace)
        && !number.is_empty()
        && number.chars().all(|c| c.is_ascii_digit())
    }
    None => false,
  }
}

impl ExpectedFailures {
  /// Parses a list, refusing an oversized file before allocation.
  pub fn parse(text: &str) -> Result<ExpectedFailures, ListError> {
    if text.len() > MAX_LIST_BYTES {
      return Err(ListError::TooLarge {
        len: text.len(),
        cap: MAX_LIST_BYTES,
      });
    }
    let mut entries: Vec<ExpectedFailure> = Vec::new();
    for (index, raw) in text.lines().enumerate() {
      let line = index + 1;
      let trimmed = raw.trim();
      if trimmed.is_empty() || trimmed.starts_with('#') {
        continue;
      }
      let (id, reason) = trimmed
        .split_once(char::is_whitespace)
        .unwrap_or((trimmed, ""));
      if !well_formed(id) {
        return Err(ListError::Malformed {
          line,
          text: raw.to_owned(),
        });
      }
      if entries.iter().any(|e| e.id == id) {
        return Err(ListError::Duplicate {
          line,
          id: id.to_owned(),
        });
      }
      entries.push(ExpectedFailure {
        id: id.to_owned(),
        reason: reason.trim().to_owned(),
      });
    }
    Ok(ExpectedFailures { entries })
  }

  /// The BLAKE3 of the canonical form (the sorted ids, one per line), hexadecimal.
  pub fn digest(&self) -> String {
    let mut ids: Vec<&str> = self.entries.iter().map(|e| e.id.as_str()).collect();
    ids.sort_unstable();
    let mut hasher = blake3::Hasher::new();
    for id in ids {
      hasher.update(id.as_bytes());
      hasher.update(b"\n");
    }
    hasher.finalize().to_hex().to_string()
  }

  /// How many expectations the list holds.
  pub fn len(&self) -> usize {
    self.entries.len()
  }

  /// Whether the list is empty.
  pub fn is_empty(&self) -> bool {
    self.entries.is_empty()
  }

  /// Whether the list names `id`.
  pub fn contains(&self, id: &str) -> bool {
    self.entries.iter().any(|e| e.id == id)
  }

  /// The expectations, in file order.
  pub fn entries(&self) -> &[ExpectedFailure] {
    &self.entries
  }
}

/// How a run's failures compare with the list.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Judgement {
  /// Failures the list does not name.
  pub unlisted_failures: Vec<String>,
  /// Listed cases that passed: the list must shrink.
  pub listed_now_passing: Vec<String>,
  /// Listed cases the run did not reach (a file that quick-exited, or a stale id).
  pub listed_absent: Vec<String>,
  /// Failures the list names.
  pub expected_failures: u32,
}

impl Judgement {
  /// Whether the run meets the list: no unlisted failure, nothing listed passing, nothing
  /// listed absent.
  pub fn acceptable(&self) -> bool {
    self.unlisted_failures.is_empty()
      && self.listed_now_passing.is_empty()
      && self.listed_absent.is_empty()
  }

  /// The judgement in words.
  pub fn describe(&self) -> String {
    format!(
      "{} expected failures; {} unlisted failures{}; {} listed now passing{}; {} listed absent{}",
      self.expected_failures,
      self.unlisted_failures.len(),
      sample(&self.unlisted_failures),
      self.listed_now_passing.len(),
      sample(&self.listed_now_passing),
      self.listed_absent.len(),
      sample(&self.listed_absent),
    )
  }
}

/// Shape: how many ids a judgement's description names before eliding the rest.
const SAMPLE: usize = 8;

fn sample(ids: &[String]) -> String {
  if ids.is_empty() {
    return String::new();
  }
  let shown: Vec<&str> = ids.iter().take(SAMPLE).map(String::as_str).collect();
  let more = ids.len().saturating_sub(SAMPLE);
  if more > 0 {
    format!(" ({} … and {more} more)", shown.join(", "))
  } else {
    format!(" ({})", shown.join(", "))
  }
}

/// Judges a run's cases against the list. A `Fail` is a failure; needs-root and TODO cases are
/// the suite's own classifications and are not failures the list covers.
pub fn judge(list: &ExpectedFailures, cases: &[TapCase]) -> Judgement {
  let mut judgement = Judgement::default();
  for case in cases {
    let id = case.id.to_string();
    match case.status {
      CaseStatus::Fail { .. } if list.contains(&id) => judgement.expected_failures += 1,
      CaseStatus::Fail { .. } => judgement.unlisted_failures.push(id),
      CaseStatus::Pass if list.contains(&id) => judgement.listed_now_passing.push(id),
      _ => {}
    }
  }
  for expected in list.entries() {
    if !cases.iter().any(|c| c.id.to_string() == expected.id) {
      judgement.listed_absent.push(expected.id.clone());
    }
  }
  judgement
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::tap::TestId;

  fn case(file: &str, number: u32, status: CaseStatus) -> TapCase {
    TapCase {
      id: TestId {
        file: file.to_owned(),
        number,
      },
      status,
    }
  }

  /// A list parses its ids and reasons, skips comments and blanks, and digests canonically.
  #[test]
  fn a_list_parses_and_digests_canonically() {
    let text = "# reviewed\n\ntests/chmod/00.t:12  NFS reports EPERM\ntests/open/01.t:3\n";
    let list = ExpectedFailures::parse(text).unwrap();
    assert_eq!(list.len(), 2);
    assert_eq!(list.entries()[0].reason, "NFS reports EPERM");
    assert_eq!(list.entries()[1].reason, "");
    let reordered =
      ExpectedFailures::parse("tests/open/01.t:3 x\ntests/chmod/00.t:12 y\n").unwrap();
    assert_eq!(
      list.digest(),
      reordered.digest(),
      "the digest is order-independent"
    );
    assert_ne!(list.digest(), ExpectedFailures::default().digest());
  }

  /// Hostile input: a malformed line, a duplicate and an oversized file are typed refusals.
  #[test]
  fn hostile_lists_are_refused_typed() {
    assert!(matches!(
      ExpectedFailures::parse("not an id\n"),
      Err(ListError::Malformed { line: 1, .. })
    ));
    assert!(matches!(
      ExpectedFailures::parse("a.t:1\na.t:1\n"),
      Err(ListError::Duplicate { line: 2, .. })
    ));
    assert!(matches!(
      ExpectedFailures::parse("a.t:x\n"),
      Err(ListError::Malformed { .. })
    ));
    let big = "#".repeat(MAX_LIST_BYTES + 1);
    assert!(matches!(
      ExpectedFailures::parse(&big),
      Err(ListError::TooLarge { .. })
    ));
  }

  /// The judgement: a listed failure is expected; an unlisted failure, a listed pass and a
  /// listed case that did not run each make the run unacceptable; needs-root and TODO do not.
  #[test]
  fn the_list_only_shrinks() {
    let list = ExpectedFailures::parse("a.t:1 reason\na.t:2 reason\na.t:9 stale\n").unwrap();
    let cases = vec![
      case(
        "a.t",
        1,
        CaseStatus::Fail {
          detail: "x".to_owned(),
        },
      ),
      case("a.t", 2, CaseStatus::Pass),
      case(
        "a.t",
        3,
        CaseStatus::Fail {
          detail: "y".to_owned(),
        },
      ),
      case(
        "a.t",
        4,
        CaseStatus::NeedsRoot {
          detail: "z".to_owned(),
        },
      ),
      case(
        "a.t",
        5,
        CaseStatus::TodoFail {
          detail: "t".to_owned(),
        },
      ),
    ];
    let judgement = judge(&list, &cases);
    assert_eq!(judgement.expected_failures, 1);
    assert_eq!(judgement.unlisted_failures, vec!["a.t:3".to_owned()]);
    assert_eq!(judgement.listed_now_passing, vec!["a.t:2".to_owned()]);
    assert_eq!(judgement.listed_absent, vec!["a.t:9".to_owned()]);
    assert!(!judgement.acceptable());
    assert!(judgement.describe().contains("1 unlisted failures (a.t:3)"));
    let exact = ExpectedFailures::parse("a.t:1 r\na.t:3 r\n").unwrap();
    let judgement = judge(&exact, &cases);
    assert!(judgement.acceptable(), "{}", judgement.describe());
    assert_eq!(judgement.expected_failures, 2);
  }
}
