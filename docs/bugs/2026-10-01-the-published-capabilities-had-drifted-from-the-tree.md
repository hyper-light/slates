# The published capabilities had drifted from the tree

**Date:** 2026-10-01. **Area:** `README.md`, `docs/cli.md`, `.github/workflows/ci.yml`, `xtask`. **Audit:**
AUD-29-32 (P2), first half.

## Description

The README described a build that no longer exists. It said the `slates grant` command did not exist, that
Linux and Windows could not mount, and that the fleet was not wired, while the tree has all three (with
limits). The CLI guide said a daemon restart can lose volume bytes, which a passing test contradicts. CI
named three lanes "nightly" that run only on pushes to main; the workflow has no schedule.

## Root cause

The capability text was prose with no tie to the evidence, so nothing failed when the code moved on.

## Fix

- **A registry.** `xtask/src/capabilities.rs` holds one row per capability: status (works, limited or owed),
  where it runs, its limits, and the named tests that prove it.
- **A generated table.** The README's "What works today" table is rendered from the registry
  (`cargo xtask capabilities --write`).
- **A check.** `cargo xtask check` fails when any of these holds:
  - the README's table differs from the rendering;
  - a cited test does not exist, is not a `#[test]`, or is `#[ignore]`d;
  - a "works" or "limited" row cites no test;
  - an "owed" row names no gap.
- **Corrected prose.** The README's stale notes give way to the table and an accurate grant note. The CLI
  guide's restart sentence now cites its test.
- **Accurate lane names.** CI's "nightly" lanes are named for what they do ("on every push to main; no
  nightly schedule"). A real schedule would add a daily full CI run, which awaits Ada's approval.

## Tests

- `capabilities::tests::every_cited_test_exists_and_runs`: the registry's 14 rows against the tree.
- `capabilities::tests::a_missing_or_ignored_proof_is_reported`: a nonexistent test and an ignored
  measurement are both reported.
- `cargo xtask check` prints `capabilities: ok (14 rows, each proof a runnable test)`.

## Still owed (AUD-29-32's second half)

- Callgrind regression gates against recorded baselines, failing on a missing baseline.
- An executable i686 test lane.
- TSan.
- A nightly schedule (Ada's approval).
