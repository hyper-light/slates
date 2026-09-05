//! `slates daemon` (§2.6 steps 2, 3 and 5): attach the anchor's segment from the environment
//! and take the profile it published, or, run alone, measure a profile and create a segment;
//! derive the configuration; serve until a stop is requested or the anchor is gone.

use std::time::Duration;

use slates_anchor::{AnchorSegment, RegionKind};
use slates_machine::facts::Facts;
use slates_machine::{MachineProfile, ProfileOptions};
use slates_server::daemon::HEARTBEAT_NS;
use slates_server::{Daemon, DaemonConfig, SegmentSource};

use crate::args::ProcessOptions;
use crate::parent::ParentWatch;
use crate::{Failure, signal};

/// Shape: the probe budget of the quick profile (milliseconds): what the server's own tests
/// measure with; the numbers are inputs to derivations, and a quick profile is marked so.
const QUICK_PROBE_MS: u64 = 5;

/// The segment's object name for an instance.
pub(crate) fn segment_name(instance: &str) -> String {
  let clean: String = instance
    .chars()
    .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
    .collect();
  format!("slates-seg-{clean}")
}

/// Measures the profile: full, or quick.
pub(crate) fn measure(quick: bool) -> MachineProfile {
  if quick {
    MachineProfile::measure(ProfileOptions {
      budget_per_probe: Duration::from_millis(QUICK_PROBE_MS),
      codecs: false,
      core_matrix: false,
    })
  } else {
    MachineProfile::measure(ProfileOptions::default())
  }
}

fn failed(what: &str, e: impl std::fmt::Display) -> Failure {
  Failure::Failed(format!("{what}: {e}"))
}

/// The profile the anchor published, when this daemon is its child.
fn published_profile() -> Result<Option<MachineProfile>, Failure> {
  if std::env::var_os(slates_anchor::segment::ENV_HANDOFF).is_none() {
    return Ok(None);
  }
  let identity = Facts::query().identity;
  let segment = AnchorSegment::attach_from_env(&identity).map_err(|e| failed("attach", e))?;
  let json = segment
    .read_published(RegionKind::Profile)
    .map_err(|e| failed("profile", e))?
    .ok_or_else(|| Failure::Failed("the anchor published no profile".to_owned()))?;
  let text = String::from_utf8(json).map_err(|e| failed("profile", e))?;
  MachineProfile::from_json(&text)
    .map(Some)
    .map_err(|e| failed("profile", e))
}

/// Runs the daemon.
pub(crate) fn run(options: &ProcessOptions) -> Result<(), Failure> {
  signal::install().map_err(Failure::Failed)?;
  let parent = ParentWatch::from_env();
  let (profile, source) = match published_profile()? {
    Some(profile) => (profile, SegmentSource::FromEnv),
    None => (
      measure(options.quick),
      SegmentSource::Create {
        name: segment_name(&options.instance),
      },
    ),
  };
  let mut config = DaemonConfig::derive(&profile, &options.instance);
  if let Some(shards) = options.shards {
    config = config.with_shards(shards);
  }
  for line in &config.derivations {
    eprintln!("slates daemon: {line}");
  }
  let daemon = Daemon::start(&profile, config, source).map_err(|e| failed("start", e))?;
  eprintln!(
    "slates daemon: instance {} serving on {} shards{}",
    options.instance,
    daemon.shards().len(),
    if parent.supervised() {
      " under the anchor"
    } else {
      " alone"
    }
  );
  loop {
    if signal::stop_requested() {
      eprintln!("slates daemon: stop requested");
      break;
    }
    if !parent.anchor_alive() {
      eprintln!("slates daemon: the anchor is gone; leaving");
      break;
    }
    std::thread::park_timeout(Duration::from_nanos(HEARTBEAT_NS));
  }
  daemon.stop();
  Ok(())
}
