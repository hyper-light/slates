//! Where the shards run (§4.3 "Shards = performance cores"; D-9 thread-per-core; §4.1 "cores and
//! classes"): a shard is fixed to a core only when the process owns that core, and only ever to a core the
//! process may run on.
//!
//! A CPU quota is a share of time on the cores a process may use, not a claim on any one of them: a
//! container given two CPUs of an eighteen-CPU pool shares the whole pool with every other tenant. Until
//! 2026-09-29 the machine facts listed a process's cores as `0..available_parallelism()` — a count the
//! quota lowers, turned into ids the kernel never named — so every daemon under a two-CPU quota fixed its
//! one shard to CPU 1. The KIND lane's eight daemons stacked their shards on one of eighteen virtual CPUs,
//! runnable and waiting for it 80 % of the time while the other seventeen idled, and their anchors killed
//! them for late heartbeats every minute or two
//! (docs/bugs/2026-09-29-every-daemon-under-a-cpu-quota-pinned-its-shard-to-cpu-1.md).
//!
//! Each test reads what the kernel says this process may run on and what CPU time its cgroup grants,
//! starts a runtime from a measured profile as the daemon does, and asks every shard thread which CPUs it
//! may run on. The cases need a Linux kernel and a particular cgroup shape, so each one that does not hold
//! here skips loudly; `docker run --cpus=2` (a quota below the cpuset) and `docker run --cpuset-cpus=2,3`
//! (an owned cpuset that does not begin at CPU 0) exercise both.

// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

#[cfg(target_os = "linux")]
mod linux {
  use std::sync::mpsc::channel;
  use std::time::Duration;

  use slates_machine::profile::{MachineProfile, ProfileOptions};
  use slates_rt::runtime::{Runtime, RuntimeConfig};

  /// Shape: the per-probe wall budget of the profile the runtime derives from (the test fixtures' quick
  /// profile; the placement depends on the facts, not on the probes' precision).
  const PROBE_MS: u64 = 50;
  /// Shape: how long every shard gets to say which CPUs it may run on (a step is microseconds).
  const ANSWER: Duration = Duration::from_secs(10);
  /// Shape: the admission and timer limits of the runtime; the test spawns one task per shard.
  const TASKS: usize = 64;
  /// Shape: the latency budget the batch is calibrated against — a millisecond, the daemon's order.
  const LATENCY_BUDGET_NS: u64 = 1_000_000;

  /// Parses a kernel CPU list such as `0-3,8,10-11`.
  fn cpu_list(text: &str) -> Vec<u32> {
    let mut out = Vec::new();
    for part in text.trim().split(',').filter(|part| !part.is_empty()) {
      match part.split_once('-') {
        Some((first, last)) => {
          out.extend(first.parse::<u32>().unwrap()..=last.parse::<u32>().unwrap())
        }
        None => out.push(part.parse::<u32>().unwrap()),
      }
    }
    out
  }

  /// The CPUs the thread whose status `path` names may run on, as the kernel lists them.
  fn allowed_of(path: &str) -> Vec<u32> {
    let status = std::fs::read_to_string(path).unwrap();
    let list = status
      .lines()
      .find_map(|line| line.strip_prefix("Cpus_allowed_list:"))
      .unwrap();
    cpu_list(list)
  }

  /// The tightest CPU time this process's cgroups grant, as `(quota, period)` microseconds: cgroup v2
  /// `cpu.max`, or v1 `cpu.cfs_quota_us` over `cpu.cfs_period_us`, at the process's own cgroup (from
  /// `/proc/self/cgroup`) and at every ancestor up to the root, since a parent's quota binds its children.
  /// `None` when none of them sets one. The machine facts take the same tightest budget over the same walk,
  /// and the test reads the kernel's files itself, so it judges the placement against the kernel. Until
  /// 2026-11-05 it read the root's `cpu.max` alone: in a `systemd-run --user -p CPUQuota=1200%` unit on a
  /// 32-CPU host the facts saw the unit's 12-CPU quota and left the shards to the scheduler, as the design
  /// says, while the test read no quota and failed the owned-cpuset case 3 of 3.
  fn cpu_budget() -> Option<(u64, u64)> {
    let cgroup = std::fs::read_to_string("/proc/self/cgroup").ok()?;
    let mut tightest: Option<(u64, u64)> = None;
    for line in cgroup.lines() {
      // `id:controllers:path`; a v1 hierarchy names its controllers, the v2 one names none.
      let mut fields = line.splitn(3, ':');
      let (Some(_), Some(controllers), Some(path)) = (fields.next(), fields.next(), fields.next())
      else {
        continue;
      };
      let v1 = !controllers.is_empty();
      if v1 && !controllers.split(',').any(|controller| controller == "cpu") {
        continue;
      }
      let mut dir = path.trim_end_matches('/').to_owned();
      loop {
        if let Some(budget) = budget_at(&dir, v1) {
          tightest = Some(match tightest {
            Some(held) if share_at_most(held, budget) => held,
            _ => budget,
          });
        }
        if dir.is_empty() {
          break;
        }
        match dir.rfind('/') {
          Some(cut) => dir.truncate(cut),
          None => dir.clear(),
        }
      }
    }
    tightest
  }

  /// The `(quota, period)` one cgroup directory states, `None` for no quota (`max`, v1's `-1`) or no file.
  fn budget_at(dir: &str, v1: bool) -> Option<(u64, u64)> {
    let read = |path: String| std::fs::read_to_string(path).ok();
    if v1 {
      let quota = read(format!("/sys/fs/cgroup/cpu{dir}/cpu.cfs_quota_us"))?;
      let period = read(format!("/sys/fs/cgroup/cpu{dir}/cpu.cfs_period_us"))?;
      return Some((
        quota.trim().parse::<u64>().ok()?,
        period.trim().parse::<u64>().ok()?,
      ));
    }
    let text = read(format!("/sys/fs/cgroup{dir}/cpu.max"))?;
    let mut fields = text.split_whitespace();
    let quota = fields.next()?.parse::<u64>().ok()?;
    let period = fields.next()?.parse::<u64>().ok()?;
    Some((quota, period))
  }

  /// Whether budget `a` grants at most the CPU share `b` does: `a.quota / a.period <= b.quota / b.period`.
  fn share_at_most(a: (u64, u64), b: (u64, u64)) -> bool {
    u128::from(a.0) * u128::from(b.1) <= u128::from(b.0) * u128::from(a.1)
  }

  /// Whether a budget of `(quota, period)` buys every one of `cores` CPUs at once.
  fn covers(budget: Option<(u64, u64)>, cores: usize) -> bool {
    budget.is_none_or(|(quota, period)| {
      u128::from(quota) >= u128::from(period) * u128::try_from(cores).unwrap()
    })
  }

  /// Starts a runtime from a measured profile, as the daemon does, and returns what every shard thread
  /// may run on, in shard order.
  fn shard_masks() -> Vec<Vec<u32>> {
    let profile = MachineProfile::measure_patiently(&ProfileOptions {
      budget_per_probe: Duration::from_millis(PROBE_MS),
      codecs: false,
      core_matrix: false,
    })
    .expect("the machine profile measures");
    let config = RuntimeConfig::from_profile(&profile, TASKS, TASKS, LATENCY_BUDGET_NS);
    let rt = Runtime::start(&config).unwrap();
    let (answers, masks) = channel();
    for (index, shard) in rt.shard_ids().iter().enumerate() {
      let answers = answers.clone();
      rt.spawn_on(*shard, async move {
        let _ = answers.send((index, allowed_of("/proc/thread-self/status")));
      })
      .unwrap();
    }
    let mut out = vec![Vec::new(); rt.shard_ids().len()];
    for _ in 0..out.len() {
      let (index, mask) = masks.recv_timeout(ANSWER).unwrap();
      out[index] = mask;
    }
    rt.shutdown().unwrap();
    out
  }

  /// A process granted a share of a larger pool's time owns none of the pool's cores (a Kubernetes pod
  /// under the default CPU manager, `docker run --cpus`), so the operating system places its shards: fixed
  /// to one core, every such daemon on the machine picks the same one and waits for it while the rest of
  /// the pool idles. Do: start a runtime from the machine's profile under a CPU quota below the process's
  /// cpuset. Expect: every shard may run on every CPU the process may.
  #[test]
  fn under_a_quota_below_the_cpuset_the_scheduler_places_every_shard() {
    let process = allowed_of("/proc/self/status");
    let budget = cpu_budget();
    if budget.is_none() || covers(budget, process.len()) {
      eprintln!(
        "skipping: needs a cgroup CPU quota below the cpuset (here {budget:?} over {} CPUs); run under \
         `docker run --cpus=2`",
        process.len()
      );
      return;
    }
    for (shard, mask) in shard_masks().iter().enumerate() {
      assert_eq!(
        mask, &process,
        "shard {shard} is confined to {mask:?} of the {process:?} its process shares in time under \
         {budget:?}"
      );
    }
  }

  /// A process that owns its cores — a cpuset with no quota, or a quota that buys all of it (Kubernetes'
  /// static CPU manager) — has each shard fixed to one of them, and never to a core outside its set, where
  /// the kernel refuses the pin and the shard would run anywhere unnoticed. Do: start a runtime from the
  /// machine's profile on a cpuset the budget covers (one that does not begin at CPU 0 exercises the ids).
  /// Expect: every shard is fixed to a single core of the process's own set, each to a different one.
  #[test]
  fn on_an_owned_cpuset_each_shard_is_fixed_to_its_own_core_of_the_set() {
    let process = allowed_of("/proc/self/status");
    let budget = cpu_budget();
    if !covers(budget, process.len()) {
      eprintln!(
        "skipping: needs a cpuset its CPU budget covers (here {budget:?} over {} CPUs); run under \
         `docker run --cpuset-cpus=2,3`",
        process.len()
      );
      return;
    }
    let masks = shard_masks();
    let mut fixed: Vec<u32> = Vec::new();
    for (shard, mask) in masks.iter().enumerate() {
      let [core] = mask.as_slice() else {
        panic!("shard {shard} may run on {mask:?}, not one core of its owned set {process:?}");
      };
      assert!(
        process.contains(core),
        "shard {shard} is fixed to CPU {core}, outside its process's set {process:?}"
      );
      assert!(
        !fixed.contains(core),
        "shard {shard} shares CPU {core} with another shard"
      );
      fixed.push(*core);
    }
  }
}

/// The placement cases read the Linux kernel's affinity lists and cgroup files; elsewhere they skip loudly.
#[cfg(not(target_os = "linux"))]
#[test]
fn placement_cases_run_on_linux() {
  eprintln!(
    "skipping: shard placement is observed through Linux's affinity lists and cgroup files"
  );
}
