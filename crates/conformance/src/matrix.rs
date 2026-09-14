//! The evidence matrix (AC-9.7 "publish capability-specific results"): one row per transport,
//! one column per suite, every cell one of `RAN`, `LIMITED`, `SKIPPED` or `OWED`, rendered from
//! the record set and nothing else. `docs/wip/conformance.md` carries the rendering between
//! `<!-- conformance-matrix:begin -->` and `<!-- conformance-matrix:end -->` as a doc-truth
//! block: `crates/conformance/tests/matrix.rs` re-renders it from the tracked records and fails
//! on any drift; `--ignored regenerate_the_evidence_matrix` rewrites it deliberately. The block
//! also lists, under the table, the exact command behind every `RAN` and `LIMITED` cell, because a
//! number without its command is not evidence (CLAUDE.md §5).

use crate::record::{Counts, Outcome, Record, Suite, Transport, WorkloadStatus};

/// The marker that opens the generated block (kept with its newline so the block starts on a line).
pub const BEGIN: &str = "<!-- conformance-matrix:begin -->\n";
/// The marker that closes the generated block.
pub const END: &str = "<!-- conformance-matrix:end -->";

/// The record for a cell, if the set holds one.
fn cell_record(records: &[Record], transport: Transport, suite: Suite) -> Option<&Record> {
  records
    .iter()
    .find(|r| r.transport == transport && r.suite == suite)
}

/// Renders the block: the table, then the commands.
pub fn render(records: &[Record]) -> String {
  let mut out = String::new();
  out.push_str("| Transport |");
  for suite in Suite::ALL {
    out.push(' ');
    out.push_str(suite.label());
    out.push_str(" |");
  }
  out.push('\n');
  out.push_str("|---|");
  for _ in Suite::ALL {
    out.push_str("---|");
  }
  out.push('\n');
  for transport in Transport::ALL {
    out.push_str("| ");
    out.push_str(transport.label());
    out.push_str(" |");
    for suite in Suite::ALL {
      out.push(' ');
      out.push_str(&cell(cell_record(records, transport, suite)));
      out.push_str(" |");
    }
    out.push('\n');
  }
  out.push('\n');
  out.push_str(&commands(records));
  out
}

/// The commands behind every `RAN` and `LIMITED` cell, in matrix order.
fn commands(records: &[Record]) -> String {
  let mut out = String::from("Commands behind the cells above (the contract of each number):\n\n");
  let mut any = false;
  for transport in Transport::ALL {
    for suite in Suite::ALL {
      if let Some(record) = cell_record(records, transport, suite).filter(|r| r.is_evidence()) {
        any = true;
        out.push_str(&format!(
          "- **{} × {}** ({}, {}, {} {}): `{}`; bound: {}.\n",
          transport.label(),
          suite.label(),
          record.date,
          record.host.os,
          record.host.kernel,
          record.host.arch,
          record.command,
          record.bound
        ));
        for note in &record.notes {
          // A note may quote a suite's multi-line output; the list item stays one line.
          out.push_str(&format!("  - {}\n", note.replace('\n', " ⏎ ")));
        }
      }
    }
  }
  if !any {
    out.push_str("- (no run recorded yet)\n");
  }
  out
}

/// One cell's text.
pub fn cell(record: Option<&Record>) -> String {
  let Some(record) = record else {
    return "OWED (no record)".to_owned();
  };
  match &record.outcome {
    Outcome::Ran { counts } => format!(
      "RAN({}; {}; {})",
      summary(counts),
      record.date,
      record.host.os
    ),
    Outcome::Limited {
      adapter,
      not_covered,
      counts,
    } => format!(
      "LIMITED(adapter: {adapter}; not covered: {not_covered}; {}; {}; {})",
      summary(counts),
      record.date,
      record.host.os
    ),
    Outcome::Skipped { reason } => format!("SKIPPED({}: {})", reason.class(), reason.text()),
  }
}

/// The one-line summary of a run's counts.
pub fn summary(counts: &Counts) -> String {
  match counts {
    Counts::Pjdfstest {
      files,
      cases,
      passed,
      failed,
      needs_root,
      todo,
      expected_failures,
      unexpected_failures,
      listed_now_passing,
    } => format!(
      "{files} files, {cases} cases: {passed} passed, {failed} failed ({expected_failures} expected, \
       {unexpected_failures} unexpected, {listed_now_passing} listed-now-passing), {needs_root} needs-root, \
       {todo} todo"
    ),
    Counts::Fsx {
      operations,
      seed,
      file_length,
      ok,
    } => format!(
      "{operations} operations, seed {seed}, file length {file_length}: {}",
      verdict(*ok)
    ),
    Counts::Fsstress {
      operations,
      processes,
      seed,
      logged_operations,
      disabled_operations,
      ok,
    } => format!(
      "{operations} operations × {processes} processes, seed {seed}, {logged_operations} logged{}: {}",
      disabled(disabled_operations),
      verdict(*ok)
    ),
    Counts::Workloads { tools } => workloads(tools),
    Counts::Hermeticity {
      write_calls,
      inside_target,
      ram_only,
      standard_streams,
      unresolved,
      outside,
      written_matched,
      written_unmatched,
    } => format!(
      "{write_calls} write-capable calls: {inside_target} inside the granted target ({written_matched} \
       matched to Written, {written_unmatched} unmatched), {ram_only} RAM-only objects, {standard_streams} \
       standard streams, {unresolved} unresolved, {outside} outside — {}",
      if *outside == 0 {
        "zero violations"
      } else {
        "VIOLATIONS"
      }
    ),
  }
}

fn verdict(ok: bool) -> &'static str {
  if ok { "passed" } else { "FAILED" }
}

fn disabled(operations: &[String]) -> String {
  if operations.is_empty() {
    String::new()
  } else {
    format!(", disabled: {}", operations.join(","))
  }
}

fn workloads(tools: &[crate::record::WorkloadResult]) -> String {
  let mut identical = Vec::new();
  let mut differing = Vec::new();
  let mut skipped = Vec::new();
  for tool in tools {
    match &tool.status {
      WorkloadStatus::Identical => identical.push(tool.name.as_str()),
      WorkloadStatus::Differs { .. } => differing.push(tool.name.as_str()),
      WorkloadStatus::Skipped { tool: absent } => {
        skipped.push(format!("{} ({absent} absent)", tool.name))
      }
    }
  }
  let mut parts = vec![format!("identical: {}", list(&identical))];
  if !differing.is_empty() {
    parts.push(format!("DIFFERS: {}", differing.join(", ")));
  }
  if !skipped.is_empty() {
    parts.push(format!("skipped: {}", skipped.join(", ")));
  }
  parts.join("; ")
}

fn list(names: &[&str]) -> String {
  if names.is_empty() {
    "none".to_owned()
  } else {
    names.join(", ")
  }
}

/// The generated block of a document, between the markers, or why it could not be found.
pub fn block_of(document: &str) -> Result<(usize, usize), &'static str> {
  let start = document
    .find(BEGIN)
    .ok_or("the document has no conformance-matrix:begin marker")?
    + BEGIN.len();
  let end = document[start..]
    .find(END)
    .ok_or("the document has no conformance-matrix:end marker after the begin marker")?
    + start;
  Ok((start, end))
}

/// The document with its generated block replaced by `rendered`.
pub fn rewrite(document: &str, rendered: &str) -> Result<String, &'static str> {
  let (start, end) = block_of(document)?;
  Ok(format!(
    "{}{}{}",
    &document[..start],
    rendered,
    &document[end..]
  ))
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::record::{Host, Privilege, SCHEMA, SkipReason};

  fn record(transport: Transport, suite: Suite, outcome: Outcome) -> Record {
    Record {
      schema: SCHEMA,
      suite,
      transport,
      host: Host {
        os: "macOS 26.4.1".to_owned(),
        kernel: "Darwin 25.4.0".to_owned(),
        arch: "arm64".to_owned(),
        privilege: Privilege::Unprivileged,
      },
      date: "2026-09-14".to_owned(),
      command: "fsx -N 1000".to_owned(),
      bound: "1000 operations".to_owned(),
      outcome,
      expected_failure_list: None,
      duration_ms: 1,
      notes: vec!["a note".to_owned()],
    }
  }

  /// An empty record set renders every cell OWED and no commands.
  #[test]
  fn an_empty_set_is_all_owed() {
    let rendered = render(&[]);
    let owed = rendered.matches("OWED (no record)").count();
    assert_eq!(owed, Transport::ALL.len() * Suite::ALL.len());
    assert!(rendered.contains("(no run recorded yet)"));
  }

  /// A run renders as RAN with its counts, a skip as SKIPPED with its typed reason, a limited run
  /// with its adapter; the commands list carries the run's command and note.
  #[test]
  fn cells_render_their_outcome() {
    let ran = record(
      Transport::NativeMacosNfs,
      Suite::Fsx,
      Outcome::Ran {
        counts: Counts::Fsx {
          operations: 1000,
          seed: 1,
          file_length: 4096,
          ok: true,
        },
      },
    );
    let skipped = record(
      Transport::VirtioFs,
      Suite::Fsx,
      Outcome::Skipped {
        reason: SkipReason::Owed("no guest".to_owned()),
      },
    );
    let limited = record(
      Transport::NativeLinuxFuse,
      Suite::Fsx,
      Outcome::Limited {
        adapter: "root NFS".to_owned(),
        not_covered: "FUSE".to_owned(),
        counts: Counts::Fsx {
          operations: 5,
          seed: 2,
          file_length: 1,
          ok: false,
        },
      },
    );
    let rendered = render(&[ran, skipped, limited]);
    assert!(rendered.contains(
      "RAN(1000 operations, seed 1, file length 4096: passed; 2026-09-14; macOS 26.4.1)"
    ));
    assert!(rendered.contains("SKIPPED(owed: no guest)"));
    assert!(rendered.contains(
      "LIMITED(adapter: root NFS; not covered: FUSE; 5 operations, seed 2, file length 1: FAILED"
    ));
    assert!(rendered.contains("`fsx -N 1000`; bound: 1000 operations."));
    assert!(rendered.contains("  - a note"));
  }

  /// The block is found between the markers and rewritten in place; a document without markers
  /// is refused with a reason.
  #[test]
  fn the_block_is_rewritten_between_the_markers() {
    let document = format!("intro\n{BEGIN}old\n{END}\ntrailer\n");
    let rewritten = rewrite(&document, "new\n").unwrap();
    assert_eq!(rewritten, format!("intro\n{BEGIN}new\n{END}\ntrailer\n"));
    assert!(rewrite("no markers", "x").is_err());
    assert!(rewrite(&format!("{BEGIN}unterminated"), "x").is_err());
  }

  /// The workload summary names the identical, differing and skipped tools.
  #[test]
  fn the_workload_summary_names_every_tool() {
    use crate::record::WorkloadResult;
    let tools = vec![
      WorkloadResult {
        name: "git".to_owned(),
        status: WorkloadStatus::Identical,
      },
      WorkloadResult {
        name: "npm".to_owned(),
        status: WorkloadStatus::Differs {
          detail: "x".to_owned(),
        },
      },
      WorkloadResult {
        name: "watcher".to_owned(),
        status: WorkloadStatus::Skipped {
          tool: "fswatch".to_owned(),
        },
      },
    ];
    assert_eq!(
      summary(&Counts::Workloads { tools }),
      "identical: git; DIFFERS: npm; skipped: watcher (fswatch absent)"
    );
  }
}
