//! The daemon's watch on its anchor: the anchor's process id arrives in the environment
//! (`ENV_ANCHOR_PID`); a daemon whose anchor died leaves, so no segment is served without a
//! supervisor. Linux also asks the kernel for the parent-death signal, which needs no
//! polling; Windows relies on the supervisor's job object, which kills the daemon with the
//! anchor's last handle. Paired `#[cfg]` functions, one signature.

/// The watch.
#[derive(Debug)]
pub(crate) struct ParentWatch {
  /// The anchor's pid, when the daemon is its child.
  anchor: Option<u32>,
}

impl ParentWatch {
  /// From the environment; asks for the parent-death signal where the kernel offers it.
  pub(crate) fn from_env() -> ParentWatch {
    let anchor = std::env::var(slates_anchor::supervise::ENV_ANCHOR_PID)
      .ok()
      .and_then(|v| v.parse::<u32>().ok());
    if anchor.is_some() {
      request_death_signal();
    }
    ParentWatch { anchor }
  }

  /// Whether the daemon is supervised at all.
  pub(crate) fn supervised(&self) -> bool {
    self.anchor.is_some()
  }

  /// Whether the anchor is still this process's parent (true for an unsupervised daemon).
  pub(crate) fn anchor_alive(&self) -> bool {
    match self.anchor {
      Some(pid) => parent_is(pid),
      None => true,
    }
  }
}

#[cfg(target_os = "linux")]
fn request_death_signal() {
  // Best effort: a refusal leaves the pid watch, which is enough.
  let _ = rustix::process::set_parent_process_death_signal(Some(rustix::process::Signal::KILL));
}

#[cfg(not(target_os = "linux"))]
fn request_death_signal() {}

#[cfg(unix)]
fn parent_is(pid: u32) -> bool {
  rustix::process::getppid()
    .is_some_and(|p| u32::try_from(p.as_raw_nonzero().get()).ok() == Some(pid))
}

#[cfg(not(unix))]
fn parent_is(_pid: u32) -> bool {
  true
}
