# Callgrind reports refused work as a successful benchmark

Date: 2026-09-20 (local). Design: D-20, Part 6, R5.

## Reproduction and cause

The approved Linux ARM64 run completes all 14 instruction benchmarks, but completion alone
does not prove the advertised work happened. A scratch copy of the wire benchmark flips
the first byte of its encoded frame. Its decoder reports `BadMagic`; the benchmark still
exits **0**. The benchmark converts the refusal to `false`, which the runner discards.
The raw runner entry is `callgrind --iai-run wire 5 0`; this exercises the same benchmark
function and teardown as the instrumented run, without requiring Valgrind on macOS.
Command preparation: `/private/tmp/slates-callgrind-negative-control/Cargo.toml`;
evidence: `/private/tmp/slates-callgrind-negative-red.log`.

Sibling review finds discarded allocation, removal, free, ring-send and task-spawn errors.
The runtime benchmarks do not require task completion. Ring setup substitutes another
capacity on refusal; ring and runtime setup can spin forever. Wire setup substitutes an
empty frame when encoding fails. These paths can benchmark less work and report a faster
successful result, or hang instead of reporting the actual refusal.

## Fix plan

Keep the existing workloads and their fixture sizes. Require setup success, propagate
operation refusals to each benchmark's teardown, and assert the expected returned bytes,
reclaimed capacity, delivered ring word and task completions there. Teardown verification
runs outside the counted function. Remove both infinite setup loops and the substituted
fixtures. Run the corrupt-frame control again, then every valid benchmark under the
approved Linux Callgrind profile, and lint all three benchmark targets.

The added result handling can change counts; old numbers that did not establish successful
work are not a justified baseline for these functions. A reproducible CI comparison and
enforced regression policy remain a separate open item. No latency ceiling changes here.

## Behavior validation and a measurement-boundary defect

`python3 /private/tmp/slates-callgrind-controls.py` builds pristine `2179cc2` and repaired
benchmark fixtures, then invokes their raw runner entries. Four negative controls distinguish
the old behavior from the repair:

| Control | Before | After |
| --- | --- | --- |
| Corrupt frame magic | Exit 0 despite `BadMagic` | Exit 1, `BadMagic`, 0.198 s |
| Ring capacity zero | Exit 0 using a substituted one-slot ring | Exit 1, `BadCapacity`, 0.195 s |
| Admit a task but omit execution | Exit 0 | Completion assertion fails, 0.197 s |
| Invalid runtime ring capacity | Still running at the 3 s diagnostic limit | Exit 1, `BadCapacity`, 0.229 s |

All 14 valid entries pass. Strict Clippy passes for all three benchmark targets before the
explicit-collection revision described below. Evidence:
`/private/tmp/slates-callgrind-controls.log`, `/private/tmp/slates-callgrind-clippy.log`.

The subsequent Linux ARM64 Callgrind run completes all 14 checks, but exposes another
defect: the CRC row includes the teardown oracle, **121,794 instructions** versus the prior
**2,651**. Disassembly places the oracle after the measured function's return; the profile
attributes it to that function anyway. Preventing oracle inlining does not repair the
boundary (**121,809 instructions**). This experiment is rejected. Logs:
`/private/tmp/slates-linux-callgrind-verified.log`,
`/private/tmp/slates-linux-callgrind-teardown-boundary.log`.

The initial claim that teardown necessarily stayed outside the count was wrong on this
build. The [Valgrind manual](https://valgrind.org/docs/manual/cl-manual.html) documents
explicit collection requests and warns about heuristic call/return detection on ARM;
the exact ARM64 mechanism here has not been established. No production CRC regression is
inferred from these counts.

The repair replaces function-return detection with explicit collection requests around an
owned operation closure. Collection ends inside the measured function, before verification;
input destruction remains inside the measurement. The pinned iai-callgrind API requires its
`client_requests` feature and libclang at build time. Each bench has an explicit
`instruction-counts` feature, so ordinary tests and Miri do not acquire that dependency.
Linux emits the requests; other hosts execute the same correctness checks. Ada authorized
libclang only in the disposable container; the host-side dependency fetch rejected by
automatic approval review was not executed.

### Small-operation boundary correction (2026-09-21)

Inlining both requests exposed a second measurement failure: header encoding reported
**zero instructions**. Its raw profile had `summary: 20` but `totals: 0`; the runner uses
`totals`. Disassembly placed the encode between the requests, ruling out the initial
compiler-hoisting hypothesis. Giving the collection request a real, non-inlined call
boundary repairs that measured failure. This differs from the rejected experiment above,
which prevented inlining of the verification oracle. The precise Valgrind accounting defect
has not been established; the control below proves the advertised operation is counted.

One trial reused an older binary because the experimental tar entry had an old timestamp.
That trial is excluded. The container wrapper now extracts with `tar -xm`, discarding archive
timestamps so Cargo recompiles the copied source. The final log shows the benchmark crates
being rebuilt. Evidence: `/private/tmp/slates-callgrind-header-boundary.log`,
`/private/tmp/slates-callgrind-results/header-profile-before.out`, and
`/private/tmp/slates-linux-callgrind-boundary-rebuilt.log`.

## Final validation (2026-09-21)

Serial Linux ARM64 container, four CPUs and 4 GiB memory, on the Apple M5 Max host;
Rust 1.98.0, Valgrind 3.24.0, iai-callgrind-runner 0.16.1 and container-only libclang 19.
Source is immutable `2179cc2` plus this change's manifests, lockfile, workflow and three
benchmark files. Concurrent snapshot-coverage edits are excluded from this validation.

- All **14 valid benchmarks pass**, including the restored run after fault injection.
- All four negative controls fail: bad frame magic (exit 1), invalid ring (exit 1),
  admitted but unexecuted task (exit 101), and invalid runtime setup (exit 1).
- The corrupt frame also fails the full instrumented runner (exit 1).
- Header encoding reports **34 instructions**; adding a second encode reports **58**.
  Restoring the original operation restores **34**.
- Multiplying only CRC verification work by eight leaves both `summary` and `totals`
  unchanged at **2,653 instructions**. The restored workload gives the same result.
- Strict Clippy for all targets of the three instrumented crates and `cargo xtask check`
  pass. Formatting and `git diff --check` pass. The ordinary test dependency graph excludes
  `bindgen`, `clang-sys` and `client_requests` (`cargo tree --offline --locked -p slates-mem
  -p slates-rt -p slates-wire -e features`). No production code or latency ceiling changes.

The executed benchmark command is:

```sh
cargo bench --offline --workspace --bench callgrind \
  --features slates-mem/instruction-counts,slates-rt/instruction-counts,slates-wire/instruction-counts
```

The bounded wrapper is `/private/tmp/slates-run-linux-callgrind-explicit-scope.sh`; controls
are `/private/tmp/slates-callgrind-linux-controls.py`; the final log is
`/private/tmp/slates-linux-callgrind-boundary-rebuilt.log`. Control logs are in
`/private/tmp/slates-callgrind-results/`. CI enables the same feature and lints the instrumented
benches before running them. These results establish successful work and collection boundaries
on Linux ARM64, not equivalence to GitHub's x86-64 runner. A saved comparison baseline and
enforced instruction-regression policy remain owed under D-20.
