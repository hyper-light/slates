//! The session log as JavaScript objects (`Client.sessionLog`, A-773): the view `slates log --json` and the MCP tool
//! `slates.log` answer, in this SDK's camelCase keys, built here from the typed log since this addon links the client
//! alone. `needsYou` first (each with a `kind`: `"collided"`, `"awaiting_approval"` or `"awaiting_grant"`), then
//! `greens` (each `head` and its `works`: `base`, `behind`, `mark`) and `awaitingGrant`. A read: it shows a pending
//! grant or approval and makes none (R10).

use napi::bindgen_prelude::*;
use napi_derive::napi;
use slates_client::{LogWork, NeedsYou, SessionLog, VolumeId, WorkMark};

use super::{ConflictWindow, hex32, status_i64, volume_hex};

/// How a work's last submission ended: `state` is `"open"`, `"accepted"`, `"identical"`, `"collided"` or
/// `"awaiting_approval"`; the other fields are the state's own.
#[napi(object)]
pub struct LogMark {
  pub state: String,
  pub version: Option<i64>,
  pub verdict: Option<i64>,
  pub windows: Option<u32>,
  pub first: Option<ConflictWindow>,
  pub identity: Option<String>,
  pub paths: Option<i64>,
}

/// A work: where it started, how far its green moved past it, and its mark.
#[napi(object)]
pub struct LogWorkLine {
  pub id: String,
  pub name: String,
  pub base: i64,
  pub behind: i64,
  pub mark: LogMark,
}

/// A green, its head (null when the log could not read it) and its works.
#[napi(object)]
pub struct LogGreen {
  pub id: String,
  pub head: Option<i64>,
  pub works: Vec<LogWorkLine>,
}

/// One decision that needs a person: `kind` is `"collided"` (`work`, `name`, `green`, `path`, `windows`, `behind`),
/// `"awaiting_approval"` (`work`, `name`, `green`, `identity`, `paths`) or `"awaiting_grant"` (`landing`, `volume`,
/// `target`, `manifest`).
#[napi(object)]
pub struct LogNeed {
  pub kind: String,
  pub work: Option<String>,
  pub name: Option<String>,
  pub green: Option<String>,
  pub path: Option<String>,
  pub windows: Option<u32>,
  pub behind: Option<i64>,
  pub identity: Option<String>,
  pub paths: Option<i64>,
  pub landing: Option<i64>,
  pub volume: Option<String>,
  pub target: Option<String>,
  pub manifest: Option<String>,
}

/// A landing presented and awaiting its grant.
#[napi(object)]
pub struct LogLandingLine {
  pub landing: i64,
  pub volume: String,
  pub target: String,
  pub manifest: String,
}

/// The session log.
#[napi(object)]
pub struct SessionLogView {
  pub needs_you: Vec<LogNeed>,
  pub greens: Vec<LogGreen>,
  pub awaiting_grant: Vec<LogLandingLine>,
}

/// A need with every field absent but its kind.
fn need(kind: &str) -> LogNeed {
  LogNeed {
    kind: kind.to_owned(),
    work: None,
    name: None,
    green: None,
    path: None,
    windows: None,
    behind: None,
    identity: None,
    paths: None,
    landing: None,
    volume: None,
    target: None,
    manifest: None,
  }
}

/// A mark with every field absent but its state.
fn mark(state: &str) -> LogMark {
  LogMark {
    state: state.to_owned(),
    version: None,
    verdict: None,
    windows: None,
    first: None,
    identity: None,
    paths: None,
  }
}

fn mark_of(work_mark: &WorkMark) -> Result<LogMark> {
  Ok(match work_mark {
    WorkMark::Open => mark("open"),
    WorkMark::Accepted { version } => LogMark {
      version: Some(status_i64(*version, "version")?),
      ..mark("accepted")
    },
    WorkMark::Identical { version } => LogMark {
      version: Some(status_i64(*version, "version")?),
      ..mark("identical")
    },
    WorkMark::Collided {
      verdict,
      windows,
      first,
    } => LogMark {
      verdict: Some(status_i64(*verdict, "verdict")?),
      windows: Some(*windows),
      first: match first {
        Some(window) => Some(ConflictWindow {
          path: window.path.clone(),
          at: status_i64(window.at, "at")?,
          len: status_i64(window.len, "len")?,
          class: u32::from(window.class),
        }),
        None => None,
      },
      ..mark("collided")
    },
    WorkMark::AwaitingApproval { identity, paths } => LogMark {
      identity: Some(hex32(identity)),
      paths: Some(status_i64(*paths, "paths")?),
      ..mark("awaiting_approval")
    },
  })
}

fn work_of(log: &SessionLog, work: &LogWork) -> Result<LogWorkLine> {
  Ok(LogWorkLine {
    id: volume_hex(&work.id),
    name: work.name.clone(),
    base: status_i64(work.base, "base")?,
    behind: status_i64(log.behind(work), "behind")?,
    mark: mark_of(&work.mark)?,
  })
}

fn need_of(needs: &NeedsYou) -> Result<LogNeed> {
  Ok(match needs {
    NeedsYou::Collided {
      work,
      name,
      green,
      path,
      windows,
      behind,
    } => LogNeed {
      work: Some(volume_hex(work)),
      name: Some(name.clone()),
      green: Some(volume_hex(green)),
      path: Some(path.clone()),
      windows: Some(*windows),
      behind: Some(status_i64(*behind, "behind")?),
      ..need("collided")
    },
    NeedsYou::AwaitingApproval {
      work,
      name,
      green,
      identity,
      paths,
    } => LogNeed {
      work: Some(volume_hex(work)),
      name: Some(name.clone()),
      green: Some(volume_hex(green)),
      identity: Some(hex32(identity)),
      paths: Some(status_i64(*paths, "paths")?),
      ..need("awaiting_approval")
    },
    NeedsYou::AwaitingGrant {
      landing,
      volume,
      target,
      manifest,
    } => LogNeed {
      landing: Some(status_i64(*landing, "landing")?),
      volume: Some(volume_hex(volume)),
      target: Some(target.clone()),
      manifest: Some(hex32(manifest)),
      ..need("awaiting_grant")
    },
  })
}

/// The whole view.
pub(crate) fn view_of(log: &SessionLog) -> Result<SessionLogView> {
  let needs_you = log
    .needs_you()
    .iter()
    .map(need_of)
    .collect::<Result<Vec<_>>>()?;
  let mut greens_seen: Vec<VolumeId> = Vec::new();
  for work in &log.works {
    if !greens_seen.contains(&work.green) {
      greens_seen.push(work.green);
    }
  }
  let mut greens = Vec::with_capacity(greens_seen.len());
  for green in greens_seen {
    let works = log
      .works
      .iter()
      .filter(|work| work.green == green)
      .map(|work| work_of(log, work))
      .collect::<Result<Vec<_>>>()?;
    greens.push(LogGreen {
      id: volume_hex(&green),
      head: match log.head_of(green) {
        Some(head) => Some(status_i64(head, "head")?),
        None => None,
      },
      works,
    });
  }
  let awaiting_grant = log
    .landings
    .iter()
    .map(|landing| {
      Ok(LogLandingLine {
        landing: status_i64(landing.landing, "landing")?,
        volume: volume_hex(&landing.volume),
        target: landing.target.clone(),
        manifest: hex32(&landing.manifest),
      })
    })
    .collect::<Result<Vec<_>>>()?;
  Ok(SessionLogView {
    needs_you,
    greens,
    awaiting_grant,
  })
}
