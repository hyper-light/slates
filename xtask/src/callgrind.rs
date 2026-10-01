//! `cargo xtask callgrind`: the instruction-count gate (D-20, Part 6; AUD-29-32). The iai-callgrind benches
//! write one `summary.json` per benchmark (`cargo bench … -- --save-summary=json`); this task reads each
//! benchmark's instruction count (`Ir`) and compares it with the baseline committed for this platform in
//! `xtask/callgrind-baseline.json`. callgrind counts are deterministic for a given binary, so any difference is
//! a change in the code or the toolchain. The gate fails:
//!
//! - on a count more than [`TOLERANCE_PERCENT`] above its baseline (a regression);
//! - on a benchmark with no baseline — a missing baseline never looks like a passed gate;
//! - on a baseline entry no benchmark produced (a renamed or removed bench leaves a stale bar).
//!
//! A count below its baseline passes and is reported, so the bar can be lowered with `--record`, which
//! rewrites this platform's baseline from the counts measured. Counts differ across architectures, so
//! baselines are keyed by the platform the benches ran on.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::Failure;

/// Shape: the largest instruction-count increase the gate passes, in percent — the one-percent change the
/// wall-clock ratchet cannot see (the reason D-20 has this gate; `.github/workflows/ci.yml`'s callgrind lane).
const TOLERANCE_PERCENT: u64 = 1;
/// Format: the percent scale.
const PERCENT: u64 = 100;
/// Format: the committed baselines, relative to the workspace root.
const BASELINE_FILE: &str = "xtask/callgrind-baseline.json";

/// The platform the counts belong to: the architecture and operating system the xtask runs on (the CI lane
/// runs the benches and this task on one machine).
fn platform() -> String {
  format!("{}-{}", std::env::consts::ARCH, std::env::consts::OS)
}

/// Every `summary.json` under `dir`.
fn summaries(dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), Failure> {
  for entry in std::fs::read_dir(dir)? {
    let path = entry?.path();
    if path.is_dir() {
      summaries(&path, out)?;
    } else if path.file_name().is_some_and(|name| name == "summary.json") {
      out.push(path);
    }
  }
  Ok(())
}

/// The benchmark's key and its instruction count, from one summary.
fn count_of(summary: &serde_json::Value) -> Option<(String, u64)> {
  let module = summary.get("module_path")?.as_str()?;
  let id = summary.get("id")?.as_str()?;
  let count = summary
    .get("profiles")?
    .get(0)?
    .get("summaries")?
    .get("total")?
    .get("summary")?
    .get("Callgrind")?
    .get("Ir")?
    .get("metrics")?
    .get("Left")?
    .get("Int")?
    .as_u64()?;
  Some((format!("{module}.{id}"), count))
}

/// The counts measured under `iai`.
fn measured(iai: &Path) -> Result<BTreeMap<String, u64>, Failure> {
  let mut files = Vec::new();
  summaries(iai, &mut files)?;
  let mut counts = BTreeMap::new();
  for file in files {
    let value: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&file)?)?;
    let (key, count) = count_of(&value).ok_or_else(|| {
      Failure(format!(
        "{}: no instruction count in the summary",
        file.display()
      ))
    })?;
    counts.insert(key, count);
  }
  Ok(counts)
}

/// The committed baselines, by platform.
fn baselines(root: &Path) -> Result<BTreeMap<String, BTreeMap<String, u64>>, Failure> {
  let path = root.join(BASELINE_FILE);
  if !path.exists() {
    return Ok(BTreeMap::new());
  }
  Ok(serde_json::from_str(&std::fs::read_to_string(path)?)?)
}

/// What the gate finds wrong with `measured` against `baseline` (empty when it passes), and the notes it
/// prints either way.
fn verdict(
  measured: &BTreeMap<String, u64>,
  baseline: &BTreeMap<String, u64>,
) -> (Vec<String>, Vec<String>) {
  let mut problems = Vec::new();
  let mut notes = Vec::new();
  for (key, count) in measured {
    match baseline.get(key) {
      None => problems.push(format!("{key}: {count} instructions and no baseline")),
      Some(bar) => {
        let ceiling = bar.saturating_add(bar.saturating_mul(TOLERANCE_PERCENT) / PERCENT);
        if *count > ceiling {
          problems.push(format!(
            "{key}: {count} instructions, above the baseline {bar} by more than {TOLERANCE_PERCENT}%"
          ));
        } else if count < bar {
          notes.push(format!(
            "{key}: {count} instructions, below the baseline {bar}"
          ));
        } else {
          notes.push(format!("{key}: {count} instructions (baseline {bar})"));
        }
      }
    }
  }
  for key in baseline.keys().filter(|key| !measured.contains_key(*key)) {
    problems.push(format!(
      "{key}: a baseline no benchmark produced (renamed or removed?)"
    ));
  }
  (problems, notes)
}

/// Runs the gate over the summaries under `iai`, or records them with `record`.
pub fn run(root: &Path, iai: &Path, record: bool) -> Result<(), Failure> {
  let counts = measured(iai)?;
  if counts.is_empty() {
    return Err(Failure(format!(
      "no benchmark summaries under {} (run the benches with `-- --save-summary=json`)",
      iai.display()
    )));
  }
  let platform = platform();
  let mut all = baselines(root)?;
  if record {
    all.insert(platform.clone(), counts.clone());
    // The development tool rewriting the tree's own baseline file (not a host path of the product).
    #[allow(clippy::disallowed_methods)]
    std::fs::write(
      root.join(BASELINE_FILE),
      format!("{}\n", serde_json::to_string_pretty(&all)?),
    )?;
    println!(
      "callgrind: recorded {} baselines for {platform}",
      counts.len()
    );
    return Ok(());
  }
  let baseline = all.get(&platform).cloned().unwrap_or_default();
  let (problems, notes) = verdict(&counts, &baseline);
  for note in &notes {
    println!("callgrind: {note}");
  }
  for problem in &problems {
    eprintln!("callgrind: {problem}");
  }
  if problems.is_empty() {
    println!("callgrind: ok ({} benchmarks on {platform})", counts.len());
    Ok(())
  } else {
    let recorded = serde_json::to_string(&counts).unwrap_or_default();
    eprintln!("callgrind: this run's counts for {platform}, to record once reviewed: {recorded}");
    Err(Failure(format!(
      "{} instruction-count problem(s)",
      problems.len()
    )))
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn counts(pairs: &[(&str, u64)]) -> BTreeMap<String, u64> {
    pairs
      .iter()
      .map(|(key, count)| ((*key).to_owned(), *count))
      .collect()
  }

  /// AUD-29-32. Do: judge measured counts against baselines — equal, within the tolerance, past it, below,
  /// a bench with no baseline, and a baseline with no bench. Expect: only the regression past the tolerance,
  /// the missing baseline and the stale entry fail; equal, within-tolerance and improved counts pass.
  #[test]
  fn a_regression_a_missing_baseline_and_a_stale_entry_fail_and_nothing_else_does() {
    let baseline = counts(&[
      ("a", 1000),
      ("b", 1000),
      ("c", 1000),
      ("d", 1000),
      ("stale", 5),
    ]);
    let measured = counts(&[
      ("a", 1000),
      ("b", 1010),
      ("c", 1011),
      ("d", 900),
      ("new", 7),
    ]);
    let (problems, _) = verdict(&measured, &baseline);
    assert_eq!(problems.len(), 3, "{problems:#?}");
    assert!(problems.iter().any(|problem| problem.starts_with("c:")));
    assert!(problems.iter().any(|problem| problem.starts_with("new:")));
    assert!(problems.iter().any(|problem| problem.starts_with("stale:")));
  }

  /// AUD-29-32. Do: read an iai-callgrind 0.16 summary's shape. Expect: its key and instruction count.
  #[test]
  fn a_summary_yields_its_benchmark_and_instruction_count() {
    let summary: serde_json::Value = serde_json::from_str(
      r#"{"module_path":"callgrind::wire::header_encode","id":"encode","profiles":[{"summaries":
        {"total":{"summary":{"Callgrind":{"Ir":{"diffs":null,"metrics":{"Left":{"Int":34}}}}}}}}]}"#,
    )
    .unwrap();
    assert_eq!(
      count_of(&summary),
      Some(("callgrind::wire::header_encode.encode".to_owned(), 34))
    );
  }
}
