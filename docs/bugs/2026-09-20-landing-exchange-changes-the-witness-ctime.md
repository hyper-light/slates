# A landing rejects the ctime change caused by its own exchange

Date: 2026-09-20. Design: §4.15 steps 4, 6 and 11; T-1.14/T-1.15.

## Reproduction and root cause

`SLATES_TEST_RAMDIR=/dev/shm cargo test -p slates-land --release --test os
-- --nocapture` fails the real kill/restart test: all 64 resumed replacements are
`Undone(TargetInUse)`. Logs in `/private/tmp/slates-landing-exchange-red.log` show
the same device, inode, size, mtime and mode before/after exchange, but ctime advances
from 1789941058193150013 to 1789941058241150013 ns. The landing's rename changes
ctime; whole-fingerprint equality mistakes its own metadata update for an outsider.

The simulated host never changed exchanged inodes' ctime, so its oracle missed this.
After making the simulator reflect the real syscall, the deterministic one-file
test advances the clock between validation and exchange and fails in 0.00 s with
the same conflict. Log: `/private/tmp/slates-landing-tick-red.log`.

The same real-kernel gate exposes a separate containment classification defect:
Linux `O_DIRECTORY|O_NOFOLLOW` rejects a symlink with `ENOTDIR`, while the adapter
recognizes only `ELOOP` as `EscapesTarget`. The symlink is not followed; its typed
refusal is wrong. The existing containment regression is the red test.

## Fix plan

Verify the file at the displaced hidden name, rather than assuming a descriptor opened
before exchange still names the displaced inode. First require the complete fingerprint
before exchange, so an earlier metadata-only change remains a conflict. Require device, inode, size, mtime
and mode to match the witness. If ctime changed, or the witness was racy, require a
bounded content hash equal to the witnessed bytes and a stable fingerprint across that
hash. A ctime difference alone must not discard the content check. Preserve the normal
fingerprint fast path when the complete fingerprint agrees and the witness was not racy.

Undo a failed verification before removing the temporary. Never unconditionally unlink
a hidden name on an error: after an exchange it may hold the original or outsider file.
Keep it for recovery if undo itself cannot run. Model the exchange's ctime change and
test valid replacement, same-timestamp outsider edits, preservation and idempotent replan.

On a refused target component, distinguish an unfollowed symlink from a regular file
using descriptor-relative `statat(SYMLINK_NOFOLLOW)`. Keep regular-file components as
`Unavailable(NotDirectory)` and retain `O_NOFOLLOW` for every actual open.

## Scope and limits

This repairs the exchange verification, not a new filesystem compare-and-swap primitive.
External mutation after the verification instant and contention with the undo itself
remain limitations of the existing exchange protocol. The historical verify-then-rename
degraded path remains separately reported. Further cleanup/recovery claims must be
proved by the crash oracle and the real kernel, not inferred from a passing simulation.

## Validation

All **18** landing oracle tests pass, including the new clock-advance and equal-timestamp
outsider histories and crash-at-every-write recovery (10.85 s on macOS). All **five** real
Linux landing cases pass in **0.06 s**, including containment, target identity and
kill/restart recovery. Logs: `/private/tmp/slates-landing-oracle-green.log` and
`/private/tmp/slates-linux-gates-landing-green.log`. Temporary fingerprint logs are removed.

The CI million-file landing benchmark also completes: the five 10,000-entry delta trials
measure **[13,657, 13,666, 13,700, 13,758, 13,796] ns/entry** on four Docker CPUs under
Linux 6.12.76 aarch64, tmpfs, Rust 1.98.0 release. Command: `SLATES_TEST_RAMDIR=/dev/shm
cargo run --release -p slates-land --example land_bench`; log:
`/private/tmp/slates-linux-cli-bench.log`. This is a local CI execution, not a quiesced
reference-machine ratchet; the ratchet reports its missing baseline explicitly.
