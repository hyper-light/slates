//! Property-test failure persistence without a disk (design §0.2, A-50; audit AUD-29-63).
//!
//! proptest's default persistence reads a seed file beside the test's source before a run, and after a
//! failure writes the new seed there. That write is a disk contact that A-50 forbids to tests as to the
//! product: a failing CI run wrote a regression file into its checkout (run 36655388624,
//! `docs/bugs/2026-09-30-the-position-mapping-oracle-judged-only-the-head.md`). Turning persistence off
//! (`failure_persistence: None`) would also stop replaying the seeds already reviewed into the tree,
//! which the audit requires be kept as source evidence.
//!
//! [`CompiledSeeds`] keeps both properties. The reviewed seed file is compiled into the test binary
//! (`include_str!`, read by the compiler, never at run time), parsed once with a typed refusal for a line
//! that does not parse, and replayed before novel cases, exactly as proptest's file persistence replays
//! it. A new failing seed is written to the test's error stream in the file's own line format, for a
//! human to review and add; the test binary opens no file. Every property suite in the workspace goes
//! through [`seeded`] or [`unseeded`]; `cargo xtask check` refuses a suite that does not.

#![cfg_attr(not(test), deny(clippy::arithmetic_side_effects))]

use std::any::Any;
use std::fmt;

use proptest::test_runner::{Config, FailurePersistence, PersistedSeed};

/// Format: the character that starts a comment in a seed file (proptest's own format).
const COMMENT: char = '#';

/// A line of a reviewed seed file that is not a seed proptest can replay.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SeedRefused {
  /// The line's number, counted from one.
  pub line: usize,
  /// The line's text without its comment.
  pub text: String,
}

impl fmt::Display for SeedRefused {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    write!(
      formatter,
      "seed file line {} is not a proptest seed: {:?}",
      self.line, self.text
    )
  }
}

impl std::error::Error for SeedRefused {}

/// The reviewed seeds of one property suite, held in memory, and what it reported.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompiledSeeds {
  seeds: Vec<PersistedSeed>,
  /// New failing seeds reported (proptest reports at most one per run).
  reported: u64,
  /// The last reported line, in the seed file's format: bounded to one.
  last_reported: Option<String>,
}

impl CompiledSeeds {
  /// Parses a seed file's text (proptest's format: one `<algorithm> <seed>` per line, `#` to the line's
  /// end a comment, blank lines ignored). A line that is not a seed refuses the whole file: a reviewed
  /// seed that silently stopped replaying is the loss this type exists to prevent.
  pub fn parse(reviewed: &str) -> Result<Self, SeedRefused> {
    let mut seeds = Vec::new();
    for (index, raw) in reviewed.lines().enumerate() {
      let text = raw.split(COMMENT).next().unwrap_or_default().trim();
      if text.is_empty() {
        continue;
      }
      let seed = text.parse::<PersistedSeed>().map_err(|()| SeedRefused {
        line: index.saturating_add(1),
        text: text.to_owned(),
      })?;
      seeds.push(seed);
    }
    Ok(Self {
      seeds,
      reported: 0,
      last_reported: None,
    })
  }

  /// New failing seeds this persistence reported instead of writing.
  pub fn reported(&self) -> u64 {
    self.reported
  }

  /// The last reported line, in the seed file's format.
  pub fn last_reported(&self) -> Option<&str> {
    self.last_reported.as_deref()
  }

  /// How many seeds replay before novel cases.
  pub fn len(&self) -> usize {
    self.seeds.len()
  }

  /// Whether no seed replays.
  pub fn is_empty(&self) -> bool {
    self.seeds.is_empty()
  }
}

impl FailurePersistence for CompiledSeeds {
  fn load_persisted_failures2(&self, _source_file: Option<&'static str>) -> Vec<PersistedSeed> {
    self.seeds.clone()
  }

  fn save_persisted_failure2(
    &mut self,
    source_file: Option<&'static str>,
    seed: PersistedSeed,
    shrunken_value: &dyn fmt::Debug,
  ) {
    // The line proptest's file persistence would have written, with its newlines folded the same way,
    // reported instead of written (A-50).
    let shrunk = format!("{shrunken_value:?}").replace(['\n', '\r'], " ");
    let line = format!("{seed} {COMMENT} shrinks to {shrunk}");
    eprintln!(
      "proptest: a new failing seed for {}, kept off the disk (A-50); review it and add this line to \
       the suite's seed file to replay it:\n{line}",
      source_file.unwrap_or("this suite")
    );
    self.reported = self.reported.saturating_add(1);
    self.last_reported = Some(line);
  }

  fn box_clone(&self) -> Box<dyn FailurePersistence> {
    Box::new(self.clone())
  }

  fn eq(&self, other: &dyn FailurePersistence) -> bool {
    other
      .as_any()
      .downcast_ref::<Self>()
      .is_some_and(|other| other == self)
  }

  fn as_any(&self) -> &dyn Any {
    self
  }
}

/// `config` replaying the reviewed seeds `reviewed` (a seed file's text, `include_str!`-ed) and reporting
/// a new failing seed instead of writing it.
pub fn seeded(config: Config, reviewed: &str) -> Result<Config, SeedRefused> {
  let seeds = CompiledSeeds::parse(reviewed)?;
  Ok(Config {
    failure_persistence: Some(Box::new(seeds)),
    ..config
  })
}

/// `config` for a suite with no reviewed seeds: a new failing seed is reported, never written.
pub fn unseeded(config: Config) -> Config {
  Config {
    failure_persistence: Some(Box::new(CompiledSeeds {
      seeds: Vec::new(),
      reported: 0,
      last_reported: None,
    })),
    ..config
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use proptest::prelude::*;
  use proptest::test_runner::{TestCaseError, TestRunner};

  /// Shape: a seed line in proptest's ChaCha format, as the reviewed files hold them.
  const SEED_LINE: &str = "cc 7bb88ab4be26465a754eace22580dd19ee58497b2fce07a489f471ea2dd6dff7";

  /// AUD-29-63. Do: parse a reviewed file with its header comment, a blank line and two seeds (one with a
  /// trailing comment). Expect: both seeds, in order, and each prints back as its line.
  #[test]
  fn a_reviewed_file_replays_every_seed_and_ignores_comments() {
    let file = format!(
      "# Seeds for failure cases proptest has generated in the past.\n\n{SEED_LINE} # shrinks to x = 1\n\
       xs 1 2 3 4\n"
    );
    let seeds = CompiledSeeds::parse(&file).unwrap();
    assert_eq!(seeds.len(), 2);
    let lines: Vec<String> = seeds
      .load_persisted_failures2(None)
      .iter()
      .map(ToString::to_string)
      .collect();
    assert_eq!(lines, [SEED_LINE, "xs 1 2 3 4"]);
  }

  /// AUD-29-63. Do: parse a file whose third line is not a seed. Expect: a typed refusal naming line 3.
  #[test]
  fn a_line_that_is_not_a_seed_refuses_the_file() {
    let refused =
      CompiledSeeds::parse(&format!("# header\n{SEED_LINE}\ncc not-hex\n")).unwrap_err();
    assert_eq!(refused.line, 3);
    assert_eq!(refused.text, "cc not-hex");
  }

  /// The persistence a runner holds after its run.
  fn persistence_of(runner: &TestRunner) -> &CompiledSeeds {
    let Some(persistence) = runner.config().failure_persistence.as_ref() else {
      panic!("the runner keeps its persistence");
    };
    let Some(seeds) = persistence.as_any().downcast_ref::<CompiledSeeds>() else {
      panic!("the runner's persistence is the compiled one");
    };
    seeds
  }

  /// The first value a failing property sees under `config`.
  fn first_value(config: Config) -> u64 {
    let mut runner = TestRunner::new(config);
    // The property is a `Fn`; the first value is kept in a cell.
    let first = std::cell::Cell::new(None);
    let outcome = runner.run(&any::<u64>(), |value| {
      if first.get().is_none() {
        first.set(Some(value));
      }
      Err(TestCaseError::fail("record the first value"))
    });
    assert!(outcome.is_err(), "the property failed");
    let Some(first) = first.get() else {
      panic!("the property saw a value");
    };
    first
  }

  /// AUD-29-63 / A-50. Do: run a property that fails for every value, with this file named as its source
  /// (so proptest's default persistence would write `proptest-regressions/lib.txt` beside the crate).
  /// Expect: the run fails; the persistence the runner holds reported exactly one seed, in the seed file's
  /// format (it parses back); and no seed file or directory appeared beside the crate.
  #[test]
  fn a_failing_property_is_reported_and_never_written() {
    let config = unseeded(Config {
      cases: 4,
      source_file: Some(file!()),
      ..Config::default()
    });
    let mut runner = TestRunner::new(config);
    let outcome = runner.run(&any::<u8>(), |_| Err(TestCaseError::fail("always")));
    assert!(outcome.is_err(), "the property failed");
    let seeds = persistence_of(&runner);
    assert_eq!(
      seeds.reported(),
      1,
      "the failing seed went through this persistence"
    );
    let line = seeds.last_reported().unwrap_or_default();
    assert_eq!(CompiledSeeds::parse(line).map(|s| s.len()), Ok(1), "{line}");
    let crate_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    assert!(
      !crate_dir.join("proptest-regressions").exists(),
      "no seed directory was made"
    );
    assert!(
      !crate_dir.join("src/lib.proptest-regressions").exists(),
      "no seed file was written"
    );
  }

  /// AUD-29-63. Do: take the first value a failing property sees under a compiled seed, twice, and under
  /// an unseeded runner (proptest seeds it at random). Expect: the seeded runs agree and the unseeded one
  /// differs (probability 2^-64 of a false failure): the compiled seed is replayed first, not ignored.
  #[test]
  fn a_compiled_seed_is_replayed_before_novel_cases() {
    let seeded_config = || seeded(Config::default(), &format!("{SEED_LINE}\n")).unwrap();
    let replayed = first_value(seeded_config());
    assert_eq!(first_value(seeded_config()), replayed);
    assert_ne!(first_value(unseeded(Config::default())), replayed);
  }
}
