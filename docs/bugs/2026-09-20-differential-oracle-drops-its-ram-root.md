# The host differential oracle drops its RAM scratch root

Date: 2026-09-20. Design: AC-1.2/T-1.2, §4.5; `EQUIVALENCE.md` §3.

## Reproduction

After both complete workspace suites passed, the next Linux CI gate fails in **0.01 s**:
`SLATES_TEST_RAMDIR=/dev/shm PROPTEST_CASES=2000 cargo test --offline -p slates-vfs
--release --test differential -- --nocapture`.
Log: `/private/tmp/slates-linux-gates-extra.log`. The preceding million-operation oracle
passed. Environment: Debian 13, aarch64 Linux, Rust 1.98.0, ordinary-user test process.

The minimized history creates `a`, then attempts to hard-link `a` onto itself. The VFS
returns `EEXIST`; the host oracle returns `ENOENT`. The generated history also produces
successful VFS writes that the host calls missing files.

## Root cause and impact

`head_state` names files by absolute VFS paths (`/a`, `/directory/file`). The host adapter
passes these strings to `root.join(target)`. An absolute argument replaces the host root,
so link, write, truncate and edit look up `/a` on the host instead of the RAM case's `a`.
The test usually refuses before mutation; if a matching host file existed and was writable,
the oracle could modify it outside its declared RAM scratch. The VFS's result in this
minimal history is correct. Changing its refusal or allowing the mismatch would hide the
test defect.

## Fix

Translate an absolute VFS path into a path relative to the case root at one host-oracle
seam. Use that seam for all four file-selection operations. A deterministic history
creates root and nested files, links them, writes/truncates/edits through their aliases,
and compares the complete host and VFS states after each operation. Keep the generated
2,000-history gate and its errno policy unchanged.

The deterministic regression fails before the translation with `EEXIST` versus `ENOENT`
in 0.00 s. After the change, it and the unchanged **2,000-history** gate pass in **0.29 s**
on the same Linux container. Logs: `/private/tmp/slates-differential-selected-red.log`
and `/private/tmp/slates-differential-green.log`. Test roots include the test's name as
well as its process id so the two tests cannot remove each other's scratch directory.

The sibling sweep finds these four selected-file operations; generated namespace paths
already build beneath the RAM case root component by component. No VFS semantics or
errno equivalence changed. The preceding million-operation model gate also passed.
