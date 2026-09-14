# The fleet suite wedges in a churned `target/` and passes from a clean build of the same source

Date: 2026-09-13
Area: the build, not the source — `target/debug/incremental/slates_rt-*` after three same-day merges
that changed `crates/rt` (`58bda2f` transport: readiness; `0575386` loom/shuttle: the parking seam
`crates/rt/src/parking.rs`, `shard.rs`, `registry.rs`).
Severity: validation integrity — four suite runs on the merged tree wedged, three earlier suite
failures the same day were attributed to CPU load, and a correct merge nearly went unrecorded as
suspect.

## Symptom

`cargo test -p slates-server --test fleet` in the main worktree's `target/` **wedged 4 of 4 times**:
once at 8/30 (two shards of a *finished* test's daemons at 100 % CPU in a `kevent` loop, its
`Runtime::shutdown` joined forever, the next test blocked on the suite's serial lock — 24 min), then
three times at 18/30 (every shard parked in `kevent` with no kick, zero hot threads, no test completing
for 5 min). The stalled test differed each time. The failing tests passed singly (10.0 s, 6.0 s, 11.3 s).

## Root cause, by elimination and then by artifact

| Experiment | Source | Build dir | Result |
|---|---|---|---|
| base tree, own fresh worktree | `58bda2f` | fresh | 30/30, 171 s |
| merged tree | `0575386` | main's churned | wedge at 18/30 |
| merged tree, parking seam reverted to base | `0575386` − seam | main's churned | wedge at 18/30 |
| same seam-reverted source | `0575386` − seam | fresh (`target-clean`) | 30/30, 163 s |
| merged tree | `0575386` | main's, **wiped and rebuilt** | 30/30, 161 s |
| merged tree, the churned dir put back | `0575386` | churned (`target-wedged`) | wedge at 18/30 |

Same source, two build directories: the churned one wedges every time, the fresh one never. The
runtime seam was cleared explicitly (reverting it did not stop the wedge; its sender and receiver halves
read as equivalent to the base; every shard has a registry entry; the doorbell bypasses it). Feature
unification across the shuttle/loom dev-dependency graph is identical on every shared crate. No build
cache or wrapper is configured (`RUSTC_WRAPPER` unset, no `target-dir`, no sccache).

At the artifact: the churned directory's `libslates_rt-06c338d8b0572074.rlib` — same crate hash as the
clean build's, same 167 codegen units, rebuilt "fresh" at 21:00:17 when the suite was re-run there —
differs from the clean build's by 32 bytes, and its incremental sessions date from **18:27**, before
either `rt`-touching merge, while the clean build's date from 20:56. Cargo's fingerprint judged `rt`
current and incremental compilation reused stale codegen units for parts of it; the linked runtime
mixed two revisions of the parking protocol, which is a lost wake by construction — the shape loom
found in the *old* protocol and the shape every wedge sample showed. (Codegen-unit *names* are
per-session hashes — 167 vs 167 with none shared on any pair of builds — so they identify nothing;
the evidence is the A/B table and the session dates.)

## Fix

Rebuild the directory: `mv target <scratch> && cargo test -p slates-server --test fleet --no-run`
(a full workspace rebuild is ~10 s on this box, artifacts timestamped within the window). Done; the
merged tree is validated 30/30 (161 s) on the rebuilt directory. No source change.

## Impact on today's records (corrections)

Three earlier suite failures on this tree were attributed to CPU starvation and re-run to green in
the same churned directory: the virtio-fs merge (`dc4a780`: 2/30 at 1028 s), the transport merge
(`58bda2f`: 1/30 at 597 s), and the first loom validation. Each showed the same signature — passes
singly, many tests over 60 s — which is *also* what a stale-object lost wake looks like over a long run.
Those commit messages' "starvation" attribution stands as *plausible but unproven*; the reruns that
carried the record were genuine passes, and the merges are correct, but the diagnosis in those messages
should be read with this record beside it.

## Rules kept

- After merging anything that changes `crates/rt` or `crates/mem` (atomics ordering, parking, rings),
  wipe `target/` before the validating suite run.
- A suite hang whose tests pass singly is compared against the same binary in a fresh
  `CARGO_TARGET_DIR` before any load attribution is written.
- libtest's `--exact` accepts one name; several names with `--exact` filter to zero tests (a vacuous
  pass — hit once here). Run the whole suite.
- Every suite run keeps the stall detector: no `... ok` line for 300 s → `sample` the fleet pid, count
  `kevent`/hot threads, kill, record.
