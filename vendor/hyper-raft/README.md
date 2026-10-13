# Vendored hyper-raft crates

The crates slates shares with focal and mantle (`../hyper-raft`, github.com/hyper-light/hyper-raft), taken as a
snapshot at one revision whose CI is green on all six targets (`SNAPSHOT`: hyper-raft `43aec4a`, 2026-10-12, green on
all six targets, Miri, the TLC models and the explorer, CI run 38352011104: the revision mantle and focal re-vendor
to as well). The four crates' sources and `ORIGIN.md` are those of `67f038d` (2026-10-10, PR #17) but for hyper-timing's
`src/link.rs`: at `43aec4a`, `LinkEstimator::independent_interval` is gone, with the interval moves hyper-liveness
made from it (a link its correlation refuses now waits at its interval for a longer level), and slates calls it
nowhere. At `67f038d`,
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
slates leaves `files` off and does not vendor `hyper-block`: it writes files only in `land` (R1). Its hyper-seal
manifest declares the feature as a known, unset value. Never a
path or git dependency on the shared repository: slates builds what it reviewed (A-52 §5,
`docs/wip/transport-quic.md` §5; the scheme mantle and focal use).

| Crate | What slates takes it for | Its origin (`ORIGIN.md`) |
|---|---|---|
| `hyper-timing` | the detector's estimator and configurator, the election law | focal-timing and slates' election law |
| `hyper-swim` | SWIM membership and its per-pair detectors | slates' detector at `5cce86a` |
| `hyper-datagram` | the sealed control-datagram plane | after slates' seal |
| `hyper-seal` | sealing at rest: the key hierarchy, STREAM, the ML-KEM-1024 recipient wrap, keyed names, keys in locked memory (A-92) | mantle's object seal, with focal's and slates' reviews (`docs/seal.md`) |

`rustfmt.toml` is hyper-raft's own (four-space indent), so `cargo fmt --all` checks the vendored code against the
format it was written in and never reformats it. Each directory holds the crate's `src/` and `ORIGIN.md` unchanged and a manifest of slates' own (its own workspace
root, outside slates' workspace). hyper-raft's `LICENSE` covers them. Tests, benches and the shared workspace's lint
wall stay in hyper-raft, whose CI runs them on all six targets. hyper-datagram's `prebuilt-nasm` feature is left to
slates' build settings (CI builds NASM from source).

A change slates needs in one of these crates is proposed to hyper-raft's owner, lands there once every consumer's suite
passes, and reaches slates as a new snapshot in its own gated commit.

## Updating

1. Pick a hyper-raft revision whose CI is green on all six targets.
2. Replace each directory's `src/` and `ORIGIN.md` with that revision's (`git archive <rev> crates/<name>/src
   crates/<name>/ORIGIN.md`), and the manifests' dependencies if the crate's changed.
3. Write the revision in `SNAPSHOT` and run slates' gates.
