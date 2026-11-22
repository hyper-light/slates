# Vendored mantle crates

The crates slates takes from mantle (`../mantle`, github.com/hyper-light/mantle) for its version-control store
(A-767): the metadata engine, the chunk store, the device layer and their codecs. Taken at mantle `87ab09e1`
(`SNAPSHOT`, 2026-11-08), Ada's ruling of 2026-11-20 ("Copy into slates"; "using Mantle's K/V store for metadata").
Their block layer and log, `hyper-block` and `hyper-log`, are hyper-raft's and live in `../hyper-raft` with the rest
of hyper-raft (its README, "Two snapshots").

| Crate | What slates takes it for | Upstream |
|---|---|---|
| `mantle-engine` | the K/V engine under the store's rows (A-767 item 3): `ShardDb`, SplinterDB's size-tiered B-epsilon tree in mantle's format, its pages checksummed, checkpointed by a double superblock (mantle `docs/design/engine-structure.md`) | `crates/engine` |
| `mantle-chunk` | the derived store: a log-structured volume of checksummed records (mantle `docs/design/chunk-store.md`) | `crates/chunk` |
| `mantle-disk` | the device's identity and measurement (block size, write unit, flush cost), which the store's layouts derive from | `crates/disk` |
| `mantle-crc` | CRC-32C and CRC-64/NVME, every page's and record's checksum | `crates/crc` |
| `mantle-codec` | the byte codec the chunk store's records use | `crates/codec` |

Each directory holds the crate's `src/` as taken, with the local changes below, and a manifest of slates' own (its own
workspace root, outside slates' workspace, whose dependencies name the vendored copies by path). Upstream's
integration tests, benches and the workspace's gates stay in mantle. The engine's unit tests (in its `src/`) run here
too, with the one support file they include (`engine/tests/support/shared_sim.rs`), because the runtime under them
changed: `TMPDIR=<a directory of your own> cargo test --manifest-path vendor/mantle/engine/Cargo.toml --lib`.

Formatting: mantle keeps upstream's own style (the default four-space indent), read from `vendor/rustfmt.toml`, which
every vendored crate without a `rustfmt.toml` of its own takes; so `cargo fmt --all` checks the vendored code against
the format it was written in and never reformats it to slates' (2026-11-22: without it, 1,435 diffs at main
2486c62e3). The local changes below are formatted the same way.

## Local changes (each marked `slates:` where it is not a path)

Main's ruling of 2026-11-20 on A-767 piece 2b: no `hyper-rt` (a second runtime, D-9) and no `unsafe impl Send`/`Sync`;
the vendored seams run on `slates_rt`, as patches recorded here.

1. **The runtime is slates'.** Every `hyper_rt::` path is `slates_rt::`'s or `slates_machine::`'s, and the manifests
   depend on `slates-rt` and `slates-machine` in `hyper-rt`'s place:
   - `hyper_rt::sync` (the bounded channel) is `slates_rt::sync`, which carries its surface with no `unsafe`: values
     cross through the standard library's bounded channel, a waiting task's word sits in hyper-rt's generational cell
     table (ported), and sender tasks waiting for room hold places granted in turn;
   - `hyper_rt::runtime::{RetirementLease, OriginalAdoption, OriginalFence, OriginalLease}` are
     `slates_rt::retirement`'s (ported), whose owners a `Reaper` holds where hyper-rt's `Runtime` held them; the two
     unit tests that asked a runtime for leases ask a `Reaper`;
   - `hyper_rt::machine::facts` is `slates_machine::facts`, the same functions by the same names (hyper-rt took them
     from slates);
   - `hyper_rt::combine::{race2, Either}` is `slates_rt::futures`'; `hyper_rt::error::RtError` is `slates_rt::RtError`
     (with `Capacity`, added for this);
   - in unit tests, `LocalRuntime::block_on` and `futures::spawn_detached` are `slates_rt`'s (added for this), and the
     runtime configurations lose the two fields slates' runtime has no use for (`interests_per_shard`,
     `latency_objective_ns`).
2. **`ranges` is left out.** `mantle-engine`'s `ranges` hosts many engines on hyper-rt's runtime; slates hosts each
   `ShardDb` on its own shard, so the module, and the unit test of it (`shard_db/room_seats.rs`), stay upstream. The
   async read paths only `ranges` called (`branch`, `scan`, `store`) are kept for slates-store's shard-hosted reads,
   so the crate root allows dead code outside tests until the store calls them.

## Evidence

Every crate builds with no warning, on slates' runtime, macOS arm64, Adas-Mac-Studio-2, 2026-11-20. Unit tests on
slates' runtime: `mantle-engine` 155 of 155, `mantle-chunk` 52 of 52 (with `chunk/tests/common/device.rs`), and
`hyper-block`'s 101 of 101, its issuer's among them. Two engine tests had to hold their `Reaper` as long as upstream's
runtime held its owners (in the case, before the runtime), and one asks the reaper's `shutdown` for the worker-join
failure the runtime's shutdown reported upstream; the tests' runtimes take two ring slots where upstream's took one
(slates' rings need two, AUD-29-33).

## Updating

1. Pick a mantle revision whose gates pass.
2. Replace each directory's `src/` with that revision's, apply the changes above again, and write the revision in
   `SNAPSHOT`.
3. Build each crate and run the engine's unit tests.
