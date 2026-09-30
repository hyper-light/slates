# A simulation freed its clock under a protected reference (undefined behaviour, found by Miri)

## Description

- **Where CI caught it.** CI's Miri lane on `e7a0a6f` (job 109804433830) failed in
  `crates/rt/tests/ownership.rs`, `a_kept_handle_held_past_its_runtime_answers_none`, at `drop(sim)`.
- **The report.** "not granting access to tag <178846> because that would remove [SharedReadOnly for
  <216959>] which is strongly protected", raised by `Box::from_raw` in `SimRuntime::drop`.

## Root cause

- **The clock was a reference field of the struct that frees it.** `SimRuntime` held its simulation clock
  twice: as the owning raw pointer `clock_allocation`, and as a `clock: &'static SimShared` field.
- **Passing by value protects reference fields.** When a `SimRuntime` is passed by value (`drop(sim)` is a
  function call), Miri retags and protects the references among its fields for the call's duration.
  `Drop::drop` then freed the clock while the protected `clock` field still pointed at it, which is
  undefined behaviour under Stacked Borrows.
- **Why it was not seen before.** It had been latent since 2026-09-16. It surfaced now because the
  ownership test is the first Miri-run test to pass a simulation to `drop`.

## Fix

- **No reference field.** `SimRuntime` keeps only `clock_allocation`. `SimRuntime::clock(&self)` lends
  the clock for the runtime's borrow, which always ends before `Drop`.
- **Budget.** The unsafe budget for `slates-rt` rises 61 → 62, for this one read, with its reason in
  `unsafe-budget.toml`.

## Evidence

- **Local Miri run.**
  `MIRIFLAGS="-Zmiri-ignore-leaks" cargo +nightly miri test -p slates-rt --test ownership --test timers`
  gives 5/5 and 6/6, with no undefined behaviour.
- **Red case.** CI's run of the old code is the red case.
