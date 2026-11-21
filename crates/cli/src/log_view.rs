//! `slates log` (A-773): the session log for a person supervising many agents, in the spirit of Sapling's Smartlog.
//! What needs the human comes first, alone and plain: a collided work (where, and how far its green moved past it) and a
//! landing awaiting its grant (its manifest, and the `grant` command that would approve it, which this view never runs:
//! R10). Then each green with its head and one line a work: where it started, how far behind it is, and how its last
//! submission ended. Nothing else: no volume sizes, no attachments, no audit.
//!
//! `--json` is the same view as one object whose keys and shapes are stable (`needs_you`, `greens`, `awaiting_grant`),
//! so a harness reads the decision list without scraping text.

use slates_client::{NeedsYou, SessionLog, VolumeId, WorkMark};

use crate::format::volume_id_text;

/// The manifest as 64 hexadecimal digits.
fn manifest_text(manifest: &[u8; 32]) -> String {
  manifest.iter().map(|b| format!("{b:02x}")).collect()
}

/// The greens the works are over, each once, in the order their first work appears.
fn greens_of(log: &SessionLog) -> Vec<VolumeId> {
  let mut greens: Vec<VolumeId> = Vec::new();
  for work in &log.works {
    if !greens.contains(&work.green) {
      greens.push(work.green);
    }
  }
  greens
}

/// A work's mark in words.
fn mark_text(mark: &WorkMark) -> String {
  match mark {
    WorkMark::Open => "open".to_owned(),
    WorkMark::Accepted { version } => format!("accepted as v{version}"),
    WorkMark::Identical { version } => format!("identical, as v{version} (nothing changed)"),
    WorkMark::AwaitingApproval { identity, paths } => format!(
      "HELD for approval: increment {} ({paths} path{})",
      manifest_text(identity),
      if *paths == 1 { "" } else { "s" }
    ),
    WorkMark::Collided { windows, first, .. } => match first {
      Some(window) => format!(
        "COLLIDED at {} [{}+{}] ({windows} window{})",
        window.path,
        window.at,
        window.len,
        if *windows == 1 { "" } else { "s" }
      ),
      None => format!("COLLIDED ({windows} windows)"),
    },
  }
}

/// One item that needs the human, as its text line.
fn need_line(need: &NeedsYou) -> String {
  match need {
    NeedsYou::Collided {
      name,
      path,
      windows,
      behind,
      ..
    } => format!(
      "  ! {name}: collided at {path} ({windows} window{}); its green moved {behind} version{} past it",
      if *windows == 1 { "" } else { "s" },
      if *behind == 1 { "" } else { "s" }
    ),
    NeedsYou::AwaitingApproval {
      name,
      identity,
      paths,
      ..
    } => format!(
      "  ! {name}: merge held for approval: increment {} ({paths} path{})",
      manifest_text(identity),
      if *paths == 1 { "" } else { "s" }
    ),
    NeedsYou::AwaitingGrant {
      landing,
      volume,
      target,
      manifest,
    } => format!(
      "  ! landing {landing} of {} into {target} awaits a grant: slates grant {landing} {}",
      volume_id_text(*volume),
      manifest_text(manifest)
    ),
  }
}

/// The view as text lines.
pub(crate) fn text_lines(log: &SessionLog) -> Vec<String> {
  let mut lines = Vec::new();
  let needs = log.needs_you();
  if needs.is_empty() {
    lines.push("needs you: nothing".to_owned());
  } else {
    lines.push(format!("needs you ({}):", needs.len()));
    for need in &needs {
      lines.push(need_line(need));
    }
  }
  for green in greens_of(log) {
    let head = log
      .head_of(green)
      .map_or_else(|| "unknown".to_owned(), |head| format!("v{head}"));
    lines.push(format!("green {} at {head}", volume_id_text(green)));
    for work in log.works.iter().filter(|work| work.green == green) {
      lines.push(format!(
        "  {} {}  from v{}, {} behind  {}",
        if matches!(
          work.mark,
          WorkMark::Collided { .. } | WorkMark::AwaitingApproval { .. }
        ) {
          '!'
        } else {
          'o'
        },
        work.name,
        work.base,
        log.behind(work),
        mark_text(&work.mark)
      ));
    }
  }
  lines
}

/// The view as one JSON object (`slates_mcp::session_log_json`): the MCP tool `slates.log` and the SDKs answer the same
/// shape, one definition (§4.12 schema parity).
pub(crate) fn json(log: &SessionLog) -> serde_json::Value {
  slates_mcp::session_log_json(log)
}

#[cfg(test)]
mod tests {
  #![allow(clippy::unwrap_used, clippy::indexing_slicing)]

  use slates_client::LogLanding;
  use slates_ipc::protocol::MergeWindow;

  use super::*;
  use slates_client::LogWork;

  fn id(n: u8) -> VolumeId {
    VolumeId { bytes: [n; 16] }
  }

  /// Ten works over one green at v10: eight accepted, one open at v1, one collided at v1; one landing waiting.
  fn ten_works() -> SessionLog {
    let mut works: Vec<LogWork> = (1..=8u8)
      .map(|n| LogWork {
        id: id(n),
        name: format!("agent-{n}"),
        green: id(0),
        base: u64::from(n) + 1,
        mark: WorkMark::Accepted {
          version: u64::from(n) + 1,
        },
      })
      .collect();
    works.push(LogWork {
      id: id(9),
      name: "agent-9".to_owned(),
      green: id(0),
      base: 1,
      mark: WorkMark::Open,
    });
    works.push(LogWork {
      id: id(10),
      name: "agent-10".to_owned(),
      green: id(0),
      base: 1,
      mark: WorkMark::Collided {
        verdict: 5,
        windows: 1,
        first: Some(MergeWindow {
          path: "shared.txt".to_owned(),
          at: 0,
          len: 4,
          class: 0,
        }),
      },
    });
    SessionLog {
      works,
      landings: vec![LogLanding {
        landing: 3,
        volume: id(20),
        target: "/srv/out".to_owned(),
        manifest: [0xab; 32],
      }],
      heads: [([0; 16], 10)].into_iter().collect(),
    }
  }

  /// A-773. Do: render ten works with one collision and one waiting landing as text. Expect: the needs-you block names
  /// exactly those two, the collision with its path and how far its green moved, the landing with the grant command;
  /// then the green at its head and one line a work.
  #[test]
  fn the_text_view_names_exactly_the_two_decisions_first() {
    let lines = text_lines(&ten_works());
    assert_eq!(lines[0], "needs you (2):");
    assert_eq!(
      lines[1],
      "  ! agent-10: collided at shared.txt (1 window); its green moved 9 versions past it"
    );
    assert!(
      lines[2].contains("landing 3")
        && lines[2].contains(&format!("slates grant 3 {}", "ab".repeat(32))),
      "{}",
      lines[2]
    );
    assert_eq!(lines[3], format!("green {} at v10", "00".repeat(16)));
    assert_eq!(lines.len(), 4 + 10, "one line a work: {lines:#?}");
    assert_eq!(
      lines
        .iter()
        .filter(|l| l.trim_start().starts_with('!'))
        .count(),
      3
    );
  }

  /// The JSON view's `needs_you`: exactly the collision on `shared.txt`, nine versions behind, then landing 3 awaiting
  /// its grant by manifest.
  fn json_needs_the_two(value: &serde_json::Value) {
    let needs = value["needs_you"].as_array().unwrap();
    assert_eq!(needs.len(), 2);
    assert_eq!(needs[0]["kind"], "collided");
    assert_eq!(needs[0]["path"], "shared.txt");
    assert_eq!(needs[0]["behind"], 9);
    assert_eq!(needs[1]["kind"], "awaiting_grant");
    assert_eq!(needs[1]["landing"], 3);
    assert_eq!(needs[1]["manifest"], "ab".repeat(32));
  }

  /// A-773. Do: render the same log as JSON. Expect: `needs_you` holds exactly the collision and the waiting landing,
  /// each typed by `kind`, and the green carries its head and all ten works with base, behind and mark.
  #[test]
  fn the_json_view_is_stable_and_names_the_same_two() {
    let value = json(&ten_works());
    json_needs_the_two(&value);
    let green = &value["greens"][0];
    assert_eq!(green["head"], 10);
    let works = green["works"].as_array().unwrap();
    assert_eq!(works.len(), 10);
    assert_eq!(works[8]["mark"]["state"], "open");
    assert_eq!(works[8]["behind"], 9);
    assert_eq!(
      works[0]["mark"],
      serde_json::json!({ "state": "accepted", "version": 2 })
    );
    assert_eq!(value["awaiting_grant"][0]["target"], "/srv/out");
  }

  /// A-785. Do: render a log whose only notable work's merge an ask policy holds. Expect: the needs-you block names it
  /// with the increment's identity and paths, its line is marked, and the JSON's `needs_you` holds one
  /// `awaiting_approval`.
  #[test]
  fn a_held_merge_is_named_as_needing_the_human() {
    let log = SessionLog {
      works: vec![LogWork {
        id: id(1),
        name: "asking-agent".to_owned(),
        green: id(0),
        base: 0,
        mark: WorkMark::AwaitingApproval {
          identity: [0xcd; 32],
          paths: 2,
        },
      }],
      landings: Vec::new(),
      heads: [([0; 16], 0)].into_iter().collect(),
    };
    let lines = text_lines(&log);
    assert_eq!(lines[0], "needs you (1):");
    assert_eq!(
      lines[1],
      format!(
        "  ! asking-agent: merge held for approval: increment {} (2 paths)",
        "cd".repeat(32)
      )
    );
    assert!(
      lines[3].trim_start().starts_with("! asking-agent"),
      "{}",
      lines[3]
    );
    let value = json(&log);
    assert_eq!(value["needs_you"][0]["kind"], "awaiting_approval");
    assert_eq!(value["needs_you"][0]["paths"], 2);
    assert_eq!(
      value["greens"][0]["works"][0]["mark"]["state"],
      "awaiting_approval"
    );
  }
}
