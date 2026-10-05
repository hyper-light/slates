# Vendored hyper-raft crates

The crates slates shares with focal and mantle (`../hyper-raft`, github.com/hyper-light/hyper-raft), taken as a
snapshot at one revision whose CI is green on all six targets (`SNAPSHOT`: hyper-raft `f9a2c8e`, 2026-10-05; hyper-seal's source and hyper-timing's `ORIGIN.md` changed from `46d1035`, hyper-datagram and hyper-swim unchanged). Never a
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
