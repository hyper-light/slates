# The operator's durability policy did not gate a write: a breach was counted, never refused

Date: 2026-09-13
Area: `crates/server/src/verbs.rs` (the write gate), `crates/server/src/config.rs` (the measured
shortfall), `crates/server/src/daemon.rs` and `crates/server/src/fleet.rs` (the measurement at every
configuration install), `crates/ipc/src/protocol.rs` (the refusal on the wire).
Severity: a declared durability contract silently unmet — the last wiring the design's §4.8 "Placement"
paragraph names as owed.

## The rule (design §4.8 "Placement", quoted)

> The copyset count is computed at every configuration change and compared with its bound; exceeding it
> is a placement bug, not a tripwire. (Implemented: `Configuration::copyset_count` is the actual
> `owner_copysets` partition, `coincident_loss` its `#copysets·C(F,R)/C(H,R)` loss under a coincident
> failure of `F` hosts, and `within_loss_bound(ε, F)` the check — computable from any configuration; the
> operator's accepted `ε` and the coincident-failure size are the durability policy that gates a refusal,
> the last wiring owed.)

D-18: "A complete snapshot is f-fault-tolerant within its declared failure domains only after f+1 of 2f+1
eligible holders reserve, verify and retain every referenced object and its record." The failure matrix:
"Correlated loss of all f+1 copies of a snapshot: data loss of that snapshot, documented (D-18); the
neighbourhood bound makes it as rare as the operator chose."

The ε is the operator's declared `accepted_loss` (a policy, never derived — an accepted loss probability
is not a machine measurement); `F` is the declared `coincident_failures`. There is no second tolerance:
the check is exactly `coincident_loss(F) ≤ ε`, and the shortfall is the measured loss beside the ε.

## Description

`record_durability` ran the check at every configuration install and **counted** a breach
(`DURABILITY_BREACHES`, a health signal), but nothing refused a write: a fleet whose committed configuration
could not hold the declared durability kept accepting creates, seals and merge submits, each promising an
f-fault-tolerance the operator had said was not enough. Reproduced by use: a daemon configured as a member
of an `f = 1` fleet of three under a policy that accepts no loss under two coincident failures (an `f = 1`
copyset has two holders, so its loss under two failures is positive) accepted `Create` —
`a_write_the_declared_durability_cannot_cover_is_refused_typed` failed with `got Created { .. }`
(`cargo test -p slates-server --test daemon a_write_the_declared_durability_cannot_cover_is_refused_typed
-- --exact`, gate disabled, 1.24 s); the live two-node fleet analogue
`a_fleet_refuses_a_write_its_configuration_cannot_hold_to_the_declared_durability` likewise
(`got Created`, 2.24 s).

## Fix

- `DurabilityBound::shortfall(&Configuration) -> Option<DurabilityShortfall>` (`config.rs`): the
  `within_loss_bound(ε, F)` check answered with its numbers — the measured `coincident_loss`, the
  `accepted_loss` (ε) and the `coincident_failures` it was measured under; `None` within the bound.
  `breached_by` now delegates to it (one law).
- `record_durability` (`daemon.rs`) returns that shortfall and counts the breach. It is called where a
  change is first seen on the node — the control shard's boot and the council commit
  (`sync_config_from_council`); every shard measures the configuration it installs (boot on the other
  shards via `boot_durability`; the cross-shard fan in `fan_configs_to_shards`) with the pure `shortfall`,
  so one change moves the health signal once, not once per shard (a boot previously counted a breach on
  every shard — a sibling over-count, fixed here).
- `ShardState::durability_shortfall` holds the measurement; the verbs read it as a field. The
  `coincident_loss` computation is a few floating-point products on a cold path (a configuration change);
  the write path never recomputes it.
- `verbs::dispatch` refuses `Refusal::DurabilityUnmet { coincident_loss, accepted_loss,
  coincident_failures }` for a verb that **claims placed durability** — one whose completion commits a new
  head or seal the fleet replicates: `Create`, `CreateGreen`, `CreateWork`, `Clone` (a creation head),
  `Snapshot` (a seal), `Submit` (a green advance). The set is drawn from the register ops each verb
  commits, not guessed: `Edit`/`Declare`/`Rebase` are owner-local live edits (D-18: "may be lost with that
  host"), `Resize` a catalog change, destroys shrink obligations; reads, `AwaitPlaced`, attachments, grants
  and landings claim no placement. The gate sits after every completion-record lookup (`serve`,
  `run_forwarded`), so a retry of a write that succeeded before a breach still meets its recorded reply
  (RIFL); a forwarded write is gated on its owner, under the owner's own measurement.
- The wire: `Refusal::DurabilityUnmet` carries the two probabilities as `f64` (the wire codec already
  encodes `f64` canonically), so `Refusal`, `ReplyBody`, `ServerError` and `ClientError` no longer derive
  `Eq` (they keep `PartialEq`; nothing hashed or ordered them). The CLI prints the typed refusal
  (`slates: refused: DurabilityUnmet { coincident_loss: 1, accepted_loss: 0, coincident_failures: 2 }`,
  exit 1, `--json` carries the same text) and `slates status` counts it under `durability_unmet`. The SDKs
  pass a refusal's text through (`SlatesError`; per-refusal subclasses are owed there, unchanged by this).
- Laptop (`f = 0`) unchanged, R8: a single copy has no coincident loss (`coincident_loss_probability` is
  zero for `copies ≤ 1`), so the shortfall is `None` under any policy, and with no policy declared it is
  `None` everywhere — the same code, the policy deciding.

## Validation (2026-09-13, 18-core box shared with ten concurrent agent builds; load 4–7)

- Failing first (gate disabled): the daemon-level test `got Created` (1.24 s); the strict fleet test
  `got Created` at the control shard (2.24 s).
- After: `cargo test -p slates-server --test daemon` 2/2 (15.04 s — the durability test with three daemons
  and the pre-existing lifecycle test, which also passes again, see the sibling record);
  `a_fleet_refuses_a_write_its_configuration_cannot_hold_to_the_declared_durability` 3.15 s and 2.17 s;
  `a_fleet_within_its_declared_durability_creates_and_seals` 2.39 s and 2.12 s (each run alone, `--exact`);
  `cargo test -p slates-server --lib` 21/21 (`a_breach_reports_its_measured_shortfall`,
  `a_durability_bound_surfaces_a_breach`, `a_breaching_configuration_moves_the_durability_signal`);
  `cargo test -p slates-ipc -p slates-client -p slates-db` all green; `cargo test -p slates-cli -p
  slates-mcp` green (the gated real-process flows skip loudly, as designed).
- Gates: `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo xtask check`
  (structural, literals, unsafe) clean.
- Not run here (the integrator's, on a quiet box): the whole fleet suite; the `SLATES_TEST_CLI=1` process
  flows.

## Siblings

- **Fixed:** the acknowledge path pruned completion windows under the ephemeral member id while `serve`
  records them under the stable cert-anchor — `docs/bugs/2026-09-13-acknowledge-prunes-under-the-ephemeral-id.md`.
- **Fixed:** a boot counted one breach per shard (`init_shard` runs on every shard); now only the control
  shard counts, every shard measures.
- **Reported:** `DurabilityBound.accepted_loss` is parsed from the manifest as a JSON number; the
  refusal's numbers reach the CLI as the `Debug` text of the refusal (readable, not a JSON field of its
  own) — a structured `refusal` object in `--json` output is a CLI surface item (§4.12), unchanged here.
- **Reported:** the SDKs expose a refusal as text; a typed `DurabilityUnmet` exception is part of the owed
  per-refusal subclassing noted in `crates/sdk-python/src/lib.rs`.
