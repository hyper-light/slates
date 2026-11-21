# Vendored hyper-raft crates

The crates slates shares with focal and mantle (`../hyper-raft`, github.com/hyper-light/hyper-raft), taken as a
snapshot at one revision whose CI is green on all six targets (`SNAPSHOT`: hyper-raft `aa82ada`, 2026-10-12, green on
all six targets, Miri, the TLC models and the explorer: the revision mantle and focal re-vendor to as well). The four
crates' sources and `ORIGIN.md` are those of `67f038d` (2026-10-10, PR #17), unchanged upstream since. At `67f038d`,
hyper-swim's source and `ORIGIN.md` changed from `5d68252` (its items 14 to 20): the owner's detection budget floors a
judged period and is re-derived at run time
(`set_detection_budget`, `least_detection_bound`); a verdict's margin is searched over every margin and a false
condemnation priced (`set_condemnation_cost`); a pair's fit to the pool follows its latest evidence; a death counts
once it stands; every probe is judged, by its pair's RFC 6298 timer until an estimator judges it; a relay judges its
own leg to the target and nacks, and each acknowledgement states when a relay's response is due (`relay_within_ns`,
beside `promise_ns` for a per-grant lease, which the detector does not read). hyper-timing lost the capped
configurator only hyper-swim used. slates' local detector patches (`53e182e6`, `e6640dd3`, `bee1bb41`, `976a23d8`)
are gone, each upstream in its own form (`docs/bugs/2026-10-10-a-relay-reachable-peer-was-condemned-by-a-relay-stage-timed-from-stale-direct-spans.md`,
"Upstream"). hyper-datagram and hyper-seal are byte for byte `5d68252`'s, unchanged upstream since. All four are as
taken at `67f038d`: hyper-swim's per-peer `OUTSTANDING` bound is public (slates' member plane imports it), and
hyper-seal's `sealed_file` sits behind a `files` feature, the only thing that enables `hyper-block`, a file device.
slates leaves `files` off. Its hyper-seal manifest declares the feature as a known, unset value. Since A-767
`hyper-block`, `hyper-log` and `hyper-sim` are vendored too (below), for the changeset log that `slates-store` writes
under a repository's standing grant (R1's reading for repositories); no other crate links them. Never a
path or git dependency on the shared repository: slates builds what it reviewed (A-52 §5,
`docs/wip/transport-quic.md` §5; the scheme mantle and focal use).

| Crate | What slates takes it for | Its origin (`ORIGIN.md`) |
|---|---|---|
| `hyper-timing` | the detector's estimator and configurator, the election law | focal-timing and slates' election law |
| `hyper-swim` | SWIM membership and its per-pair detectors | slates' detector at `5cce86a` |
| `hyper-datagram` | the sealed control-datagram plane | after slates' seal |
| `hyper-block` | aligned direct I/O, the platform's full flush (`fdatasync`, `F_FULLFSYNC`, `FlushFileBuffers`), kept records, group commit's wait, the simulated power-loss file (`sim`) | mantle's `crates/disk` at `147f035` (`ORIGIN.md`) |
| `hyper-log` | one log a device, group-committed, its frames, recovery and the stop-at-the-first-hole rule: the changeset log's substrate (A-767) | mantle's `crates/log` at `147f035` |
| `hyper-measure` | a dev-dependency only: the allocation and page-fault counts some unit tests of `hyper-block` and the engine read | hyper-raft's measurement crate |
| `hyper-sim` | the deterministic world `hyper-block`'s `sim` runs in (crash tests of the changeset log) | hyper-raft's simulator (docs/sim.md) |
| `hyper-seal` | sealing at rest: the key hierarchy, STREAM, the ML-KEM-1024 recipient wrap, keyed names, keys in locked memory (A-92) | mantle's object seal, with focal's and slates' reviews (`docs/seal.md`) |

`rustfmt.toml` is hyper-raft's own (four-space indent), so `cargo fmt --all` checks the vendored code against the
format it was written in and never reformats it. Each directory holds the crate's `src/` and `ORIGIN.md` unchanged and a manifest of slates' own (its own workspace
root, outside slates' workspace). hyper-raft's `LICENSE` covers them. Tests, benches and the shared workspace's lint
wall stay in hyper-raft, whose CI runs them on all six targets. hyper-datagram's `prebuilt-nasm` feature is left to
slates' build settings (CI builds NASM from source).

**Two snapshots (A-767, 2026-11-20).** `hyper-block`, `hyper-log` and `hyper-sim` are hyper-raft `5e40eb9`'s (each
directory's `SNAPSHOT`), as mantle vendors them at mantle `87ab09e1`; the four above stay at `aa82ada`. `hyper-log`
builds against `aa82ada`'s `hyper-seal` and `hyper-timing` unchanged: its unit suite (18 of 18) and its `log`, `confirm`
and `group` suites (65, 2 and 14 of 14) passed against them, macOS arm64, Adas-Mac-Studio-2, 2026-11-20 (run from
hyper-raft's tests, which stay upstream; run before `unwind.rs`, dead code once the issuer was gone, was left out, and
the final tree builds both crates, `sim` on, with no warning). One revision for all seven is owed at the next re-vendor.

**Local changes** (each marked `slates:` in the source), to be proposed to hyper-raft's owner as a feature gate so the
sources return to upstream's bytes:
1. `hyper-block`'s device issuer (`src/issuer.rs`) runs on slates' runtime: every `hyper_rt::` path in it is
   `slates_rt::`'s, whose `sync` (the bounded channel, A-767 piece C2) carries hyper-rt's surface with no `unsafe`, and
   the manifest depends on `slates-rt` in hyper-rt's place (main's ruling of 2026-11-20: no hyper-rt, no `unsafe impl
   Send`/`Sync`; CLAUDE.md §2 item 2, D-9). Because the runtime under it changed, its unit tests run here (its
   manifest lists their dev-dependencies): `TMPDIR=<a directory of your own> cargo test --manifest-path
   vendor/hyper-raft/hyper-block/Cargo.toml --lib`, 101 of 101 on macOS arm64, Adas-Mac-Studio-2, 2026-11-20.
2. `hyper-block`'s `threads::macos::thread_cpu` reads the kernel's thread CPU clock as given, without hyper-rt's
   restatement at the kernel's tick rate, which only a process Rosetta 2 translates needs; slates ships native arm64
   and x86_64 builds only. Its test times the spin by `std::time::Instant` (3 of 3 `threads` tests pass).
3. `hyper-block`'s `DeviceFile::adopt` takes a file another crate opened (slates' `land`, relative to a store root its
   standing grant names, `slates_land::store_root`), made direct after the open where asked and the platform can
   (`F_NOCACHE` on macOS, `O_DIRECT` through `F_SETFL` on Linux) and buffered elsewhere, as `open` falls back; `open`
   and `adopt` share the steps after the open (`finish`). So the store's files are never opened by path in a vendored
   crate (A-767 C2; main's ruling: the engine's disk writes sit behind `land`'s write seam).

A change slates needs in one of these crates is proposed to hyper-raft's owner, lands there once every consumer's suite
passes, and reaches slates as a new snapshot in its own gated commit.

## Updating

1. Pick a hyper-raft revision whose CI is green on all six targets.
2. Replace each directory's `src/` and `ORIGIN.md` with that revision's (`git archive <rev> crates/<name>/src
   crates/<name>/ORIGIN.md`), and the manifests' dependencies if the crate's changed.
3. Write the revision in `SNAPSHOT` and run slates' gates.
