# Test fixtures measured the machine beside each other

Date: 2026-09-25. Scope: test fixtures, not the product. Contracts: §4.1 (the profile is measured
once, at the anchor's boot; `MeasurementTimeout` is its refusal), CLAUDE.md §4 (tests never assume a
timing; tests are isolated from each other's runs). Found by CI run 36202635768 (`85a29fb`, macOS gates).

## Symptom

`cargo test -p slates-server --lib` failed on the macOS runner:

```
daemon::tests::an_independent_group_cannot_supply_consensus_messages_or_join_state
panicked at crates/server/src/daemon.rs:2633:6:
the machine profile measures: MeasurementTimeout { probe: "wake" }
```

The test is about consensus messages, not the machine. The refusal came from its fixture's machine
profile. Before `85a29fb` the same fixture got a zero or near-empty wake mean instead of a refusal
(`2026-09-25-the-wake-probe-reported-a-zero-mean-when-it-measured-nothing.md`).

## Evidence

Every audit fixture (`audit_on_shard_configured`) measured its own profile with a 5 ms budget per
probe. The config tests used 1 ms. So did each integration test's `profile()` helper, once per daemon.
libtest runs as many tests at once as the host has CPUs, so the wake probes ran beside each other and
beside the other tests' daemons.

With a temporary log of each fixture's wake result (macOS, 18 CPUs, one run of the server unit tests):

| Measurement | Wakes kept | Mean |
|---|---|---|
| Each fixture its own profile | 16–160; three at 16, 20 and 23 (the floor is 16) | 1.6–53 µs in one run |

The 16 is the stopping rule's floor, so a slower machine keeps fewer and refuses. To give each test
binary a small runner's share of the CPU, six copies of the binary ran at once (18 CPUs, so three
per copy, as on the macOS runner):

| Six copies at once | Refused `MeasurementTimeout` |
|---|---|
| Before: each fixture its own profile | 33 fixture measurements across 18 runs (20, 12 and 1 in three rounds) |
| After: one profile per process | 0 across 30 runs; the one measurement kept 92–157 wakes, mean 3.4–6.9 µs |

## Root cause

A test fixture measured the machine each time it started a daemon. Production measures once, at
the anchor's boot, and every daemon derives from that one profile. The fixtures' measurements ran
concurrently, so each measured the other probes and daemons, not the machine, and needed a budget
the concurrent load did not leave. The tiny budgets (1–5 ms) assumed a quiet machine: a timing
assumption, which is a test defect. The daemon under test then derived its spin window, quantum and
ring sizes from a mean that varied 33-fold between tests in one run.

## Fix

- Each test binary measures the profile **once per process** and every fixture derives from it:
  - `crates/server/src/daemon.rs`: `test_profile()` (the audit fixtures, the warm-votes test and
    the config tests).
  - `crates/server/tests/common/mod.rs`: `machine_profile()` (every server integration test; `daemon.rs`
    and `attach_forms.rs` now include `common`).
  - `crates/client/tests/client.rs`, `consumer.rs`: memoized `profile()`.
- The profile sits in a `OnceLock` with its `Result`. A refusal fails every test that needs the
  profile, with the refusal named, instead of an arbitrary subset.
- A binary that measures once already (`async_core`, `reap`, `mcp`, the memory region test) is
  unchanged. So are the machine crate's own profile tests, whose subject is the measurement.

## Found on the way (fixed in the same change)

The same six-copy run failed `slates-machine`'s `a_corrupted_header_is_unavailable_not_a_panic`:
`OsRefused { call: "shm_open", code: Some(17) }`, `EEXIST`. The profile segment tests created named
objects under fixed names (`slates-profile-test-a` … `d`). Two overlapping runs of the binary, such
as another worktree's or another agent's in this tree, meet each other's object. On macOS
the create first unlinks the name, so the second run either takes the first's name from under it or
is refused. Each name now carries the process id:

- `crates/machine/src/segment.rs` (4 names; kept under macOS's 31-byte shm limit with the uid);
- the same class in `crates/ipc/src/endpoint.rs` (2), `crates/anchor/tests/anchor.rs` (4; the
  re-invoked child learns the name from the handoff) and `crates/mem/src/shared.rs` (1). The
  Linux-only tests use `memfd`, whose names never collide, and are unchanged.

After: the machine tests passed 30 of 30 runs as six concurrent copies (one failure in 18 before). The
anchor, shared-object and endpoint tests pass on macOS and in the Linux container.

## Tests

The regression evidence is the six-copy differential above. A test that asserted the refusal away
under load would test the host's load, not behaviour. The zero-budget refusal tests in
`crates/machine` already pin the probe's refusal deterministically.

## Edits

- `crates/server/src/{daemon,config}.rs`, `crates/server/tests/{common/mod,attach_forms,daemon,fleet,
  nfs_mount,observe,recovery,virtiofs}.rs`, `crates/client/tests/{client,consumer}.rs`.
- `crates/machine/src/segment.rs`, `crates/ipc/src/endpoint.rs`, `crates/anchor/tests/anchor.rs`,
  `crates/mem/src/shared.rs`.
- `docs/wip/TBD_FIXES.md`.
