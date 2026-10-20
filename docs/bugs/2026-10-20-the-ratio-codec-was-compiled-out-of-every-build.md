# The ratio codec was compiled out of every build

- Date: 2026-10-20
- Area: §4.11 archive policy, D-17 (the ratio codec), §4.1 boot profile
- Status: fixed in this change
- Found by: tracing why `aarch64-pc-windows-msvc` could not be cross-checked (the C
  dependencies), which led to the workspace's `default-features` declarations.

## Description

The daemon never compressed an archive chunk. Every chunk was stored raw, on every platform, in
every build, since 2026-09-05. Nothing refused and nothing logged an error: the archive format is
self-describing, so raw chunks are valid archives, and the one signal — a note in the boot
profile — went to a field nobody read.

Measured on this machine (M5 Max, macOS 15.4, 2026-10-20) with the shipped binary:

```
$ cargo run -q -p slates-cli -- profile --json | ...
codecs entries: 0
notes:
  - codec probe not compiled in (feature `codecs` off)
```

`--quick` skips the codec probe deliberately (`crates/cli/src/daemon.rs:43`, `codecs: false`), so
the run above is the **full** profile, the one `slates anchor` takes at boot without `--quick`.
It asks for the probe (`ProfileOptions::default().codecs == true`) and gets nothing back.

## Root cause

One line, in the workspace root:

```toml
# Cargo.toml:18
slates-machine = { path = "crates/machine", version = "0.1.0", default-features = false }
```

`crates/machine/Cargo.toml` declares `default = ["codecs"]` and documents the intent: "The codec
probe (LZ4 and zstd) is on by default; a cross-compilation check that has no C toolchain for the
target turns it off and the profile says so in its notes." The gap ledger records why the root
declaration was added (§8, 2026-09-05): "the C dependencies are off for those checks behind
`slates-machine`'s `codecs` and `pure-hash` features".

The switch was correct in intent and wrong in placement. Put on the **workspace dependency**, it
applies to every build of every crate, not to the cross-target lint it was written for, and no
crate anywhere re-enabled `codecs`. Confirmed by resolution rather than by reading: in the
`slates-cli` graph, `lz4_flex` is reached only through `slates-archive`, never through
`slates-machine`, and `slates-machine`'s sole path to `lz4_flex` is its `codecs` feature.

The consequence runs through four hops, each of which behaves exactly as designed:

1. `probes::codecs` compiles to the `#[cfg(not(feature = "codecs"))]` stub returning `Vec::new()`
   (`crates/machine/src/probes.rs:393`).
2. `MachineProfile::measure` sees an empty list and pushes the note
   (`crates/machine/src/profile.rs:162`).
3. `codec_policy` turns an empty `profile.codecs` into `CodecPolicy::raw_only()`
   (`crates/server/src/config.rs:1214`) — documented as the degenerate case, "the same decision
   path with no candidates (R8)".
4. `codec::decide` returns `Verdict::Raw` for every chunk before it even samples
   (`crates/archive/src/codec.rs:368`).

No step is a bug. The defect is that step 1's input was never what the product intended.

## Why no test caught it

Every codec test builds its own policy and hands it in:

- `crates/archive/tests/archive.rs:239` — `measured_policy()`, synthetic rates.
- `crates/vfs/tests/special.rs:188,248` — `CodecPolicy::raw_only()` explicitly.

So the codec decision logic is well covered and the *wiring* from the measured machine to that
logic had no test at all. This is precisely the failure mode CLAUDE.md's non-vacuity rule names:
"a fast path exports a counter the test asserts moved, so a silently-dead reuse path can never
masquerade as a passing oracle". The codec path had no such counter.

## Impact

- Archives (§4.11, §4.16, Phase 7) were stored raw: more bytes on every replication and every
  export. No corruption, no refusal, no incorrect output — archives remained valid and
  round-tripped, which is why the suites stayed green.
- The boot profile carried a truthful note that no surface read.
- Not a security or durability issue: nothing was lost, mis-sealed or mis-verified.

## The fix

The binary that measures the profile asks for the probe, in `crates/cli/Cargo.toml`:

```toml
slates-machine = { workspace = true, features = ["codecs"] }
```

This restores D-17 for the product and leaves the library crates' cross-target lint C-free,
because `slates-cli` is not in those graphs. It is the same feature the design already defines,
requested where it is needed — not a second path and not a mode switch.

## The test that would have caught it

`crates/cli/tests/cli.rs::the_shipped_binary_measures_the_codecs_its_archive_policy_chooses_from`
runs the real `slates profile --json` and asserts the full profile measured at least one codec
point, that zstd is among them, and that the "not compiled in" note is absent. Red before the
fix at the note assertion, green after.

## Sibling sweep

Reported, not fixed here:

- **Still untested end to end, and now the narrowest remaining gap.** The new test proves the
  probe is compiled in and measures real points. It does not prove a daemon writes a compressed
  archive, because every test anchor starts with `--quick` (`crates/cli/tests/cli.rs:137` and
  five siblings), and `--quick` sets `codecs: false` on purpose so the suites do not pay 250 ms
  per process for the probe. So the policy the suites exercise is still `raw_only()`, and the
  first build to encode a chunk from a measured policy is the product. What is owed is one
  end-to-end test that boots a full-profile anchor, archives a compressible volume and asserts
  the chunks came back encoded — the counterpart to this one, on the far side of the wiring.

- `pure-hash` has the same shape — a cross-check feature that only takes effect if something
  enables it — but the opposite polarity: it is off by default, so production correctly gets
  BLAKE3's SIMD path and only a cross-lint turns it on. No defect.
- `slates-archive` is declared without `default-features = false` at the root, so its own
  `zstd` feature is on for every dependent. That is why zstd-sys still resolves for a
  cross-target check of `slates-vfs` and why `aarch64-pc-windows-msvc` cannot be checked from a
  host with no C toolchain for it. Tracked in GAPS §8; untouched by this change.
- Four other root dependencies set `default-features = false`: `aws-lc-rs`, `rustls`, `rcgen`
  and `lz4_flex`. Each pairs it with an explicit `features = [...]` list, which is the correct
  use of the switch — a curated feature set, stated at the declaration. `slates-machine` was the
  only one that turned defaults off and named no replacement, which is exactly what made it a
  silent off-switch rather than a choice. A root declaration with `default-features = false` and
  no `features` list is worth treating as a review smell.
