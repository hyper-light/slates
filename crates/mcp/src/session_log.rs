//! The session log as JSON (`slates log --json`, the MCP tool `slates.log`, the SDKs' `session_log`; A-773, A-782,
//! A-785): one definition of the shape, so every surface agrees. Keys and shapes are stable: `needs_you` (each with a
//! `kind`: `collided`, `awaiting_approval` or `awaiting_grant`), `greens` (each with its head and works: `base`,
//! `behind`, `mark`), and `awaiting_grant`. A read: it shows a pending grant or approval and never makes one (R10).

use serde_json::{Value, json};
use slates_client::{LogWork, NeedsYou, SessionLog, VolumeId, WorkMark};

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

/// A work's mark as JSON: `{"state": "open" | "accepted" | "identical" | "collided" | "awaiting_approval", ...}`.
fn mark_json(mark: &WorkMark) -> Value {
  match mark {
    WorkMark::Open => json!({ "state": "open" }),
    WorkMark::Accepted { version } => {
      json!({ "state": "accepted", "version": version })
    }
    WorkMark::Identical { version } => {
      json!({ "state": "identical", "version": version })
    }
    WorkMark::AwaitingApproval { identity, paths } => json!({
      "state": "awaiting_approval",
      "identity": super::hex32(identity),
      "paths": paths,
    }),
    WorkMark::Collided {
      verdict,
      windows,
      first,
    } => json!({
      "state": "collided",
      "verdict": verdict,
      "windows": windows,
      "first": first.as_ref().map(|w| json!({ "path": w.path, "at": w.at, "len": w.len, "class": w.class })),
    }),
  }
}

/// A work as JSON.
fn work_json(log: &SessionLog, work: &LogWork) -> Value {
  json!({
    "id": super::id_hex(work.id),
    "name": work.name,
    "base": work.base,
    "behind": log.behind(work),
    "mark": mark_json(&work.mark),
  })
}

/// The view as one JSON object: `needs_you` (each `{"kind": "collided" | "awaiting_grant", ...}`), `greens` (each with
/// its head and works) and `awaiting_grant`.
pub fn session_log_json(log: &SessionLog) -> Value {
  let needs: Vec<Value> = log
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
      } => json!({
        "kind": "collided",
        "work": super::id_hex(*work),
        "name": name,
        "green": super::id_hex(*green),
        "path": path,
        "windows": windows,
        "behind": behind,
      }),
      NeedsYou::AwaitingApproval {
        work,
        name,
        green,
        identity,
        paths,
      } => json!({
        "kind": "awaiting_approval",
        "work": super::id_hex(*work),
        "name": name,
        "green": super::id_hex(*green),
        "identity": super::hex32(identity),
        "paths": paths,
      }),
      NeedsYou::AwaitingGrant {
        landing,
        volume,
        target,
        manifest,
      } => json!({
        "kind": "awaiting_grant",
        "landing": landing,
        "volume": super::id_hex(*volume),
        "target": target,
        "manifest": super::hex32(manifest),
      }),
    })
    .collect();
  let greens: Vec<Value> = greens_of(log)
    .into_iter()
    .map(|green| {
      json!({
        "id": super::id_hex(green),
        "head": log.head_of(green),
        "works": log.works.iter().filter(|w| w.green == green).map(|w| work_json(log, w)).collect::<Vec<_>>(),
      })
    })
    .collect();
  let waiting: Vec<Value> = log
    .landings
    .iter()
    .map(|landing| {
      json!({
        "landing": landing.landing,
        "volume": super::id_hex(landing.volume),
        "target": landing.target,
        "manifest": super::hex32(&landing.manifest),
      })
    })
    .collect();
  json!({ "needs_you": needs, "greens": greens, "awaiting_grant": waiting })
}
