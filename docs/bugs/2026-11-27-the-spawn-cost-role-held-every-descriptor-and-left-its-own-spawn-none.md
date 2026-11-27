# The spawn cost role held every descriptor and left its own spawn none

- **Date:** found 2026-11-27, on diag CI 10's macos-latest lane (run 46033104507, at 0f8e83640), and fixed the same
  day.
- **Status:** fixed in the test. No product code changes.
- **Subsystem:** the spawner's tests (A-924): `spawn_cost_role` and
  `a_spawns_cost_on_its_calling_thread_is_flat_in_the_daemons_descriptors` in `crates/ipc/src/spawner/tests.rs`.
- **Severity:** a red lane on macOS CI. The spawner is not affected.

## Symptom

Since 2386b45f1, the role's standard error is printed by its parent. On diag CI 10 it showed:

```
spawner/tests.rs:298:69: Result::unwrap() on Os { code: 24, kind: TooManyOpenFiles }
```

The role had printed the 100 and 10,000 rows, then `SPAWN-HELD 100000 10231`.

## Root cause

macos-latest caps a process at about 10,240 descriptors. That is below the 100,000 the role tries for, and below the
soft `RLIMIT_NOFILE` it was allowed to raise.

The role duplicates descriptors until the system refuses one, which leaves the process's table full. A direct spawn's
file actions then open the null device once per stream in the child's copy of that table, before its exec. At the cap,
each such open is refused `EMFILE`, so the role's direct spawn failed, and its unwrap ended the role before it printed
the row.

The parent then counted two rows against three counts and failed. That was not the spawner's doing.

## Exact edits (main's ruling of 09:45)

- `SPAWN_STREAMS` (3, derived from `confined_spawn::spawn`'s file actions) is left free. The most the role holds is
  its raised `RLIMIT_NOFILE`, or the system's own cap where lower, less those three.
- A count past that most is measured at it, as before. A count no larger than the last row's makes no row.
- A refused direct spawn fails naming the descriptors held and the soft limit, instead of an unwrap.
- The parent fails a role that failed, naming its status and standard error. With fewer than two rows there is no
  flatness to judge, so it skips, saying so. Otherwise it judges its last row against the first.

## Verification

- The red: diag CI 10's macos-latest, above. The MacBook's own cap is far higher, so it never reached this here.
- The focused test on the MacBook: recorded in the landing's commit message.
- Owed (main): the next diag CI's macOS lane.
