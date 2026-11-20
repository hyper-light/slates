# Every Windows client's connect was refused `TouchesWord` (2026-11-20, A-717; found by use, goal 87)

Status: **fixed** by A-717 (`crates/ipc/src/rendezvous.rs` `marks_image`). Test:
`rendezvous::platform::tests::a_claim_reads_the_process_marks_of_a_published_bootstrap_object` (red on macOS arm64,
Mac Studio, 2026-11-20: `Memory(LayoutRefused { offset: 0, len: 64, reason: TouchesWord })`; green after).

## Description

On Windows, every client verb against a running daemon failed with exit 4 and
`slates: channel: shared memory: shared layout refused at 0 for 64 bytes: TouchesWord`. Found reproducing goal-83's
B3 (fsx over the WinFsp drive) on CI's windows-latest, diagnostics branch `diag-w-bugs-b3-4` (run 44684243854, main
ea05b2ad6): `cargo xtask conformance run --suite fsx` gave up with "the daemon did not come up within 60s (last exit
4)" while the anchor's log showed its daemon serving on 2 shards, and `slates volume list` answered the refusal on each
of 20 tries. No Windows lane could reach a daemon through a client, so no WinFsp suite (fsx, winfstest, the
version-control workloads) ran.

## Root cause

A Windows claim judges its daemon (A-447) and its anchor (A-468) by the process marks the bootstrap header holds at
bytes 24 to 48. `unanswered_end` and `holder_lives` read them by copying the whole 64-byte header with
`SharedObject::read(0, ..)` (28c4f3839, 3e93acc84, 2026-11-18). The header's bytes 8 to 24 are declared words
(`bootstrap_words`: the doorbell, the claim's mark, the start stamp), and `slates-mem` refuses a plain copy that touches
a declared word (AUD-29-09). Every test of the marks built a header's image by hand and never read one from an object
laid out with its words, and the callers are `cfg(windows)`, so no Mac or Linux run could meet the refusal; main's
Windows build was itself broken from 9abe7e9a2 until 36478561d, which hid it on CI.

## Fix

`marks_image` copies the marks' bytes alone (24 to 48, which hold no declared word) into a header-sized image at their
own offsets; both Windows callers read the marks from it.
