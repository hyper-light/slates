# The spawner parity test met another test's open descriptors

- **Date:** found 2026-11-27 (the TSan lane, diag CI 1 and diag CI 2, run 45986066740, job 140461888152); diagnosed
  the same day (diagnostics run 45991805330, branch `diag-measurer-tsan-spawner-1` at 566b11519); fixed 2026-11-27.
- **Status:** fixed in the tests. No product code changes.
- **Subsystem:** the spawner's tests (A-924), `crates/ipc/src/spawner/tests.rs`, and `delivery`'s wrong-kind test,
  `crates/ipc/src/delivery.rs`.
- **Severity:** a red lane. The TSan lane failed
  `spawner::tests::a_child_started_through_the_spawner_holds_what_a_direct_one_holds` in 2 runs of 2. The product is
  not affected.

## Symptom

```
the child through the spawner holds what a direct one holds
  left:  0 1 11 142 145 2 3 4 5
  right: 0 1 11 142 145 2 3 4 5 6
```

The direct child (`confined_spawn::spawn`) listed descriptor 6, and the child through the spawner did not.

## Root cause

The diagnostics run printed every descriptor of the test process and of each child, with its target and its open
flags. Just before both spawns, the test process held three descriptors without close-on-exec:

```
3 /home/runner/work/slates/slates/crates/ipc flags: 0300000
4 /dev/null flags: 0100000
5 /home/runner/work/slates/slates/crates/ipc/Cargo.toml flags: 0100000
```

They are `delivery`'s `a_directory_a_device_or_a_file_is_the_wrong_kind`, running in parallel: it opens ".",
`/dev/null` and its crate's `Cargo.toml` (and `/dev/tty`) with `OFlags` lacking `CLOEXEC`. That test holds
`descriptor_test_gate()`, but the parity test did not, so they overlapped. TSan's timing made the overlap likely.

- The direct child inherited all three, so `ls`'s own handle on the directory it lists moved from 3 to 6.
- The spawner had taken number 3 for its own end when it started, so its child held 4 and 5 but not 3. Its `ls`
  opened at 3.

So the listings differed by a number that shifts, not by what the children hold.

## Impact

A red TSan lane on main, 2 runs of 2. No product descriptor was found inheritable by this cause. The same listing
showed a held rendezvous socket inheritable in both children (`socket:[22118]`, flags 04002), which is the holder's by
design and is diagnosed separately.

## Exact edits

- `crates/ipc/src/delivery.rs`: `a_directory_a_device_or_a_file_is_the_wrong_kind` opens its four descriptors with
  `OFlags::CLOEXEC`. Its point, the wrong-kind refusal, does not depend on the flag.
- `crates/ipc/src/spawner/tests.rs`: every test that spawns a process holds `crate::descriptor_test_gate()` across it,
  as the gate's doc requires of every spawning test in the binary. That covers the exit test, the parity test, the
  killed group, the over-long frame and the spawn-cost test (which re-invokes the binary). The test of a descriptor
  received with a request spawns nothing and does not take it.
- `crates/ipc/src/lib.rs`: the gate's doc names the spawner's tests and this record's cause.

## Verification

- The red: the lane's two runs, and the diagnostics run's listing above (run 45991805330, `tsan-1.out`).
- On the MacBook (debug): `cargo test -p slates-ipc --lib -- spawner:: delivery::`, 19 of 19.
- The lane's rerun is main's (`run-requests.md`).
