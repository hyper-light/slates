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

/// What the anchor handed this daemon: nothing (run alone), its profile, or a profile written in another
/// format version — an anchor that has run across an upgrade of the binary it spawns (§2.6).
enum Published {
  /// No anchor: this daemon runs alone and creates its own segment.
  Alone,
  /// The anchor's profile, in this build's format (boxed: a profile is kilobytes, the other answers a word).
  Profile(Box<MachineProfile>),
  /// The anchor's profile is in another format version: a field's meaning changed between the two, so the
  /// daemon measures its own rather than derive from the wrong quantity, and still serves the anchor's
  /// segment.
  OtherFormat(u32),
}

/// The profile the anchor published, when this daemon is its child.
fn published_profile() -> Result<Published, Failure> {
  if std::env::var_os(slates_anchor::segment::ENV_HANDOFF).is_none() {
    return Ok(Published::Alone);
  }
  let identity = Facts::query().identity;
  let segment = AnchorSegment::attach_from_env(&identity).map_err(|e| failed("attach", e))?;
  let json = segment
    .read_published(RegionKind::Profile)
    .map_err(|e| failed("profile", e))?
    .ok_or_else(|| Failure::Failed("the anchor published no profile".to_owned()))?;
  let text = String::from_utf8(json).map_err(|e| failed("profile", e))?;
  match MachineProfile::format_version(&text) {
    Some(version) if version != slates_machine::profile::PROFILE_VERSION => {
      Ok(Published::OtherFormat(version))
    }
    _ => MachineProfile::from_json(&text)
      .map(|profile| Published::Profile(Box::new(profile)))
      .map_err(|e| failed("profile", e)),
  }
}

/// Runs the daemon.
pub(crate) fn run(options: &ProcessOptions) -> Result<(), Failure> {
  signal::install().map_err(Failure::Failed)?;
  let parent = ParentWatch::from_env();
  let (profile, source) = match published_profile()? {
    Published::Profile(profile) => (*profile, SegmentSource::FromEnv),
    Published::OtherFormat(version) => {
      eprintln!(
        "slates daemon: the anchor's profile is format version {version}; this daemon reads version {}: measuring its own",
        slates_machine::profile::PROFILE_VERSION
      );
      (measure(options.quick), SegmentSource::FromEnv)
    }
    Published::Alone => (
      measure(options.quick),
      SegmentSource::Create {
        name: segment_name(&options.instance),
      },
    ),
  };
  let mut config = DaemonConfig::derive(&profile, &options.instance, options.shards);
  config.recovery_key = crate::recovery_key::load()?;
  // A fleet node (§2.6 boot step 6): the shared manifest gives the membership the placement authority is
  // built over and the transport the membership loop drives; a laptop passes neither and runs the same
  // placement path, degenerate (R8).
  let transport = match &options.fleet {
    Some(selection) => {
      let plan = crate::fleet::load(selection)?;
      eprintln!(
        "slates daemon: fleet node `{}` of `{}`: member {} with {} peer(s) at f = {}",
        selection.node,
        plan.transport.name,
        plan.membership.host.0,
        plan.membership.peers.len(),
        plan.membership.quorum.f
      );
      config = config.with_fleet(plan.membership);
      Some(plan.transport)
    }
    None => None,
  };
  for line in &config.derivations {
    eprintln!("slates daemon: {line}");
  }
  let daemon = Daemon::start_with_fleet(&profile, config, source, transport)
    .map_err(|e| failed("start", e))?;
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
