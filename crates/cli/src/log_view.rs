//! `slates log` (A-773): the session log for a person supervising many agents, in the spirit of Sapling's Smartlog.
//! What needs the human comes first, alone and plain: a collided work (where, and how far its green moved past it) and a
//! landing awaiting its grant (its manifest, and the `grant` command that would approve it, which this view never runs:
//! R10). Then each green with its head and one line a work: where it started, how far behind it is, and how its last
//! submission ended. Nothing else: no volume sizes, no attachments, no audit.
//!
//! `--json` is the same view as one object whose keys and shapes are stable (`needs_you`, `greens`, `awaiting_grant`),
//! so a harness reads the decision list without scraping text.

use slates_client::{LogWork, NeedsYou, SessionLog, VolumeId, WorkMark};

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

/// The view as text lines.
pub(crate) fn text_lines(log: &SessionLog) -> Vec<String> {
  let mut lines = Vec::new();
  let needs = log.needs_you();
  if needs.is_empty() {
    lines.push("needs you: nothing".to_owned());
  } else {
    lines.push(format!("needs you ({}):", needs.len()));
    for need in &needs {
      lines.push(match need {
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
      });
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
        if matches!(work.mark, WorkMark::Collided { .. }) {
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

/// A work's mark as JSON: `{"state": "open" | "accepted" | "collided", ...}`.
fn mark_json(mark: &WorkMark) -> serde_json::Value {
  match mark {
    WorkMark::Open => serde_json::json!({ "state": "open" }),
    WorkMark::Accepted { version } => {
      serde_json::json!({ "state": "accepted", "version": version })
    }
    WorkMark::Collided {
      verdict,
      windows,
      first,
    } => serde_json::json!({
      "state": "collided",
      "verdict": verdict,
      "windows": windows,
      "first": first.as_ref().map(|w| serde_json::json!({ "path": w.path, "at": w.at, "len": w.len, "class": w.class })),
    }),
  }
}

/// A work as JSON.
fn work_json(log: &SessionLog, work: &LogWork) -> serde_json::Value {
  serde_json::json!({
    "id": volume_id_text(work.id),
    "name": work.name,
    "base": work.base,
    "behind": log.behind(work),
    "mark": mark_json(&work.mark),
  })
}

/// The view as one JSON object: `needs_you` (each `{"kind": "collided" | "awaiting_grant", ...}`), `greens` (each with
/// its head and works) and `awaiting_grant`.
pub(crate) fn json(log: &SessionLog) -> serde_json::Value {
  let needs: Vec<serde_json::Value> = log
    .needs_you()
    .iter()
    .map(|need| match need {
      NeedsYou::Collided {
        work,
        name,
        green,
        path,
        windows,
        behind,
      } => serde_json::json!({
        "kind": "collided",
        "work": volume_id_text(*work),
        "name": name,
        "green": volume_id_text(*green),
        "path": path,
        "windows": windows,
        "behind": behind,
      }),
      NeedsYou::AwaitingGrant {
        landing,
        volume,
        target,
        manifest,
      } => serde_json::json!({
        "kind": "awaiting_grant",
        "landing": landing,
        "volume": volume_id_text(*volume),
        "target": target,
        "manifest": manifest_text(manifest),
      }),
    })
    .collect();
  let greens: Vec<serde_json::Value> = greens_of(log)
    .into_iter()
    .map(|green| {
      serde_json::json!({
        "id": volume_id_text(green),
        "head": log.head_of(green),
        "works": log.works.iter().filter(|w| w.green == green).map(|w| work_json(log, w)).collect::<Vec<_>>(),
      })
    })
    .collect();
  let waiting: Vec<serde_json::Value> = log
    .landings
    .iter()
    .map(|landing| {
      serde_json::json!({
        "landing": landing.landing,
        "volume": volume_id_text(landing.volume),
        "target": landing.target,
        "manifest": manifest_text(&landing.manifest),
      })
    })
    .collect();
  serde_json::json!({ "needs_you": needs, "greens": greens, "awaiting_grant": waiting })
}

#[cfg(test)]
mod tests {
  #![allow(clippy::unwrap_used, clippy::indexing_slicing)]

  use slates_client::LogLanding;
  use slates_ipc::protocol::MergeWindow;

  use super::*;

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
}
