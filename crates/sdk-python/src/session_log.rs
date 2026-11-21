//! The session log as a Python dict (`Client.session_log`, A-773): the keys and shapes `slates log --json` and the MCP
//! tool `slates.log` answer (`slates_mcp::session_log_json`), built here from the typed log, since this addon links the
//! client alone. `needs_you` (each with a `kind`: `"collided"`, `"awaiting_approval"` or `"awaiting_grant"`), `greens`
//! (each with its `head` and `works`: `base`, `behind`, `mark`), and `awaiting_grant`. A read: it shows a pending grant
//! or approval and makes none (R10).

use pyo3::prelude::*;
use pyo3::types::PyDict;
use slates_client::{LogWork, NeedsYou, SessionLog, VolumeId, WorkMark};

use super::{hex32, volume_hex};

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

/// A work's mark: `{"state": "open" | "accepted" | "identical" | "collided" | "awaiting_approval", ...}`.
fn mark_dict(py: Python<'_>, mark: &WorkMark) -> PyResult<Py<PyDict>> {
  let dict = PyDict::new_bound(py);
  match mark {
    WorkMark::Open => dict.set_item("state", "open")?,
    WorkMark::Accepted { version } => {
      dict.set_item("state", "accepted")?;
      dict.set_item("version", version)?;
    }
    WorkMark::Identical { version } => {
      dict.set_item("state", "identical")?;
      dict.set_item("version", version)?;
    }
    WorkMark::Collided {
      verdict,
      windows,
      first,
    } => {
      dict.set_item("state", "collided")?;
      dict.set_item("verdict", verdict)?;
      dict.set_item("windows", windows)?;
      match first {
        Some(window) => {
          let first = PyDict::new_bound(py);
          first.set_item("path", &window.path)?;
          first.set_item("at", window.at)?;
          first.set_item("len", window.len)?;
          first.set_item("class", window.class)?;
          dict.set_item("first", first)?;
        }
        None => dict.set_item("first", py.None())?,
      }
    }
    WorkMark::AwaitingApproval { identity, paths } => {
      dict.set_item("state", "awaiting_approval")?;
      dict.set_item("identity", hex32(identity))?;
      dict.set_item("paths", paths)?;
    }
  }
  Ok(dict.into())
}

/// A work: its id, name, base, how far behind its green it is, and its mark.
fn work_dict(py: Python<'_>, log: &SessionLog, work: &LogWork) -> PyResult<Py<PyDict>> {
  let dict = PyDict::new_bound(py);
  dict.set_item("id", volume_hex(&work.id))?;
  dict.set_item("name", &work.name)?;
  dict.set_item("base", work.base)?;
  dict.set_item("behind", log.behind(work))?;
  dict.set_item("mark", mark_dict(py, &work.mark)?)?;
  Ok(dict.into())
}

/// One decision that needs the human.
fn need_dict(py: Python<'_>, need: &NeedsYou) -> PyResult<Py<PyDict>> {
  let dict = PyDict::new_bound(py);
  match need {
    NeedsYou::Collided {
      work,
      name,
      green,
      path,
      windows,
      behind,
    } => {
      dict.set_item("kind", "collided")?;
      dict.set_item("work", volume_hex(work))?;
      dict.set_item("name", name)?;
      dict.set_item("green", volume_hex(green))?;
      dict.set_item("path", path)?;
      dict.set_item("windows", windows)?;
      dict.set_item("behind", behind)?;
    }
    NeedsYou::AwaitingApproval {
      work,
      name,
      green,
      identity,
      paths,
    } => {
      dict.set_item("kind", "awaiting_approval")?;
      dict.set_item("work", volume_hex(work))?;
      dict.set_item("name", name)?;
      dict.set_item("green", volume_hex(green))?;
      dict.set_item("identity", hex32(identity))?;
      dict.set_item("paths", paths)?;
    }
    NeedsYou::AwaitingGrant {
      landing,
      volume,
      target,
      manifest,
    } => {
      dict.set_item("kind", "awaiting_grant")?;
      dict.set_item("landing", landing)?;
      dict.set_item("volume", volume_hex(volume))?;
      dict.set_item("target", target)?;
      dict.set_item("manifest", hex32(manifest))?;
    }
  }
  Ok(dict.into())
}

/// The whole view.
pub(crate) fn session_log_dict(py: Python<'_>, log: &SessionLog) -> PyResult<Py<PyDict>> {
  let dict = PyDict::new_bound(py);
  let needs = log
    .needs_you()
    .iter()
    .map(|need| need_dict(py, need))
    .collect::<PyResult<Vec<_>>>()?;
  dict.set_item("needs_you", needs)?;
  let mut greens = Vec::new();
  for green in greens_of(log) {
    let entry = PyDict::new_bound(py);
    entry.set_item("id", volume_hex(&green))?;
    entry.set_item("head", log.head_of(green))?;
    let works = log
      .works
      .iter()
      .filter(|work| work.green == green)
      .map(|work| work_dict(py, log, work))
      .collect::<PyResult<Vec<_>>>()?;
    entry.set_item("works", works)?;
    greens.push(entry);
  }
  dict.set_item("greens", greens)?;
  let mut waiting = Vec::new();
  for landing in &log.landings {
    let entry = PyDict::new_bound(py);
    entry.set_item("landing", landing.landing)?;
    entry.set_item("volume", volume_hex(&landing.volume))?;
    entry.set_item("target", &landing.target)?;
    entry.set_item("manifest", hex32(&landing.manifest))?;
    waiting.push(entry);
  }
  dict.set_item("awaiting_grant", waiting)?;
  Ok(dict.into())
}
