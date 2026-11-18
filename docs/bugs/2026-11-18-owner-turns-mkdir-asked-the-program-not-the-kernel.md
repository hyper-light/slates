# owner_turn's refused mkdir asked the program, not the kernel (2026-11-18)

## Description

`a_mutations_reply_waits_for_its_barrier_and_a_refused_barrier_answers_eio`
(`crates/bridge-fuse/tests/owner_turn.rs`) arms its stand-in owner to refuse the next barrier, makes a directory
through the mount, and expects `EIO`. Since main moved its Linux lanes to Ubuntu 26.04 it failed on every run of CI's
`ubuntu-latest` gates job: "the refused barrier answered EIO: " with nothing on standard error. Runs: 43146258837
(push of 29ef4521, job 130944662601), every nightly from 43206782027 to 44077815398, and 44210937388 (push of
44018ec4, job 134488788092). GAPS had it as a rare race (1 failure, 3 of 3 passes on adacachy2); on the 26.04 runner
it is not rare: diag-slabs-58 (run 42715290546) failed 200 of 200 alone.

## Root cause

The test made the directory with `sh -c 'mkdir "$1/d"'` and judged the program's exit status and standard error.
Ubuntu 26.04's `/usr/bin/mkdir` is uutils coreutils 0.10.0 (`rust-coreutils`). diag-slabs-61 (run 43146145846, job
130944292325) traced it with strace and logged the request the owner refused:

```
owner_turn: refused the barrier of Some(MkDir)
4033  mkdir(".../slates-owner-turn-3999/mnt/d", 0777) = -1 EIO (Input/output error)
mkdir exit 0
```

The owner refused the right request, and the kernel told the caller `EIO`. The program then exited 0 with nothing on
standard error. The refused directory exists in the volume, as the design says a refused publication leaves it, and
the program counts a directory that exists as made. slates answered correctly; the test asked a program that hides
the answer.

The other images the test ran on (adacachy2's Arch, Debian's GNU coreutils in the conformance containers, Ubuntu
24.04) ship GNU `mkdir`, which reports the failure, so the test passed there.

## Impact

Test only: main's ubuntu gates job has been red on it since 2026-11-12, which hid every other result of the
`slates-bridge-fuse` suites after it on that lane. No change to the bridge or the owner's turn.

## Exact edits

- `crates/bridge-fuse/tests/owner_turn.rs`: the directory is made by `rustix::fs::mkdir` on the mount path, and the
  test holds the answer to `Err(Errno::IO)`. The following `test -d` check, that the directory is in the volume, is
  unchanged.

## Siblings

Swept every test in `crates/*/tests` that judges a shell program's failure (`!ok`, `!status.success()`, an expected
error in standard error). None but this one asks a coreutils program to report a refusal the volume can leave behind:
`crates/cli/tests/cli.rs` redirects in the shell (`write_with_the_shell`, the destroyed and the read-only mounts) or
`cat`s a path that no longer exists, and `owner_turn.rs`'s other refusals run util-linux's `fallocate`, which passed on
the 26.04 runner before this test reached its `mkdir`.

A consequence found with it: the ubuntu-latest gates job runs `cargo test --workspace`, which stops at the first
failing test binary, so no suite after this one ran on that lane while it failed.
