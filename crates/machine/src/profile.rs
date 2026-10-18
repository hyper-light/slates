//! The machine profile: every fixed fact and every measurement, with a JSON export that carries
//! the intervals, and the derived-constants table of §4.1 with each constant's formula and
//! anchors.
//!
//! The profile is measurements only; the derived constants are recomputed from it on every load,
//! so a cached profile never carries a stale derivation. A profile records `quick: true` when any
//! probe stopped at its wall bound, and the notes say which facts the OS refused.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::bench::{Measurement, PROBE_WALL_BUDGET, TIMER_OVERHEAD_FACTOR, timer_overhead_ns};
use crate::derived::Derived;
use crate::error::MachineError;
use crate::facts::Facts;
#[cfg(test)]
use crate::facts::PowerState;
use crate::placement::Placement;
use crate::probes::{
  CodecPoint, CorePairRtt, FaultCosts, HashThroughput, LockCapacity, MemcpyPoint, Pinning,
};
use crate::wake::WakeLatency;
use crate::{derived, probes, wake};

/// The profile format version; bumped when a field's meaning changes. 2 (2026-09-22): the wake latency is
/// the confirmed sleeper's wake on the production placement, reported by its mean with an interval, its
/// rounds and their verdict ([`crate::wake`]); version 1's p50/p99 timed a different event. 3 (2026-09-29):
/// the facts' cores are the ones this process may run on, by the OS's ids, beside the CPU budget a cgroup
/// grants it, and the wake probe is placed by [`crate::placement`] (unpinned, `Scheduled`, where the
/// process shares its cores in time); version 2 listed `0..available_parallelism()`
/// (docs/bugs/2026-09-29-every-daemon-under-a-cpu-quota-pinned-its-shard-to-cpu-1.md).
/// Format: the version this build writes and the only one it reads.
pub const PROFILE_VERSION: u32 = 3;

/// How to take a profile.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProfileOptions {
  /// The wall budget per probe.
  pub budget_per_probe: Duration,
  /// Whether to measure the codecs (the most expensive probe).
  pub codecs: bool,
  /// Whether to measure the core matrix.
  pub core_matrix: bool,
}

impl Default for ProfileOptions {
  fn default() -> Self {
    Self {
      budget_per_probe: PROBE_WALL_BUDGET,
      codecs: true,
      core_matrix: true,
    }
  }
}

/// The profile.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MachineProfile {
  /// The format version.
  pub version: u32,
  /// The fixed facts.
  pub facts: Facts,
  /// Nanoseconds per monotonic clock read.
  pub timer_overhead_ns: u64,
  /// The null syscall.
  pub syscall: Measurement,
  /// Fault costs per page class.
  pub faults: FaultCosts,
  /// The wake latency: the expected cost of parking ([`crate::wake`]).
  pub wake: WakeLatency,
  /// The core matrix.
  pub core_rtt: Vec<CorePairRtt>,
  /// How the matrix's threads were placed.
  pub pinning: Pinning,
  /// The memcpy curve.
  pub memcpy: Vec<MemcpyPoint>,
  /// Hash throughput.
  pub hash: HashThroughput,
  /// Codec throughput per candidate level (empty when not measured).
  pub codecs: Vec<CodecPoint>,
  /// The lock capacity.
  pub lock: LockCapacity,
  /// True when any probe stopped at its wall bound before converging.
  pub quick: bool,
  /// The wall time the whole profile took, in nanoseconds.
  pub elapsed_ns: u64,
}

impl MachineProfile {
  /// Measures the machine within each budget in turn, stopping at the first that answers; the last
  /// budget's refusal is the refusal. `options`' own `budget_per_probe` is unused — the budgets are the
  /// argument — while its `codecs` and `core_matrix` are honoured, so a quick profile stays quick in
  /// **scope** and only its patience grows.
  ///
  /// This exists because a single bound is a guess about the machine, and the cheap bound is wrong on a busy
  /// one: five milliseconds is not enough for the wake probe when a machine is loaded, measured on
  /// 2026-10-18 when a full `cargo test --workspace` refused that bound twice while the same calls passed
  /// alone. Refusing to measure means refusing to start — `slates`' MCP server takes this path — so a caller
  /// that can afford to wait longer says so here rather than failing. An empty `budgets` is refused without
  /// probing, since no bound was offered.
  pub fn measure_within(
    options: &ProfileOptions,
    budgets: &[std::time::Duration],
  ) -> Result<MachineProfile, MachineError> {
    let mut last = Err(MachineError::MeasurementTimeout { probe: "wake" });
    for budget in budgets {
      last = MachineProfile::measure(ProfileOptions {
        budget_per_probe: *budget,
        codecs: options.codecs,
        core_matrix: options.core_matrix,
      });
      if last.is_ok() {
        return last;
      }
    }
    last
  }

  /// Measures the machine within the caller's own per-probe bound, and — if that bound was too short for this
  /// machine right now — again within the probe's own wall ceiling ([`crate::bench::PROBE_WALL_BUDGET`]) before
  /// the refusal stands. The scope is the caller's throughout: only the patience grows.
  ///
  /// This is the one escalation policy, in one place. It was written out by hand at eleven call sites — the CLI's
  /// quick measurement and ten test fixtures, each with its own copy of the same two-element ladder and the same
  /// paragraph explaining it. The copies are why the sweep of 2026-10-18 reached ten of the eleven and missed
  /// `slates-server`'s own unit-test fixture, where a single refused probe behind a `OnceLock` failed sixty tests
  /// in one binary in 1.19 seconds. A policy spelled out in eleven places is one that has to be remembered eleven
  /// times.
  ///
  /// The caller's bound is the first rung and not a decoration: binaries differ in how much measurement they
  /// want before they settle (`slates-rt`'s placement test asks for fifty milliseconds, the seal suite
  /// twenty-five, most fixtures five), and a profile measured within a longer bound is a finer one. A caller
  /// whose bound already reaches the ceiling gets one attempt, not a second shorter one.
  pub fn measure_patiently(options: &ProfileOptions) -> Result<MachineProfile, MachineError> {
    let ceiling = crate::bench::PROBE_WALL_BUDGET;
    if options.budget_per_probe >= ceiling {
      return MachineProfile::measure_within(options, &[options.budget_per_probe]);
    }
    MachineProfile::measure_within(options, &[options.budget_per_probe, ceiling])
  }

  /// Measures the machine; refused `MeasurementTimeout` when a probe could not measure at all within its
  /// bound (the wake probe, [`wake::wake`]) rather than a profile carrying a fabricated value.
  pub fn measure(options: ProfileOptions) -> Result<MachineProfile, MachineError> {
    let started = std::time::Instant::now();
    let facts = Facts::query();
    let budget = options.budget_per_probe;
    let timer_overhead_ns = timer_overhead_ns();
    let syscall = probes::syscall(budget);
    let faults = probes::faults(&facts.page, budget);
    let wake = wake::wake(budget, &Placement::of(&facts.cores, facts.cpu_budget))?;
    let (core_rtt, pinning) = if options.core_matrix {
      probes::core_matrix(&facts.cores, budget)
    } else {
      (Vec::new(), Pinning::Refused)
    };
    let cap = memcpy_cap(&facts);
    let memcpy = probes::memcpy_curve(facts.cache_line, cap, budget);
    let hash = probes::hash(hash_bytes(&facts), budget);
    let mut facts = facts;
    let codecs = if options.codecs {
      let points = probes::codecs(codec_bytes(&facts), budget);
      if points.is_empty() {
        facts
          .notes
          .push("codec probe not compiled in (feature `codecs` off)".to_owned());
      }
      points
    } else {
      Vec::new()
    };
    let lock = probes::lock_capacity(&facts);
    let mut profile = MachineProfile {
      version: PROFILE_VERSION,
      facts,
      timer_overhead_ns,
      syscall,
      faults,
      wake,
      core_rtt,
      pinning,
      memcpy,
      hash,
      codecs,
      lock,
      quick: false,
      elapsed_ns: 0,
    };
    profile.quick = !profile.quick_probes().is_empty();
    profile.elapsed_ns = crate::bench::nanos(started.elapsed());
    Ok(profile)
  }

  /// Whether the machine's power state differs from the one the profile was measured under
  /// (one OS query; the daemon polls it on its slow timer and re-measures when it flips).
  pub fn power_changed(&self) -> bool {
    Facts::query().power != self.facts.power
  }

  /// Re-measures the cheap subset if the power state changed; returns whether it did (a refused wake
  /// re-measure keeps the previous measurement and is the refusal).
  /// **Nothing in the daemon calls this, and D-11 says something should.** That decision's own title is
  /// "every tunable derived at boot from measurements, logged with inputs, **re-measured on power-state
  /// change**", and the re-measurement half is unwired: this function, [`MachineProfile::power_changed`] and
  /// [`MachineProfile::refresh_cheap`] have no caller outside this file's tests. A laptop moving between
  /// battery and mains therefore keeps every tunable at its boot value, which is exactly the machine slates
  /// targets. Recorded in `docs/wip/GAPS.md` (2026-10-18); the budget a caller should pass is its own
  /// question, since a refusal here means the machine was too busy to measure, not that nothing changed.
  pub fn refresh_if_power_changed(&mut self, budget: Duration) -> Result<bool, MachineError> {
    if !self.power_changed() {
      return Ok(false);
    }
    self.refresh_cheap(budget)?;
    Ok(true)
  }

  /// Re-measures the cheap, power-sensitive subset (timer, syscall, wake) and the power state,
  /// for the daemon to call when the power state changes.
  ///
  /// A wake re-measure that is refused leaves the whole profile in place — the wake was measured
  /// and a refusal is no measurement to replace it with, and the power state it was measured under
  /// is what makes the next poll try again — and the refusal is returned.
  ///
  /// Guard then apply, the `crates/db/src/partition.rs` pattern: every fallible measurement is taken
  /// before a single field moves. Recording the new power state ahead of the measurement (as this did
  /// until 2026-10-18) retires the trigger on a refusal — [`MachineProfile::power_changed`] then reads
  /// false while the profile still carries the old state's wake, so a daemon polling it re-measures
  /// never, and one busy moment pins every tunable to the wrong power source for the process's life.
  /// Found by `a_power_state_change_is_detected_and_triggers_a_refresh` under load 56 on 2026-10-18.
  pub fn refresh_cheap(&mut self, budget: Duration) -> Result<(), MachineError> {
    let power = Facts::query().power;
    let timer_overhead_ns = timer_overhead_ns();
    let syscall = probes::syscall(budget);
    let wake = wake::wake(
      budget,
      &Placement::of(&self.facts.cores, self.facts.cpu_budget),
    )?;
    self.facts.power = power;
    self.timer_overhead_ns = timer_overhead_ns;
    self.syscall = syscall;
    self.wake = wake;
    Ok(())
  }

  /// The profile as JSON with every interval.
  pub fn to_json(&self) -> Result<String, serde_json::Error> {
    serde_json::to_string_pretty(self)
  }

  /// A profile from its JSON, refused by name when it was written in another format version (a field's
  /// meaning changed between them, so reading one as the other would derive from the wrong quantity).
  pub fn from_json(json: &str) -> Result<MachineProfile, serde_json::Error> {
    match Self::format_version(json) {
      Some(version) if version != PROFILE_VERSION => {
        Err(<serde_json::Error as serde::de::Error>::custom(format!(
          "profile format version {version}; this build reads version {PROFILE_VERSION}"
        )))
      }
      _ => serde_json::from_str(json),
    }
  }

  /// The format version a profile's JSON declares, `None` when the text names none — asked before
  /// [`from_json`](Self::from_json) by a reader that measures afresh rather than fail on another version
  /// (the daemon handed an anchor's profile across an upgrade).
  pub fn format_version(json: &str) -> Option<u32> {
    serde_json::from_str::<serde_json::Value>(json)
      .ok()?
      .get("version")?
      .as_u64()
      .and_then(|version| u32::try_from(version).ok())
  }

  /// The derived constants, recomputed from the measurements.
  pub fn derived(&self) -> DerivedConstants {
    DerivedConstants::from_profile(self)
  }

  /// The probes that stopped at their wall bound before converging, by name.
  pub fn quick_probes(&self) -> Vec<&'static str> {
    let mut out = Vec::new();
    if self.syscall.quick {
      out.push("syscall");
    }
    if self.faults.base_region.quick || self.faults.map_unmap_region.quick {
      out.push("faults");
    }
    if self.wake.quick {
      out.push("wake");
    }
    // Rounds that measured different wakes (a mode, or the host's load drifting through the probe) make
    // the measurement degraded even when each round converged; nothing is sized from the verdict itself.
    if self.wake.rounds_agree == Some(false) {
      out.push("wake.rounds");
    }
    if self.core_rtt.iter().any(|p| p.rtt.quick) {
      out.push("core_rtt");
    }
    if self.memcpy.iter().any(|p| p.per_copy.quick) {
      out.push("memcpy");
    }
    if self.hash.per_buffer.quick {
      out.push("hash");
    }
    if self.codecs.iter().any(|c| c.quick) {
      out.push("codecs");
    }
    out
  }

  /// The median ring round trip across the measured pairs (0 when the matrix was skipped).
  pub fn core_rtt_median_ns(&self) -> u64 {
    let mut values: Vec<u64> = self.core_rtt.iter().map(|p| p.rtt.median_ns()).collect();
    values.sort_unstable();
    values
      .get(crate::stats::Percentile::P50.index(values.len()))
      .copied()
      .unwrap_or(0)
  }
}

/// The memcpy curve's largest size: the larger of eight times the largest L2 and a thousand base
/// pages, bounded by a sixteenth of available memory so the probe never pressures the machine.
fn memcpy_cap(facts: &Facts) -> u64 {
  derived!(
    {
      let by_cache = facts.largest_l2().saturating_mul(8);
      let by_page = facts.page.base.saturating_mul(1024);
      let bound = facts.memory.available / 16;
      by_cache.max(by_page).min(bound.max(facts.page.base)).next_power_of_two()
    },
    "max(8 × largest L2, 1024 × base page), bounded by available memory / 16, rounded to a power of two",
    ["cores.l2_bytes", "page.base", "memory.available"]
  )
  .get()
}

/// The buffer the hash and codec probes run over: a thousand base pages (4 MiB at 4 KiB pages,
/// 16 MiB at 16 KiB), the large-chunk class candidate of §4.11.
fn hash_bytes(facts: &Facts) -> u64 {
  derived!(
    facts.page.base.saturating_mul(1024),
    "1024 × base page",
    ["page.base"]
  )
  .get()
}

/// The corpus the codec probe runs over: sixty-four base pages (1 MiB at 16 KiB pages, 256 KiB
/// at 4 KiB), the small-chunk class candidate of §4.11; the slow levels would blow the boot
/// budget over the large class, and the cost model of Phase 7 measures that class itself.
fn codec_bytes(facts: &Facts) -> u64 {
  derived!(
    facts.page.base.saturating_mul(64),
    "64 × base page",
    ["page.base"]
  )
  .get()
}

/// The constants of §4.1 that other crates consume, each with its formula and anchors.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct DerivedConstants {
  /// How long a waiter spins before parking: the expected (mean) cost of parking — the threshold of the
  /// 2-competitive spin-then-park rule of Karlin et al. (spin as long as parking is expected to cost, then
  /// park), with the tail included because it is a cost of parking, not noise ([`crate::wake`]).
  pub spin_before_park_ns: Derived<u64>,
  /// The arena region size: the size at which one map syscall is one percent of the faults it
  /// amortizes, rounded to a power of two and to the huge page where the huge class is cheaper.
  pub arena_region_bytes: Derived<u64>,
  /// The timing wheel's tick: no shorter than the expected wake, and no shorter than the timer floor.
  pub timer_tick_ns: Derived<u64>,
  /// The cooperative quantum: how long one task step may run before the shard serves its other work (the
  /// I/O harvest cadence, the slice of a destroy, an archive or a pre-fault, and the long-step count) —
  /// the expected wake, so a step keeps a peer waiting no longer than parking would have.
  pub task_step_budget_ns: Derived<u64>,
  /// The size above which a remap beats a copy, from the memcpy curve and the fault cost.
  pub copy_versus_remap_bytes: Derived<u64>,
  /// The smallest released block whose pages go back to the OS rather than being zeroed in place: the smallest
  /// measured memcpy size whose copy costs more than one syscall (zeroing a block costs at most a copy of it, and
  /// giving its pages back costs one `madvise`). `u64::MAX` when no measured size does: zeroing always wins.
  pub discard_from_bytes: Derived<u64>,
  /// The inbound ring's entry count — Little's law at the ring's overflow target: a producer sending one
  /// message per syscall for as long as the consumer's wake takes at its p99, so a producer spins on a full
  /// ring in fewer than one wake in a hundred.
  pub ring_entries: Derived<u64>,
}

impl DerivedConstants {
  fn from_profile(p: &MachineProfile) -> DerivedConstants {
    let page = p.facts.page.base.max(1);
    let wake_mean = p.wake.mean_ns.max(1);
    let wake_p99 = p.wake.p99_ns.max(1);
    let syscall = p.syscall.median_ns().max(1);
    let fault = p.faults.base_ns.max(1);
    let timer_floor = p
      .timer_overhead_ns
      .saturating_mul(TIMER_OVERHEAD_FACTOR)
      .max(1);
    DerivedConstants {
      spin_before_park_ns: derived!(
        wake_mean,
        "wake.mean (the expected cost of parking: the 2-competitive spin-then-park threshold)",
        ["wake.mean_ns"]
      ),
      arena_region_bytes: derived!(
        arena_region(p, page, syscall, fault),
        "pages = 100 × 2 × syscall / fault; region = pages × page, rounded up to a power of two; the huge page when huge faults are cheaper per byte",
        [
          "syscall.median",
          "faults.base_ns",
          "faults.huge_ns",
          "page.base",
          "page.huge"
        ]
      ),
      timer_tick_ns: derived!(
        wake_mean.max(timer_floor),
        "max(wake.mean, 100 × timer overhead)",
        ["wake.mean_ns", "timer_overhead_ns"]
      ),
      task_step_budget_ns: derived!(
        wake_mean,
        "wake.mean (a step longer than a peer's expected wake starves the shard)",
        ["wake.mean_ns"]
      ),
      copy_versus_remap_bytes: derived!(
        copy_versus_remap(p, page, fault),
        "the smallest memcpy size whose copy time exceeds the fault cost of its pages (u64::MAX when no measured size does: copying always wins)",
        ["memcpy", "faults.base_ns", "page.base"]
      ),
      discard_from_bytes: derived!(
        discard_versus_zero(p, syscall),
        "the smallest memcpy size whose copy time exceeds one syscall (u64::MAX when none does: zeroing in place always wins)",
        ["memcpy", "syscall"]
      ),
      ring_entries: derived!(
        wake_p99
          .checked_div(syscall)
          .unwrap_or(0)
          .max(1)
          .next_power_of_two(),
        "Little's law at the overflow target: one message per syscall.median for a wake at its p99, rounded up to a power of two",
        ["wake.p99_ns", "syscall.median"]
      ),
    }
  }

  /// One line per constant: `name = value (formula; anchors)`, for the boot log.
  pub fn lines(&self) -> Vec<String> {
    fn line(name: &str, d: &Derived<u64>) -> String {
      format!(
        "{name} = {} ({}; anchors: {})",
        d.value,
        d.formula,
        d.anchors.join(", ")
      )
    }
    vec![
      line("spin_before_park_ns", &self.spin_before_park_ns),
      line("arena_region_bytes", &self.arena_region_bytes),
      line("timer_tick_ns", &self.timer_tick_ns),
      line("task_step_budget_ns", &self.task_step_budget_ns),
      line("copy_versus_remap_bytes", &self.copy_versus_remap_bytes),
      line("discard_from_bytes", &self.discard_from_bytes),
      line("ring_entries", &self.ring_entries),
    ]
  }
}

fn arena_region(p: &MachineProfile, page: u64, syscall: u64, fault: u64) -> u64 {
  // Two syscalls (map and unmap) at one percent of the faults they amortize.
  let pages = syscall
    .saturating_mul(2)
    .saturating_mul(TIMER_OVERHEAD_FACTOR)
    .checked_div(fault)
    .unwrap_or(0);
  let region = pages.max(1).saturating_mul(page).next_power_of_two();
  match (p.faults.huge_ns, p.facts.page.huge.first()) {
    (Some(huge_ns), Some(huge)) if huge_ns < fault => region.max(*huge),
    _ => region,
  }
}

fn discard_versus_zero(p: &MachineProfile, syscall: u64) -> u64 {
  p.memcpy
    .iter()
    .find(|point| point.per_copy.median_ns() > syscall)
    .map_or(u64::MAX, |point| point.bytes)
}

fn copy_versus_remap(p: &MachineProfile, page: u64, fault: u64) -> u64 {
  p.memcpy
    .iter()
    .find(|point| {
      let pages = point.bytes.div_ceil(page);
      point.per_copy.median_ns() > pages.saturating_mul(fault)
    })
    .map_or(u64::MAX, |point| point.bytes)
}

#[cfg(test)]
mod tests {
  /// Shape: the budget this test gives the refresh — the full profile's own per-probe bound, not a tenth of
  /// it. The test is about the *detection and the refresh*, never about their speed, and a ten-millisecond
  /// budget is not enough for the wake probe on a loaded machine: a full `cargo test --workspace` refused it
  /// `MeasurementTimeout { probe: "wake" }` on 2026-10-18 where the test passes alone every time.
  const REFRESH_BUDGET: Duration = crate::bench::PROBE_WALL_BUDGET;

  use super::*;

  fn quick_options() -> ProfileOptions {
    ProfileOptions {
      budget_per_probe: Duration::from_millis(30),
      codecs: false,
      core_matrix: false,
    }
  }

  /// The profile these tests work from: measured **once per process** and **patiently**, then cloned for each
  /// test to read or mutate. Seven tests here each measured their own within a thirty-millisecond bound and
  /// required it to succeed — seven rolls of the same die, each able to fail for the machine's load rather than
  /// for anything the test asserts. The measurement is not what any of them is about; it is the material they
  /// need in order to assert something else.
  fn measured_once() -> MachineProfile {
    static PROFILE: std::sync::OnceLock<Result<MachineProfile, MachineError>> =
      std::sync::OnceLock::new();
    PROFILE
      .get_or_init(|| MachineProfile::measure_patiently(&quick_options()))
      .clone()
      .expect("the machine profile measures")
  }

  #[test]
  fn a_profile_measures_within_its_budget_and_round_trips_through_json() {
    let profile = measured_once();
    assert_eq!(profile.version, PROFILE_VERSION);
    assert!(profile.syscall.median_ns() > 0);
    assert!(profile.faults.base_ns > 0);
    assert!(profile.wake.mean_ns > 0);
    assert!(!profile.memcpy.is_empty());
    let json = profile.to_json().unwrap();
    assert!(json.contains("\"interval\""), "intervals are exported");
    let back = MachineProfile::from_json(&json).unwrap();
    assert_eq!(profile, back);
  }

  /// A profile written in another format version is refused by name, never read as this one (version 1's
  /// wake p50/p99 timed a different event than version 2's), and its version is readable first so a
  /// reader can measure afresh instead (the daemon handed an older anchor's profile across an upgrade).
  #[test]
  fn a_profile_of_another_format_version_is_refused_by_name() {
    let profile = measured_once();
    let json = profile.to_json().unwrap();
    assert_eq!(MachineProfile::format_version(&json), Some(PROFILE_VERSION));
    let older = json.replacen(
      &format!("\"version\": {PROFILE_VERSION}"),
      "\"version\": 1",
      1,
    );
    assert_eq!(MachineProfile::format_version(&older), Some(1));
    let refused = MachineProfile::from_json(&older).unwrap_err().to_string();
    assert!(refused.contains("format version 1"), "{refused}");
  }

  #[test]
  fn every_derived_constant_has_a_formula_and_anchors_and_is_logged() {
    let profile = measured_once();
    let d = profile.derived();
    assert_eq!(d.spin_before_park_ns.get(), profile.wake.mean_ns.max(1));
    assert_eq!(d.task_step_budget_ns.get(), profile.wake.mean_ns.max(1));
    assert!(d.arena_region_bytes.get() >= profile.facts.page.base);
    assert!(
      d.arena_region_bytes.get().is_power_of_two()
        || profile
          .facts
          .page
          .huge
          .contains(&d.arena_region_bytes.get())
    );
    assert!(d.timer_tick_ns.get() >= profile.wake.mean_ns);
    assert!(d.ring_entries.get().is_power_of_two());
    let discard = d.discard_from_bytes.get();
    assert!(
      discard == u64::MAX || profile.memcpy.iter().any(|point| point.bytes == discard),
      "the discard threshold is a measured copy size or never: {discard}"
    );
    let lines = d.lines();
    assert_eq!(lines.len(), 7);
    assert!(lines.iter().all(|l| l.contains("anchors:")), "{lines:?}");
  }

  /// §4.1: a re-measure of the cheap, power-sensitive subset leaves the rest of the profile alone — the fault
  /// costs above all, which it never measures. That claim holds whichever way the wake probe went, and both ways
  /// are asserted: requiring the probe to succeed made this a test of the machine's load, and it failed as one in
  /// a full `cargo test --workspace` on 2026-10-18.
  #[test]
  fn refreshing_the_cheap_subset_keeps_the_rest() {
    let mut profile = measured_once();
    let faults = profile.faults.clone();
    match profile.refresh_cheap(REFRESH_BUDGET) {
      Ok(()) => assert!(
        profile.wake.mean_ns > 0,
        "a re-measure that answered carries a wake"
      ),
      Err(MachineError::MeasurementTimeout { probe: "wake" }) => {}
      Err(other) => panic!("the re-measure refused for another reason: {other:?}"),
    }
    assert_eq!(
      profile.faults, faults,
      "the fault costs are not the cheap subset and are never touched by a refresh, measured or refused"
    );
  }

  /// §4.1 refusals: a re-measure whose wake probe could not measure refuses by name and leaves the
  /// profile whole — the wake it already measured, and the power state it measured that wake under.
  /// Keeping the wake while recording the new power state (the shape until 2026-10-18) is the worse
  /// half of one bug: [`MachineProfile::power_changed`] then reads false, so the re-measure the
  /// refusal still owes is never retried and the profile carries one power source's wake under the
  /// other's label for the rest of the process's life.
  #[test]
  fn a_refused_wake_re_measure_keeps_the_measured_wake() {
    let mut profile = measured_once();
    let measured = profile.wake;
    // Pretend the profile was taken on the other source, so the refusal has a trigger it could drop.
    profile.facts.power = match profile.facts.power {
      PowerState::Mains => PowerState::Battery,
      _ => PowerState::Mains,
    };
    let stale = profile.facts.power;
    assert_eq!(
      profile.refresh_cheap(Duration::ZERO),
      Err(MachineError::MeasurementTimeout { probe: "wake" })
    );
    assert_eq!(profile.wake, measured);
    assert_eq!(
      profile.facts.power, stale,
      "a refused re-measure leaves the power state it could not measure under"
    );
    assert!(
      profile.power_changed(),
      "the trigger survives the refusal, so the next poll re-measures this machine"
    );
  }

  /// §4.1 refusals: a profile whose wake probe could not measure at all is refused by name, never a
  /// profile carrying a zero wake for the daemon to derive its spin and its quantum from.
  #[test]
  fn a_profile_whose_wake_was_not_measured_is_refused() {
    let refused = MachineProfile::measure(ProfileOptions {
      budget_per_probe: Duration::ZERO,
      codecs: false,
      core_matrix: false,
    });
    assert_eq!(
      refused.err(),
      Some(MachineError::MeasurementTimeout { probe: "wake" })
    );
  }

  /// D-11 "re-measured on power-state change": a profile taken under the other power source is
  /// detected and re-measured. A busy machine can refuse the wake probe, and the two outcomes are
  /// asserted apart — a completed refresh records the current state, a refused one leaves the
  /// profile whole so the trigger survives for the next poll. Asserting only the completed branch
  /// made this a measurement of the machine's load (it failed at load 56 on 2026-10-18), and the
  /// refused branch is the one that found the defect: the state was recorded before the probe ran.
  #[test]
  fn a_power_state_change_is_detected_and_triggers_a_refresh() {
    let mut profile = measured_once();
    assert!(
      !profile.power_changed(),
      "the state has not moved since the profile"
    );
    assert_eq!(profile.refresh_if_power_changed(REFRESH_BUDGET), Ok(false));
    // Pretend the profile was taken on the other source: the next check must refresh.
    profile.facts.power = match profile.facts.power {
      PowerState::Mains => PowerState::Battery,
      _ => PowerState::Mains,
    };
    let stale = profile.facts.power;
    let before = profile.wake;
    assert!(profile.power_changed());
    match profile.refresh_if_power_changed(REFRESH_BUDGET) {
      Ok(true) => assert!(
        !profile.power_changed(),
        "a refresh that measured records the state it measured under"
      ),
      Err(MachineError::MeasurementTimeout { probe: "wake" }) => {
        assert!(
          profile.power_changed(),
          "a refused refresh keeps the trigger, or nothing ever re-measures this machine"
        );
        assert_eq!(
          profile.facts.power, stale,
          "a refused refresh leaves the power state it could not measure under"
        );
        assert_eq!(
          profile.wake, before,
          "a refused refresh leaves the measurement it had no replacement for"
        );
      }
      other => panic!("the refresh neither measured nor timed out on the wake probe: {other:?}"),
    }
  }

  #[test]
  fn the_copy_versus_remap_threshold_follows_the_curve() {
    let mut profile = measured_once();
    profile.faults.base_ns = 1;
    let first = profile.memcpy.first().map(|p| p.bytes).unwrap();
    let d = profile.derived();
    assert!(d.copy_versus_remap_bytes.get() >= first);
    profile.faults.base_ns = u64::MAX / 4;
    assert_eq!(profile.derived().copy_versus_remap_bytes.get(), u64::MAX);
  }
}
