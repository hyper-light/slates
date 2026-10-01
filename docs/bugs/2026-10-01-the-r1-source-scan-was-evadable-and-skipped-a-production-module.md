# The R1 source scan was evadable, and skipped a production module

**Date:** 2026-10-01. **Area:** `xtask` (structural check), `clippy.toml`, `slates-machine`, `slates-mem`,
`slates-land`. **Audit:** AUD-29-31 (P2). **Design:** R1, D-26, A-59.

## Description

The design called the structural check a proof that no write-capable syscall *links* outside
`slates-land`. It was a scan of source lines for spelled substrings (`std::fs::`, `libc::mkdirat`,
`rustix::fs::unlink`, …).

**Spellings it missed:**
- `use std::fs;` then `fs::write(..)`;
- `use std::{fs, io};`;
- `use libc::{mkdirat, openat};`;
- a glob (`use libc::*;`);
- an alias of a crate root (`use libc as c;`, `extern crate std as s;`);
- a multi-line import.

**What the resolved-path lints covered:** `clippy.toml`'s `disallowed-methods` named only the `std::fs`
writers. The `rustix` and `libc` calls that `slates-land` actually uses were guarded by the evadable scan
alone.

**A module it skipped:** the scan's test-module detection armed on any `#[cfg(test)]` attribute and stayed
armed until the next `mod`. In `slates-machine/src/segment.rs` a `#[cfg(test)] fn bytes_mut` armed it, and
the production `mod platform` after it was taken for a test module and never scanned. That module creates
the profile segment's shared-memory object: `shm_open` with `CREATE | RDWR`, `ftruncate`, and an `unsafe`
map. The `unsafe` count, which shares the detection, undercounted the crate by one.

## Root cause

A substring scan treated as a proof. Its line-local test detection was also never checked against a
fixture.

## Fix

- **Strengthened scan:** `source_violations` is now a pure function over a source file.
  - Beside the spelled check, it gathers each `use` statement (across the lines rustfmt wraps it over) and
    expands its tree (braces, nesting, `self`, aliases) to full paths. They are checked against the same
    host-path and write rules, with the spelled rule's prefix semantics.
  - It refuses a glob import of a write-capable module and an alias of a guarded root, since calls through
    those cannot be checked.
  - A `#[cfg(test)]` reaches only the item it sits on: the next non-attribute, non-comment line disarms it
    unless it opens a `mod`.
- **Resolved-path lints:** `clippy.toml` now also refuses the `rustix::fs` and `libc` write calls by resolved
  path, which aliases and macro expansion cannot hide.
- **Allowed sites, each in place with a reason:**
  - the landing seam (`impl LandFs for OsLand`, `link_unnamed`, `media_barrier`);
  - the two shared-memory objects (`slates-machine` segment, `slates-mem` shared object);
  - two test fixtures that write through a kernel mount or remove their own socket.
- **Budget:** the `slates-machine` unsafe budget is corrected 49 → 50 for the map that had gone uncounted.
- **Wording:** the documents now state the static check as what it is: two source-level checks plus the
  dynamic tracer, none a linker proof.

## Tests

- `structural::fixtures::every_spelling_of_a_write_call_is_refused`: ten fixtures, one per spelling listed
  above plus a call inside a `macro_rules!` body, all refused. The old substring scan caught only the
  `rustix::fs::mkdirat as make` line among them.
- `structural::fixtures::a_test_only_function_does_not_hide_the_next_module`: the production module after a
  test-only function is scanned, and a real test module stays exempt.
- `structural::fixtures::read_only_and_allowed_sites_pass`: read-only imports, the landing crate, and a
  reasoned `structural: allow` pass.
- Run against the tree, the fixed scan found the hidden `slates-machine` calls (then given their reasoned
  exemptions).
- The lints are clean on macOS, on Linux in Docker (`rust:1.98.0`, which found three Linux-only sites, now
  allowed in place), and in the Windows cross-lint (no unresolved-path warnings where `rustix` and `libc` are
  absent).

## Siblings reported

- `CLAUDE.md`'s R1 row still says the structural test proves a write syscall "links" nowhere but `land`.
  `CLAUDE.md` is Ada's file; the wording to match is A-59's.
- A capability review of dependencies (which third-party crates can write) is not automated. The dependency
  walk refuses named crates only.
