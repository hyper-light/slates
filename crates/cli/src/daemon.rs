//! `slates daemon` (§2.6 steps 2, 3 and 5): attach the anchor's segment from the environment
//! and take the profile it published, or, run alone, measure a profile and create a segment;
//! derive the configuration; serve until a stop is requested or the anchor is gone.

use std::time::Duration;

use slates_anchor::{AnchorSegment, RegionKind};
use slates_machine::facts::Facts;
use slates_machine::{MachineProfile, ProfileOptions};
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

/// Shape: the per-probe budgets a **quick** measurement tries in turn — the quick bound, then the full
/// profile's own. Both already exist and neither is invented here; what is new is trying the second.
const QUICK_BUDGETS: [Duration; 2] = [
  Duration::from_millis(QUICK_PROBE_MS),
  slates_machine::bench::PROBE_WALL_BUDGET,
];

/// Measures the profile: full, or quick. A probe that could not measure at all within its bound is the
/// failure, named (`MeasurementTimeout`), never a profile carrying a fabricated value.
///
/// **A quick measurement escalates its bound before it gives up.** The quick bound is five milliseconds, and
/// a machine busy enough cannot answer the wake probe inside it — measured on 2026-10-18, where a full
/// `cargo test --workspace` refused this very call twice while the same code passed alone. That matters past
/// the tests: `serve_mcp_http` measures this way, so an MCP server could refuse to start because the machine
/// was busy, on a product whose own standing rule is that the busy machine is the norm. So a refused quick
/// probe is retried at the full profile's per-probe bound before the refusal stands: one path with an
/// escalating bound, not a second path, and both bounds are ones the profile already defines.
pub(crate) fn measure(quick: bool) -> Result<MachineProfile, Failure> {
  let measured = if quick {
    MachineProfile::measure_within(
      &ProfileOptions {
        budget_per_probe: Duration::from_millis(QUICK_PROBE_MS),
        codecs: false,
        core_matrix: false,
      },
      &QUICK_BUDGETS,
    )
  } else {
    MachineProfile::measure(ProfileOptions::default())
  };
  measured.map_err(|refusal| failed("measure the machine profile", refusal))
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
  // Before a volume holds a byte: no core dump may carry it (AUD-29-41).
  crate::dumps::exclude_from_dumps()?;
  signal::install().map_err(Failure::Failed)?;
  let parent = ParentWatch::from_env();
  let (profile, source) = match published_profile()? {
    Published::Profile(profile) => (*profile, SegmentSource::FromEnv),
    Published::OtherFormat(version) => {
      eprintln!(
        "slates daemon: the anchor's profile is format version {version}; this daemon reads version {}: measuring its own",
        slates_machine::profile::PROFILE_VERSION
      );
      (measure(options.quick)?, SegmentSource::FromEnv)
    }
    Published::Alone => (
      measure(options.quick)?,
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
      // The serve sockets are bound here, before the daemon starts: an address in use refuses the start
      // by name instead of leaving a daemon that can never be probed.
      Some(
        plan
          .transport
          .bind()
          .map_err(|e| failed("fleet serve", e))?,
      )
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
  // The process thread waits for a reason to stop without polling: the control shard's heartbeat asks the anchor's
  // stop request and these two at its own cadence and wakes this thread once one holds.
  let watch = parent;
  let waited = daemon.wait_for_stop(move || signal::stop_requested() || !watch.anchor_alive());
  if let Err(refusal) = waited {
    eprintln!("slates daemon: the stop watch could not be installed ({refusal}); stopping");
  } else if signal::stop_requested() || daemon.stop_requested() {
    // A planned stop: hand off any consensus leadership first, so the groups pay a handoff rather than the
    // election timeout a leader loss waits out (bounded; the anchor holds the daemon to its declaration).
    match daemon.drain_leadership() {
      Ok(report) => eprintln!("slates daemon: stop requested; drained: {report:?}"),
      Err(refusal) => eprintln!("slates daemon: stop requested; drain refused: {refusal}"),
    }
  } else if !parent.anchor_alive() {
    eprintln!("slates daemon: the anchor is gone; leaving");
  } else {
    eprintln!("slates daemon: the control shard's watch ended; stopping");
  }
  daemon.stop();
  Ok(())
}

#[cfg(test)]
mod budget_tests {
  //! The quick measurement's budget escalation (§4.1): a busy machine must not stop the CLI starting.
  #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

  use std::time::Duration;

  use slates_machine::{MachineProfile, ProfileOptions};

  use super::QUICK_BUDGETS;

  /// The quick profile's scope, as `measure` uses it: the budgets are the argument.
  fn scope() -> ProfileOptions {
    ProfileOptions {
      budget_per_probe: Duration::ZERO,
      codecs: false,
      core_matrix: false,
    }
  }

  fn measure_within(budgets: &[Duration]) -> Result<MachineProfile, slates_machine::MachineError> {
    MachineProfile::measure_within(&scope(), budgets)
  }

  /// A zero budget cannot measure the wake probe, so it is the deterministic refusal this file's other
  /// assertions stand on — `MachineProfile::measure` is refused `MeasurementTimeout`, never a fabricated
  /// profile (`crates/machine/src/profile.rs` asserts the same of `refresh_cheap(Duration::ZERO)`).
  #[test]
  fn a_zero_budget_is_refused_so_the_escalation_test_below_is_not_vacuous() {
    assert!(
      measure_within(&[Duration::ZERO]).is_err(),
      "a zero-budget probe must be refused, or the escalation test proves nothing"
    );
  }

  /// §4.1 — a quick measurement escalates its bound rather than giving up. Do: ask for a budget that
  /// cannot work followed by one that can. Expect: a profile, because the second was tried. This is the
  /// case a loaded machine produces, where the first bound is refused and the CLI must still start —
  /// measured 2026-10-18, when a full `cargo test --workspace` refused the quick probe twice.
  #[test]
  fn a_refused_budget_escalates_to_the_next_one() {
    let measured = measure_within(&[Duration::ZERO, Duration::from_millis(250)]);
    assert!(
      measured.is_ok(),
      "the escalation must reach the second budget: {measured:?}"
    );
  }

  /// The shipped order escalates: the quick bound first, then a strictly larger one. A single-budget list
  /// would make the escalation above unreachable in production however well it tests.
  #[test]
  fn the_shipped_budgets_escalate() {
    assert!(
      QUICK_BUDGETS.len() >= 2 && QUICK_BUDGETS[1] > QUICK_BUDGETS[0],
      "the shipped budgets must grow: {QUICK_BUDGETS:?}"
    );
  }
}
