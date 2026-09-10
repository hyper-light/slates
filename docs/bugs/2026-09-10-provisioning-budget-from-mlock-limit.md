# The daemon refuses all provisioning when the OS grants little lockable memory

Status: **fixed** — the default provisioning reserve is derived from usable memory (`memory.available`),
not the OS lock limit; the lock limit governs only the `require_locked` path, best-effort (§4.2 D-12
honest degradation). Date: 2026-09-10.

## Description

On a machine where the OS grants little lockable memory — a locked-down container, an old default
`RLIMIT_MEMLOCK` (64 KiB), a CI runner — the daemon refuses **every** volume provision, even a 1 MiB
one, with `BudgetExceeded { available: 0 }`. Reproduced locally, matching the CI failure exactly:

```
$ ( ulimit -l 64; cargo test -p slates-client --test client )
thread 'the_typed_verbs_drive_the_lifecycle_and_refusals_are_typed' panicked at crates/client/tests/client.rs:93:
  called `Result::unwrap()` on an `Err` value: Refused(BudgetExceeded { available: 0 })
```

Surfaced when CI was restored (the workflow YAML had been unparseable, so no CI had run): the macOS and
Linux gates both failed here, as did the `slates-anchor` supervised-child test — every test that spawns a
real daemon.

## Root cause

The per-shard provisioning reserve was derived from the OS **lock limit**:

```rust
// crates/server/src/config.rs
let reserve = region_bytes(profile.lock.bytes, shards, MEMORY_CLASSES);  // lock.bytes / shards / classes
```

`profile.lock.bytes` is what the OS will let the process `mlock` (`RLIMIT_MEMLOCK`, the macOS wire limit,
or 0 when locking is refused — `crates/machine/src/probes.rs`). The reserve feeds the store's byte budget
and the arena size (`crates/server/src/daemon.rs`), so a tiny lock limit rounds the reserve to ~0 and the
budget admits nothing.

But **the arena is not locked by default.** Locking happens only when a client asks for it
(`require_locked`, `crates/server/src/verbs.rs`: `if require_locked && … arena_mut().lock()`), and the
memory crate's lock sequence is explicit that an unlocked region "stays mapped and **usable**, unlocked,
and counted" (`crates/mem/src/lock.rs`; D-12 "honest degradation"). The design's own §4.2 boot-order
failure matrix names this exact case: *"a locked-down CI container refuses `mlock`; the profile records
lock capacity 0, the diagnostic surface reports why residency cannot be established"* — i.e. **degrade and
keep serving**, not refuse. So capping the *default* provisioning budget at the lock *limit* contradicts
the design: default volumes come from usable RAM, whether or not it can be locked.

## Impact

A daemon on any machine with a small mlock limit — a container without `CAP_IPC_LOCK`, an old Linux
default, a CI runner — could not provision a single volume, defeating "laptop ≡ fleet, one code path"
(R8) and sub-50 µs provisioning (R9) on those hosts. It is not a data or safety bug (nothing is written;
the RAM is real), but a robustness/availability one. It was invisible until CI was fixed, because CI had
not been running.

## Fix (applied)

`crates/server/src/config.rs`: derive the default reserve from usable memory —
`region_bytes(profile.facts.memory.available, shards, MEMORY_CLASSES)` — with a comment citing D-12 and
the §4.2 failure case. `crates/mem/src/budget.rs`: `region_bytes`'s parameter and derivation note are
generalized from "lock capacity" to "memory capacity" (the caller decides the basis). The lock limit is
unchanged where it belongs: a `require_locked` volume still calls `arena.lock()`, which respects the mlock
limit best-effort and refuses if it cannot lock. On a machine that can lock freely the two bases are the
same — the lock probe already records `memory.available` when `RLIMIT_MEMLOCK` is unlimited.

Verified by reproducing under `ulimit -l 64` before and after: the client, anchor and server-`nfs_mount`
suites go from failing (`BudgetExceeded { available: 0 }`) to passing; the budget unit tests and normal
(high-mlock) runs are unchanged; `cargo xtask check` (structural/literals/unsafe) clean.

## Sibling sweep

`region_bytes` (the budget helper) had exactly one real caller (this reserve derivation); the other
`region_bytes` name is an unrelated `SharedObject`/buddy method. No other site derives a *default* budget
from the lock limit. The `require_locked` path is intentionally lock-bounded and left as is.
