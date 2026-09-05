//! `cargo xtask ratchet`: the performance ratchet (design R4, Part 6; Phase 0 task 7).
//!
//! The three bench examples print one `ratchet\t<key>\t<lower>\t<median>\t<upper>` line per row.
//! This task runs each example N times in release mode and treats the runs as the outer level of
//! Kalibera and Jones's hierarchy [A: Kalibera & Jones, ISMM'13]: a run's interval captures the
//! noise inside one process, and the spread across runs captures the noise between processes
//! (thermal state, frequency, what else the machine is doing), which a single run cannot see.
//! The recorded ceiling of a row is therefore the highest upper edge across its N runs, and the
//! check compares the lowest lower edge across N fresh runs against it: a regression is declared
//! only when every fresh run sits wholly above every recorded run. The smallest change the gate
//! can see is the between-run drift of the machine, which every run prints so it is known.
//!
//! Ceilings are keyed by the machine identity hash the profile computes, because a number
//! measured on one machine says nothing about another. A machine without an entry skips loudly;
//! `--record` writes a first baseline for the current machine; `--tighten` lowers ceilings to a
//! better run, and only when the improvement is larger than the row's own between-run drift,
//! so a lucky cold run never sets a bar a warm run fails; it never raises one; `--reset`
//! rebuilds the current machine's entry from scratch (a deliberate act, for when the rule or
//! the benches change); `--runs N` sets N.
//!
//! N defaults to a ratified shape constant: three is the smallest count with a middle run, which
//! is what "best of N with all N shown" (CLAUDE.md §5) needs to show whether the best was a fluke.
//! The instruction-count gate the design also names (iai-callgrind, D-20) needs valgrind, which
//! is non-Rust tooling and waits on authorization (GAPS §8a); this gate is what exists without it.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;

use crate::Failure;

/// Shape: default runs per bench example (see the module doc).
const DEFAULT_RUNS: usize = 3;

/// The bench examples, as `(crate, example)`.
const BENCHES: &[(&str, &str)] = &[
  ("slates-mem", "mem_bench"),
  ("slates-rt", "rt_bench"),
  ("slates-wire", "wire_bench"),
  ("slates-vfs", "vfs_bench"),
  ("slates-base", "base_bench"),
  ("slates-land", "land_bench"),
  ("slates-db", "db_bench"),
  ("slates-ipc", "ipc_bench"),
];

/// One row of one run.
#[derive(Clone, Copy, Debug)]
struct Row {
  lower: u64,
  median: u64,
  upper: u64,
  /// False for `ratchet-info` rows: printed, never gated (their cost depends on thread
  /// placement the OS refused to control).
  gated: bool,
}

/// One row across the runs.
#[derive(Clone, Debug)]
struct Across {
  /// Whether the row is gated.
  gated: bool,
  /// The lowest lower edge of any run.
  min_lower: u64,
  /// The run medians, in run order.
  medians: Vec<u64>,
  /// The highest upper edge of any run.
  max_upper: u64,
}

impl Across {
  fn best_median(&self) -> u64 {
    self.medians.iter().copied().min().unwrap_or(0)
  }

  /// Whether this measurement improves on `ceiling` by more than its own between-run drift
  /// (parts per thousand of the best median), so tightening never follows a lucky run.
  fn clears(&self, ceiling: u64) -> bool {
    /// Format: parts per thousand.
    const PERMILLE: u64 = 1000;
    self.max_upper < ceiling
      && (ceiling - self.max_upper).saturating_mul(PERMILLE)
        > ceiling.saturating_mul(self.drift_permille())
  }

  /// The between-run drift: the spread of the run medians as parts per thousand of the best.
  fn drift_permille(&self) -> u64 {
    let best = self.best_median().max(1);
    let worst = self.medians.iter().copied().max().unwrap_or(0);
    /// Format: parts per thousand.
    const PERMILLE: u64 = 1000;
    (worst.saturating_sub(best)).saturating_mul(PERMILLE) / best
  }
}

/// The recorded file.
#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
struct Ratchets {
  #[serde(default)]
  machine: BTreeMap<String, Machine>,
}

#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
struct Machine {
  identity: String,
  recorded: String,
  #[serde(default)]
  runs: usize,
  #[serde(default)]
  ceilings: BTreeMap<String, u64>,
}

/// The flags.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Flags {
  pub(crate) record: bool,
  pub(crate) tighten: bool,
  pub(crate) reset: bool,
  pub(crate) runs: Option<usize>,
}

/// Runs the ratchet with the given flags.
pub(crate) fn run(root: &Path, flags: Flags) -> Result<(), Failure> {
  let facts = slates_machine::facts::Facts::query();
  let identity = facts.identity.line();
  let hash: String = facts
    .identity
    .hash()
    .iter()
    .take(8)
    .map(|b| format!("{b:02x}"))
    .collect();
  let path = root.join("ratchets.toml");
  let text = std::fs::read_to_string(&path).unwrap_or_default();
  let header: String = text
    .lines()
    .take_while(|l| l.starts_with('#'))
    .map(|l| format!("{l}\n"))
    .collect();
  let mut file: Ratchets =
    toml::from_str(&text).map_err(|e| Failure(format!("ratchets.toml: {e}")))?;

  if flags.reset {
    file.machine.remove(&hash);
  }
  let known = file.machine.contains_key(&hash);
  let writing = flags.record || flags.tighten || flags.reset;
  if !known && !writing {
    println!("ratchet: skipped — no baseline recorded for this machine ({identity}, id {hash});");
    println!("ratchet: run `cargo xtask ratchet --record` here to set one");
    return Ok(());
  }
  let runs = flags.runs.unwrap_or(DEFAULT_RUNS).max(1);

  let across = measure_all(root, runs)?;
  let entry = file.machine.entry(hash.clone()).or_default();
  if !known {
    entry.identity = identity.clone();
    entry.recorded = today();
    entry.runs = runs;
  }

  let mut regressions = Vec::new();
  let mut tightened = 0usize;
  let mut recorded = 0usize;
  println!(
    "{:<40} {:>9} {:>22} {:>9} {:>9} {:>6}  verdict",
    "row", "min lower", "run medians", "max upper", "ceiling", "drift‰"
  );
  for (key, a) in &across {
    let ceiling = entry.ceilings.get(key).copied();
    let verdict = match ceiling {
      _ if !a.gated => {
        entry.ceilings.remove(key);
        "informational (thread placement not controllable here)"
      }
      None if writing => {
        entry.ceilings.insert(key.clone(), a.max_upper);
        recorded += 1;
        "recorded"
      }
      None => "no ceiling (run --tighten to add)",
      Some(c) if a.min_lower > c => {
        regressions.push(format!(
          "{key}: every run's interval lies above the ceiling {c} (lowest lower edge {}, run medians {:?})",
          a.min_lower, a.medians
        ));
        "REGRESSION"
      }
      // A ceiling tightens only when the improvement clears the row's own between-run drift:
      // a lucky cold run must not set a bar a warm run then fails (measured 2026-09-05: three
      // rows, one in a crate untouched since Phase 0, "regressed" by 1-5% after such a run).
      Some(c) if (flags.tighten || flags.record) && a.clears(c) => {
        entry.ceilings.insert(key.clone(), a.max_upper);
        tightened += 1;
        "tightened"
      }
      Some(_) => "ok",
    };
    println!(
      "{:<40} {:>9} {:>22} {:>9} {:>9} {:>6}  {verdict}",
      key,
      a.min_lower,
      format!("{:?}", a.medians),
      a.max_upper,
      ceiling.map_or("-".to_owned(), |c| c.to_string()),
      a.drift_permille()
    );
  }
  let worst_drift = across
    .values()
    .map(Across::drift_permille)
    .max()
    .unwrap_or(0);
  println!(
    "ratchet: between-run drift up to {worst_drift}‰ of a row's best median over {runs} runs; a change inside that is invisible to this gate"
  );
  if writing {
    let body = toml::to_string_pretty(&file).map_err(|e| Failure(format!("ratchets.toml: {e}")))?;
    // xtask is a development tool rewriting a tracked file in the repository, like the design's
    // `--ignored regenerate` writers; shipped code never reaches this call.
    #[allow(clippy::disallowed_methods)]
    std::fs::write(&path, format!("{header}\n{body}"))?;
    println!(
      "ratchet: wrote {} ({recorded} recorded, {tightened} tightened)",
      path.display()
    );
  }
  if regressions.is_empty() {
    println!(
      "ratchet: ok ({} rows against the baseline of {identity})",
      across.len()
    );
    Ok(())
  } else {
    for r in &regressions {
      eprintln!("ratchet: {r}");
    }
    Err(Failure(format!(
      "{} performance regression(s) against the recorded ceilings",
      regressions.len()
    )))
  }
}

/// Runs every bench `runs` times and folds the runs per row, printing every run.
fn measure_all(root: &Path, runs: usize) -> Result<BTreeMap<String, Across>, Failure> {
  let mut across: BTreeMap<String, Across> = BTreeMap::new();
  for (krate, example) in BENCHES {
    for run in 1..=runs {
      let output = Command::new(env!("CARGO"))
        .args([
          "run",
          "--quiet",
          "--release",
          "-p",
          krate,
          "--example",
          example,
        ])
        .current_dir(root)
        .output()?;
      if !output.status.success() {
        // The bench's own acceptance lines are on stdout; the last of them names the gate.
        let stdout = String::from_utf8_lossy(&output.stdout);
        let gates: Vec<&str> = stdout
          .lines()
          .filter(|l| l.starts_with("ac-") || l.starts_with("gate"))
          .collect();
        return Err(Failure(format!(
          "{krate} bench failed: {}\n{}",
          String::from_utf8_lossy(&output.stderr).trim(),
          gates.join("\n")
        )));
      }
      let rows = parse(&String::from_utf8_lossy(&output.stdout));
      println!("{krate} run {run}/{runs}: {} rows", rows.len());
      for (key, row) in rows {
        println!(
          "  {key}: median {} [{}, {}]",
          row.median, row.lower, row.upper
        );
        let a = across.entry(key).or_insert(Across {
          gated: row.gated,
          min_lower: u64::MAX,
          medians: Vec::new(),
          max_upper: 0,
        });
        a.min_lower = a.min_lower.min(row.lower);
        a.max_upper = a.max_upper.max(row.upper);
        a.medians.push(row.median);
      }
    }
  }
  Ok(across)
}

fn parse(stdout: &str) -> BTreeMap<String, Row> {
  let mut rows = BTreeMap::new();
  for line in stdout.lines() {
    let mut parts = line.split('\t');
    let gated = match parts.next() {
      Some("ratchet") => true,
      Some("ratchet-info") => false,
      _ => continue,
    };
    let (Some(key), Some(lower), Some(median), Some(upper)) =
      (parts.next(), parts.next(), parts.next(), parts.next())
    else {
      continue;
    };
    if let (Ok(lower), Ok(median), Ok(upper)) = (lower.parse(), median.parse(), upper.parse()) {
      rows.insert(
        key.to_owned(),
        Row {
          lower,
          median,
          upper,
          gated,
        },
      );
    }
  }
  rows
}

/// Today's date as the OS reports it, for the record line.
fn today() -> String {
  let output = Command::new("date").arg("+%Y-%m-%d").output();
  output
    .ok()
    .filter(|o| o.status.success())
    .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
    .unwrap_or_else(|| "unknown".to_owned())
}
