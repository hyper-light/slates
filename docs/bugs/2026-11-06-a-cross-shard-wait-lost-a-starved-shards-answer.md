# A cross-shard wait lost a starved shard's answer (2026-11-06, §4.3, §4.14, R3 class A)

## Description

A verb served on one shard that needs work done on another sends it there (`crate::xshard`) and waits for the answer.
About twenty such waits gave up after `LIVENESS_BUDGET_NS`, one second of wall time, whatever the other shard was doing.
On a busy machine, the normal case (Ada's rule), the target shard is often alive but kept off its core, running a
little at a time. Its answer then comes after the second. The caller had already moved on and refused the client
`Overloaded` (or `NotFound`), and the work went on to take effect anyway: a live shard's answer was lost, and the client
was told its request failed when it had not. A-228 already judges a shard's liveness in its own CPU time (A-65's rule),
so the daemon's watchdog called the shard alive while the verb's wait called it gone.

## Measurement

`crates/server/tests/cross_shard_wait.rs` on main `d61c3be2` (macOS arm64, M5 Max, 18 CPUs, 2026-11-06, load 26–33): a
client served on the second shard bootstraps the daemon while the control shard runs one poll of 1.5 s that is on the
CPU a fifth of the time (10 ms bursts, 40 ms off). The bootstrap was refused `Overloaded { shard: 0 }` after
**1.003 s**.

## Root cause

The wait was a wall deadline (`xshard::within`, `futures::within(LIVENESS_BUDGET_NS, call)`), so it measured how the
machine scheduled the target thread, not whether the target was working.

## Edits

- `crates/server/src/xshard.rs`: a call carries its target. `within_shard_time` waits a wall window of the budget, then
  goes on while the target made CPU progress in the window just ended, for the part of the budget of CPU it has not yet
  run. A wedged target is given up after one whole window off the CPU, and a busy one that never answers once it has
  run the budget. `call_in_shard_time` and `task_in_shard_time` are `call_within` and `task_within` waited that way.
  `within`, `call_within` and `task_within` stay for waits bounded by the caller's own clock (a coordinator period, a
  client's remaining deadline).
- `crates/server/src/observe.rs`: `ShardTime` (A-65) is shared, and its next window is also given in nanoseconds for a
  wait on a shard's own timer.
- Each call family moves to the shard-time wait in its own commit, with its own failing-first test:
  - **The consensus verbs** (`verbs.rs` `consensus_on_control`: bootstrap and recovery, the operation on the control
    shard and its publication on every other). Measured green (same host, load 35–49): the starved bootstrap
    acknowledged after 1.52–1.68 s over four runs. A wedged control shard (a 3 s poll off the CPU) is still refused
    `Overloaded` after 1.000–1.004 s. CI diag run of `diag-clients-r3a-1` (2026-11-07 UTC, 4-CPU runners): green on
    ubuntu x86_64 and arm64 and on Windows x86_64 and arm64, quiet and beside two spinning loops a CPU (acknowledged
    after 1.50–1.60 s, wedged refused after 1.00–1.12 s), and red on each over main's sources (refused after
    1.00–1.02 s).
  - **The consumer verbs** (`verbs.rs` `revoke_everywhere`, the record on the consumer's shard and the mark on every
    shard; and the attestation's read of the consumer's record). Measured (same host, load 107–136): over the
    unconverted code a revoke with the other shard starved was refused `NotFound` after 1.003 s and an attestation
    with its record's shard starved after 1.075 s; with the change they answered `Revoked` after 1.57–1.71 s and
    `Attested` after 1.61–1.80 s over three runs, and a revoke with the other shard wedged is still refused after
    1.01–1.09 s. CI diag run of `diag-clients-r3a-2` (2026-11-07 UTC): revoke green on the four Linux and Windows
    runners and attest on both Linux runners (it is Unix-only), quiet and busy, 1.50–1.59 s; red on each over main's
    sources (1.000–1.012 s). One busy round on ubuntu x86_64 failed in the test's own helper: it asked which shard
    served a client its connect had just returned, before the shard had taken the client's ring. The helper now asks
    again until a shard has, within twenty windows.
  - **The NFS bridge** (`nfs.rs`: `serve_remote`, a call served on its volume's shard; `gather_entries`, the host
    root's listing from every shard, which had one wall second for them all; `send_note`; and the v4 front end's and
    its v3 backend's `call_owner`, a state call at the state id's owner). Measured (same host, load 52–66): over the
    unconverted code a host-root listing with the other shard starved was refused `SYSTEM_ERR` after 1.262 s, and an
    NFSv4 `FREE_STATEID` at a starved owner was answered `NFS4ERR_DELAY` after 1.296 s; with the change the listing
    succeeded after 1.59–1.80 s and the owner's own `NFS4ERR_BAD_STATEID` came back after 1.52–1.58 s, over three runs.

## Tests

- `a_bootstrap_whose_control_shard_is_starved_is_acknowledged_past_a_wall_window`: red on main, green with the fix.
- `a_bootstrap_whose_control_shard_is_wedged_is_refused_after_a_window`: the guard on the other side, green on both.

- `a_revoke_with_a_starved_shard_is_revoked_past_a_wall_window`, `an_attest_whose_record_shard_is_starved_is_attested_past_a_wall_window`
  (Unix: an attestation binds only a uid principal): red on main, green with the fix.
- `a_revoke_with_a_wedged_shard_is_refused_after_a_window`: the guard, green on both.
- `a_host_root_listing_with_a_starved_shard_lists_past_a_wall_window` (NFSv3, every platform) and
  `an_nfs_state_call_to_a_starved_owner_hears_its_answer_past_a_wall_window` (NFSv4, Unix: the test's v4 client is):
  red on main, green with the fix. The v4 test learns which shard serves its compounds from the daemon's
  `nfs4.state.forwarded` count, and asserts the count moved, so its call did cross shards.

All use a client whose reply deadline is ten windows, so they measure the daemon's wait and not the client's. The
client's own deadline is the liveness window too; deriving that window is A-232 (classes C and D).

## Siblings

Still waiting on the wall second, each its own commit: the forwarded verbs (`verbs.rs` `serve_forward`,
`await_deferred_completion`), the direct map's delivery (`direct_map.rs`) and a peer's remote reap (`remote_attach.rs`
`serve_unforwardable`). The delegation's recall and the back channel's probe (`delegation.rs`, `nfs.rs`
`probe_back_channel`, both through `callback::call_on_slot`) also wait a liveness budget, but on the NFS client across
the network, not on a shard: that is class B (the transport's round-trip estimate).

Found on the way, not changed here:

- An attestation binds only a channel whose principal is `Principal::Uid` equal to the consumer's account
  (`verbs.rs` `verify_attestation`), so on Windows, where a channel's principal is a `Sid`, no consumer can ever be
  attested.
- On both Windows runners, `cargo test -p slates-server --lib` fails two landing tests on main's code too
  (`a_landing_of_a_named_snapshot_presents_that_snapshot_whatever_the_head_became`,
  `a_presentation_past_the_bound_is_refused_and_changes_nothing`; the daemon lands nothing on Windows). CI's Windows
  lanes do not run the server's unit tests, so nothing gates them there.
