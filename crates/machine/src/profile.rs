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
use crate::facts::Facts;
#[cfg(test)]
use crate::facts::PowerState;
use crate::probes::{
  CodecPoint, CorePairRtt, FaultCosts, HashThroughput, LockCapacity, MemcpyPoint, Pinning,
  WakeLatency,
};
use crate::{derived, probes};

/// Format: the profile format version; bumped when a field's meaning changes.
pub const PROFILE_VERSION: u32 = 1;

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
  /// Park/unpark latency.
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
  /// Measures the machine.
  pub fn measure(options: ProfileOptions) -> MachineProfile {
    let started = std::time::Instant::now();
    let facts = Facts::query();
    let budget = options.budget_per_probe;
    let timer_overhead_ns = timer_overhead_ns();
    let syscall = probes::syscall(budget);
    let faults = probes::faults(&facts.page, budget);
    let wake = probes::wake(budget);
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
    profile
  }

  /// Whether the machine's power state differs from the one the profile was measured under
  /// (one OS query; the daemon polls it on its slow timer and re-measures when it flips).
  pub fn power_changed(&self) -> bool {
    Facts::query().power != self.facts.power
  }

  /// Re-measures the cheap subset if the power state changed; returns whether it did.
  pub fn refresh_if_power_changed(&mut self, budget: Duration) -> bool {
    if !self.power_changed() {
      return false;
    }
    self.refresh_cheap(budget);
    true
  }

  /// Re-measures the cheap, power-sensitive subset (timer, syscall, wake) and the power state,
  /// for the daemon to call when the power state changes.
  pub fn refresh_cheap(&mut self, budget: Duration) {
    self.facts.power = Facts::query().power;
    self.timer_overhead_ns = timer_overhead_ns();
    self.syscall = probes::syscall(budget);
    self.wake = probes::wake(budget);
  }

  /// The profile as JSON with every interval.
  pub fn to_json(&self) -> Result<String, serde_json::Error> {
    serde_json::to_string_pretty(self)
  }

  /// A profile from its JSON.
  pub fn from_json(json: &str) -> Result<MachineProfile, serde_json::Error> {
    serde_json::from_str(json)
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
  /// How long a waiter spins before parking: the median park/unpark cost, the 2-competitive
  /// bound of Karlin et al. (spin as long as the switch would cost, then park).
  pub spin_before_park_ns: Derived<u64>,
  /// The arena region size: the size at which one map syscall is one percent of the faults it
  /// amortizes, rounded to a power of two and to the huge page where the huge class is cheaper.
  pub arena_region_bytes: Derived<u64>,
  /// The timing wheel's tick: no shorter than a wake, and no shorter than the timer floor.
  pub timer_tick_ns: Derived<u64>,
  /// The step budget a task may run before the watchdog counts it as starving its shard.
  pub task_step_budget_ns: Derived<u64>,
  /// The size above which a remap beats a copy, from the memcpy curve and the fault cost.
  pub copy_versus_remap_bytes: Derived<u64>,
  /// The inbound ring's entry count: enough that a producer can keep sending for a whole wake.
  pub ring_entries: Derived<u64>,
}

impl DerivedConstants {
  fn from_profile(p: &MachineProfile) -> DerivedConstants {
    let page = p.facts.page.base.max(1);
    let wake_p50 = p.wake.p50_ns.max(1);
    let wake_p99 = p.wake.p99_ns.max(wake_p50);
    let syscall = p.syscall.median_ns().max(1);
    let fault = p.faults.base_ns.max(1);
    let timer_floor = p
      .timer_overhead_ns
      .saturating_mul(TIMER_OVERHEAD_FACTOR)
      .max(1);
    DerivedConstants {
      spin_before_park_ns: derived!(
        wake_p50,
        "wake.p50 (2-competitive spin bound)",
        ["wake.p50_ns"]
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
        wake_p50.max(timer_floor),
        "max(wake.p50, 100 × timer overhead)",
        ["wake.p50_ns", "timer_overhead_ns"]
      ),
      task_step_budget_ns: derived!(
        wake_p99,
        "wake.p99 (a step longer than a peer's wake starves the shard)",
        ["wake.p99_ns"]
      ),
      copy_versus_remap_bytes: derived!(
        copy_versus_remap(p, page, fault),
        "the smallest memcpy size whose copy time exceeds the fault cost of its pages (u64::MAX when no measured size does: copying always wins)",
        ["memcpy", "faults.base_ns", "page.base"]
      ),
      ring_entries: derived!(
        (wake_p99 / syscall).max(1).next_power_of_two(),
        "wake.p99 / syscall.median, rounded up to a power of two",
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
      line("ring_entries", &self.ring_entries),
    ]
  }
}

fn arena_region(p: &MachineProfile, page: u64, syscall: u64, fault: u64) -> u64 {
  // Two syscalls (map and unmap) at one percent of the faults they amortize.
  let pages = syscall
    .saturating_mul(2)
    .saturating_mul(TIMER_OVERHEAD_FACTOR)
    / fault;
  let region = pages.max(1).saturating_mul(page).next_power_of_two();
  match (p.faults.huge_ns, p.facts.page.huge.first()) {
    (Some(huge_ns), Some(huge)) if huge_ns < fault => region.max(*huge),
    _ => region,
  }
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
  use super::*;

  fn quick_options() -> ProfileOptions {
    ProfileOptions {
      budget_per_probe: Duration::from_millis(30),
      codecs: false,
      core_matrix: false,
    }
  }

  #[test]
  fn a_profile_measures_within_its_budget_and_round_trips_through_json() {
    let profile = MachineProfile::measure(quick_options());
    assert_eq!(profile.version, PROFILE_VERSION);
    assert!(profile.syscall.median_ns() > 0);
    assert!(profile.faults.base_ns > 0);
    assert!(profile.wake.p50_ns > 0);
    assert!(!profile.memcpy.is_empty());
    let json = profile.to_json().unwrap();
    assert!(json.contains("\"interval\""), "intervals are exported");
    let back = MachineProfile::from_json(&json).unwrap();
    assert_eq!(profile, back);
  }

  #[test]
  fn every_derived_constant_has_a_formula_and_anchors_and_is_logged() {
    let profile = MachineProfile::measure(quick_options());
    let d = profile.derived();
    assert_eq!(d.spin_before_park_ns.get(), profile.wake.p50_ns.max(1));
    assert!(d.arena_region_bytes.get() >= profile.facts.page.base);
    assert!(
      d.arena_region_bytes.get().is_power_of_two()
        || profile
          .facts
          .page
          .huge
          .contains(&d.arena_region_bytes.get())
    );
    assert!(d.timer_tick_ns.get() >= profile.wake.p50_ns);
    assert!(d.ring_entries.get().is_power_of_two());
    let lines = d.lines();
    assert_eq!(lines.len(), 6);
    assert!(lines.iter().all(|l| l.contains("anchors:")), "{lines:?}");
  }

  #[test]
  fn refreshing_the_cheap_subset_keeps_the_rest() {
    let mut profile = MachineProfile::measure(quick_options());
    let faults = profile.faults.clone();
    profile.refresh_cheap(Duration::from_millis(20));
    assert_eq!(profile.faults, faults);
    assert!(profile.wake.p50_ns > 0);
  }

  #[test]
  fn a_power_state_change_is_detected_and_triggers_a_refresh() {
    let mut profile = MachineProfile::measure(quick_options());
    assert!(
      !profile.power_changed(),
      "the state has not moved since the profile"
    );
    assert!(!profile.refresh_if_power_changed(Duration::from_millis(10)));
    // Pretend the profile was taken on the other source: the next check must refresh.
    profile.facts.power = match profile.facts.power {
      PowerState::Mains => PowerState::Battery,
      _ => PowerState::Mains,
    };
    let before = profile.syscall;
    assert!(profile.power_changed());
    assert!(profile.refresh_if_power_changed(Duration::from_millis(10)));
    assert!(
      !profile.power_changed(),
      "the refresh recorded the current state"
    );
    let _ = before;
  }

  #[test]
  fn the_copy_versus_remap_threshold_follows_the_curve() {
    let mut profile = MachineProfile::measure(quick_options());
    profile.faults.base_ns = 1;
    let first = profile.memcpy.first().map(|p| p.bytes).unwrap();
    let d = profile.derived();
    assert!(d.copy_versus_remap_bytes.get() >= first);
    profile.faults.base_ns = u64::MAX / 4;
    assert_eq!(profile.derived().copy_versus_remap_bytes.get(), u64::MAX);
  }
}
