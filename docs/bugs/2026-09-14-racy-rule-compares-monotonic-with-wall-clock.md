# The racy rule compared the volume's monotonic clock with the host's wall-clock timestamps: every production witness was racy (§4.5 copy-up, §4.15)

- **Date:** 2026-09-14
- **Subsystem:** the base plane's copy-up and listing cache (`crates/vfs/src/base.rs`: `load_listing`, `copy_up`); the daemon's volume clock (`crates/server/src/verbs.rs`, `HostClock::new()` per volume).
- **Severity:** performance, not correctness. A racy witness is one whose fingerprint cannot be trusted alone, so every drift check of it re-hashes the whole file through the descriptor (`check_drift` step 1 `identity_of`, step 2 `identity_now`; the landing engine hashes racy witnesses too, `crates/land/src/engine.rs` "if w.racy"). With every witness racy, every read of an unpinned range, every `status` and every landing verdict paid a whole-file read and BLAKE3 per witnessed entry, and every copy-up carried a racy flag that meant nothing. Drift was still detected (the re-hash is the conservative branch), so no torn or stale bytes were served.
- **Found by:** reading the clock domains while building the digest cache's racy rule (GAP-A9-13, 2026-09-13); proven by `crates/vfs/tests/base.rs::a_witness_is_racy_only_within_the_hosts_own_clock`, which fails before the fix (`a file a second older than the listing, at a microsecond granularity, is not racy`) and passes after.

## Symptom

In the daemon, `Witness.racy` was `true` for every copy-up, whatever the file's age. Nothing visible to a client; the cost was hidden in the drift checks.

## Root cause

`load_listing` stamped a listing's `read_at_ns` from `self.vol.clock.monotonic_ns()`. The daemon gives every volume `HostClock::new()` (`verbs.rs:1617`, `daemon.rs:1025`), whose `monotonic_ns` is `Instant::elapsed()` since the clock was made — nanoseconds since boot, small. `copy_up` then computed `racy = read_at - fp.mtime_ns <= granularity`, where `fp.mtime_ns` is the host's `stat` timestamp in wall-clock nanoseconds since the Unix epoch (`crates/base/src/unix.rs` `stamp_ns`), about 1.7 × 10¹⁸. A small number minus a huge one is negative, and negative is always at most the granularity: racy, always.

The simulated host hid it: `SimHost`'s timestamps start at zero while the test volumes use a `StepClock` starting at 10⁶, so the same subtraction happened to come out "not racy" for every fixture except the one that meant to be racy (T-1.11, granularity 10⁹) — right answers by accident of magnitudes, not by the rule. The oracle certified drift outcomes, which do not depend on the flag, so it could not see this.

## Fix

The host seam gained `HostFs::now_ns` (the host's clock in its fingerprints' own domain: `CLOCK_REALTIME` on Unix, FILETIME nanoseconds on Windows, the simulated clock in `SimHost`), and `load_listing` stamps `read_at_ns` from it; `copy_up`'s subtraction is now between two readings of one clock. `Listing.read_at_ns` carries the domain's type (`i64`).

Fixture consequence: with the rule now physical, the worked example's files, created and listed in the same simulated nanosecond, were racy — correctly. The fixture lets their tick close (`host.advance_ns(2)`) before the volume lists them, as a real disk written before the agent starts would.

## Sibling sweep

- `crates/vfs/src/base.rs` `copy_up` and `rewitness` still stamp `Witness.witnessed_at` from the volume's monotonic clock; that field is a volume-time record (when the witness was taken, for reports), not a filesystem comparison, so it stays.
- `keep_digest` (the digest cache's racy rule) was written against `HostFs::now_ns` from the start (`crates/vfs/src/base.rs`), so the digest cache never had this defect.
- The landing engine's verdict (`crates/land/src/verdict.rs`) consumes the flag and needed no change; its oracle (`crates/land/tests/oracle.rs`) passes with the physical rule (the worked example's calls remain equal across base sizes, AC-1.14).
