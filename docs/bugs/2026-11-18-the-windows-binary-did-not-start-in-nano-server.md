# The Windows binary did not start in Nano Server (2026-11-18)

## Description

The release lane's test tag `v0.1.0-test.3` (run 44326428333, commit 1cd57d18b, main at 14:38 UTC) failed one of
its fourteen build jobs, `slates-windows-x64.exe (build)`, at the step "The Windows binary starts in Nano Server":

```
slates version exited -1073741511 in Nano Server
```

The same bytes had passed their smoke test on the `windows-2025` runner a step earlier, and the target's second copy
(`rebuild`, which does not run the Nano Server step) passed too. The dry run of main at 843f527ff that morning (run
44266895044) passed the same step.

-1073741511 is 0xC0000139, `STATUS_ENTRYPOINT_NOT_FOUND`: Windows found every DLL the binary imports, but not every
function it imports from them, and refused to start the process before any of its code ran.

## Root cause

A-223 (aa9f7b9a0, merged into main between the two runs) added `crates/machine/src/switches.rs`, which called
`EnableThreadProfiling`, `ReadThreadProfilingData` and `DisableThreadProfiling` through `windows-sys`'s static
imports. The binary's import table gained exactly those three kernel32 functions: `llvm-objdump -p` lists 215
imported functions in run 44266895044's x86_64 binary and 218 in run 44326428333's (its `rebuild` copy), and the
three are the whole difference. Nano Server's kernel32 does not export them.

## Impact

- The release lane refused the release: `stage` needs every build job, so it could not run, and the test tag made no
  draft (its run was then cancelled with its two macOS jobs still queued, and the tag deleted). The tag `v0.1.0`
  waited for this fix.
- Every binary built from main since A-223 merged failed to start in a Nano Server container
  (`mcr.microsoft.com/windows/nanoserver`), the smallest Windows base image. The SDKs' Windows addons link
  `slates-machine` too (through `slates-client`, whose ipc wait reads the switch count since A-223's sibling (d)), so
  they would have failed to load there as well. Windows desktop and Server export the three functions and were not
  affected.
- Only the release lane runs the Nano Server check (on a tag or a dry run), so no push showed the regression; the test
  tag did, as it is meant to before a release tag.

## Exact edits

- `crates/machine/src/switches.rs`: the three calls are found in kernel32 at a thread's first read, with
  `GetModuleHandleW` and `GetProcAddress`, typed by the signatures Microsoft documents (`Enable`, `Read`, `Disable`),
  and kept in the thread's `Profiling`. Where kernel32 lacks any of them, `switches_now()` answers `None`, the answer it
  already gave when the OS refuses the profiling. The wake probe then reads its round's sleep as unknown
  (`SLEEP_UNKNOWN`), and a shard learns its park's wake without the unslept filter, as on macOS (`note_wake` in
  `crates/rt/src/shard.rs`). The binary imports none of the three now. It imported
  `GetModuleHandleW` and `GetProcAddress` already, for `slates-rt`'s run-time lookup of `NtQuerySystemInformation`
  (`thread_clock.rs`), the pattern this follows.
- `unsafe-budget.toml`: `slates-machine` 81 → 85 (the three typed function-pointer aliases and the lookup's block; the
  three calls' blocks were already counted).
- GAPS (the binary release lane's entry), the design's §4.1 status and A-357.

## Verification

Diagnostics branch `diag-g071-nano-1` (run 44332283768, 2026-11-18, `windows-2025-vs2026` and
`windows-11-vs2026-arm64` runners), the release lane's build (`+crt-static`, `/Brepro`) and its Nano Server step:

- **Red:** main 68148152d, without the fix: `slates version` exited -1073741511 (0xC0000139) in a process-isolated
  `nanoserver:ltsc2025`.
- **Green:** the same commit with the fix: exit 0, `slates 0.1.0`. Its import table (`llvm-objdump -p`, 217 functions)
  holds none of the three; it differs from that of the binary that started there on the morning of 2026-11-18 (run
  44266895044, 215) only by A-336's `CreateMemoryResourceNotification` and `QueryMemoryResourceNotification`, which
  Nano Server's kernel32 exports.
- **The count where kernel32 has the calls:** `cargo test --locked -p slates-machine --lib switches`
  (`a_blocked_thread_switched_and_a_running_one_did_not`) passed on Windows x86_64 and arm64: a sleep moved the
  thread's switch count, and two quick reads agreed.

The red arm's job reported failure after printing the reproduction, since GitHub's `pwsh` wrapper ends a step with the
last native command's exit code, here the reproduced 0xC0000139.
