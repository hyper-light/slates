//! The time budgets every judgment of liveness is derived from (§4.14, A-228, A-232, A-359), in the smallest crate the
//! daemon, its database and the workspace's tools (`xtask`) all build on, so each has one source (R3; main's ruling of
//! 2026-11-20: a tool waiting on a daemon calls the derivation the daemon uses, never a copy of its number).
//! `slates_db::replay::RECOVERY_BUDGET_NS` and `slates_server::daemon::{LIVENESS_BUDGET_NS, OBSERVE_BUDGET_NS}` are these.

/// Shape: the recovery budget, RAMCloud's target of about a second for a crashed node
/// [A: Ongaro et al., SOSP'11: 35 GB in 1.6 s]; an operator input in Phase 2's CLI, ratified as the default (GAPS §5).
pub const RECOVERY_BUDGET_NS: u64 = 1_000_000_000;

/// Shape: the liveness windows an out-of-band observation has before it ends with a deadline (ten): ample headroom for
/// a shard a heavy CPU load slows toward a window a coordinator period to answer, while a wedged one still ends it
/// (`slates_server::daemon::OBSERVE_BUDGET_NS`).
pub const OBSERVE_LIVENESS_WINDOWS: u64 = 10;

/// Derived: the liveness window `W` (A-228, A-359): the recovery budget [`RECOVERY_BUDGET_NS`], by rent or buy (a
/// verdict waits as long as the recovery it starts costs). `slates_server::daemon::LIVENESS_BUDGET_NS` states the
/// derivation in full.
pub const fn liveness_budget_ns() -> u64 {
  RECOVERY_BUDGET_NS
}

/// Derived: the observation budget: [`OBSERVE_LIVENESS_WINDOWS`] liveness windows ([`liveness_budget_ns`]), the longest
/// any party goes unobserved before its observer's question ends with a deadline, and the ceiling every lapse bound is
/// held under (`slates_server::daemon::OBSERVE_BUDGET_NS`, `slates_server::watchdog::WINDOW_CEILING_NS`).
pub const fn observe_budget_ns() -> u64 {
  liveness_budget_ns().saturating_mul(OBSERVE_LIVENESS_WINDOWS)
}
