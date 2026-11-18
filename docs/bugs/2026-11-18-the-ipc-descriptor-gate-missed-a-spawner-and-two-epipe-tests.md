# The ipc descriptor gate missed a spawner and two EPIPE tests (2026-11-18, GAP-A9-9, A-161)

## Description

On macOS a pipe or a socket pair becomes close-on-exec only by an `fcntl` after `pipe` or `socketpair` (no `pipe2`, no
`SOCK_CLOEXEC`), so a child that another thread spawns inside that window inherits the end for its whole life. The
`slates-ipc` lib tests serialize the tests that spawn against the tests that assert a descriptor's last close
(`descriptor_test_gate`, 2026-10-06), but two later additions went around it: `mach_claim`'s stranger test spawns `sh`
(which re-runs the test binary) without the gate, and `stream.rs`'s two `EPIPE` tests make their pair without it.

## How it showed

g242's runs on 2026-11-18 (Adas-MacBook-Pro-3): 2 of 4 runs of `cargo test -p slates-ipc` failed one `EPIPE` test,
`a_whole_record_takes_once_and_the_descriptor_is_closed` (the pipe's write `Ok(1)`) and
`a_full_end_refuses_at_once_and_a_gone_peer_answers_epipe` (the send `EAGAIN`), each while the stranger's child ran
(its output follows the failure in both logs).

## Measurement

A Python probe on this Mac: 444,617 socket pairs beside 138 `sleep 1` spawns; 4 ends held, each for 1.06 to 1.18 s,
the child's life, and none for less, so a spawn takes along only what was made inside the window. A/B on a 15-core Mac
at load 47 to 53, each arm built in its own tree into its own fresh target directory and its test binary run directly,
interleaved: 8 of 100 runs failed without the fix (5 `EAGAIN`, 4 `Ok(1)`), 0 of 100 with it. (Two first A/Bs shared one
target directory between the two trees: cargo found the copied tree's sources older than the artifact and ran one stale
binary for both arms. Build each arm apart.)

## Fix (exact edits)

`crates/ipc/src/lib.rs` (the gate's rule, its holders named), `crates/ipc/src/mach_claim.rs` (the stranger's spawn holds
it), `crates/ipc/src/stream.rs` (both `EPIPE` tests hold it), `crates/ipc/src/rendezvous.rs` (the Linux refused
delivery's zero read holds it). Landed as 25a94ae7c.
