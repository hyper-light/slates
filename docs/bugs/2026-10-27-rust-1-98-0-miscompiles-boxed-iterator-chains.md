# Rust 1.98.0 miscompiles boxed iterator chains, and slates was built with it

- **Date:** found 2026-10-27, while working on Shards (whose toolchain file records it), fixed 2026-10-27.
- **Status:** fixed: slates builds with Rust 1.98.1, and a test fails any build made by 1.98.0.
- **Subsystem:** the toolchain: `rust-toolchain.toml`, the pins in `.github/workflows/`, `.kiln/` and the `Dockerfile`.
- **Severity:** a compiler bug that can make safe code jump to address 0. 1.98.0 compiles some trait-object vtables
  with a slot that must hold a function pointer left null, so a call through that slot ends the process with SIGSEGV
  (Rust 1.98.1 release notes). slates boxes iterator chains of the affected kind:
  - `slates_vfs::names::NameEquivalence::folded` boxes `name.nfc().flat_map(char::to_lowercase)`;
  - `slates_vfs::dir::DirNode::iter` and `iter_from_hash` box chains over the directory tree's own iterators.
- **Exposure:** unproven for slates' own sites. Every slates suite passes on 1.98.0, so either none of them takes the
  null slot or none of the tests calls through it. The compiler bug itself is proven on this machine (below).

## Evidence

focal reduced the failure it met moving to 1.98.0 to one program: nested `impl Iterator` chains (`flatten`, `flat_map`,
`chain`) boxed as a `dyn Iterator`, whose boxed `next` jumps to address 0. hyper-raft and Shards carry it as a test,
and both moved to 1.98.1.

On this Mac (aarch64-apple-darwin, 2026-10-27), the reduction in a scratch crate outside every repository:

| toolchain | opt-level 0 | opt-level 3 |
|---|---|---|
| 1.98.0 | SIGSEGV (signal 11) | SIGSEGV (signal 11) |
| 1.98.1 | 1 passed | 1 passed |

And as slates' own test (`crates/wire/tests/toolchain.rs`): SIGSEGV built by 1.98.0, `1 passed` built by 1.98.1.

## Fix

- Every pin moves to 1.98.1, as the toolchain file asks: `rust-toolchain.toml`, the workflows' setup steps,
  `.kiln/ci.yaml` and `.kiln/nightly.yaml` (`rust:1.98.1`), the `Dockerfile`'s toolchain and the MCP conformance build
  script (7318e744). The workspace's `rust-version` stays `1.98`.
- 7318e744 said every pin had moved, and it had not. The same day, a sweep for every spelling of a toolchain
  (`rust:1.98`, `rust:1.98.0`, `RUSTUP_TOOLCHAIN=`, `-alpine`) found seven more:
  - the live-guest image and the Linux container lane's image (`ci/guest/Dockerfile`, `ci/linux-oci/Dockerfile`:
    `FROM rust:1.98.0`);
  - the KIND lane's pod probe image (`deploy/kind/pod-probe.Dockerfile`: `RUSTUP_TOOLCHAIN=1.98.0`, which made the
    image build the probe with 1.98.0 whatever its base held);
  - the conformance lane's container (`xtask/src/conformance/container.rs`: `rust:1.98.0`);
  - two floating tags, which take whichever 1.98 release a machine has cached: the slates image's and the probe
    image's `FROM rust:1.98`, and the release's musl builds' `rust:1.98-alpine` (`.github/workflows/release.yml`,
    `.kiln/release.yaml`).

  Each is now `1.98.1` exactly (`rust:1.98.1`, `rust:1.98.1-alpine`; both tags checked for linux/amd64 and
  linux/arm64 with `docker buildx imagetools inspect`).
- `crates/wire/tests/toolchain.rs` carries focal's reduction unchanged, so a stale toolchain or image fails the build
  on every platform CI tests `slates-wire`.

## Verification

- Under 1.98.1 on macOS: format, workspace clippy `-D warnings`, `cargo xtask check` and CI's doc build all pass.
- The Windows lane's crates cross-lint clean for x86_64-pc-windows-msvc under 1.98.1 (`cargo xwin`).
- The new test fails under 1.98.0 and passes under 1.98.1.

## Siblings

- The historical records in `docs/` (benchmarks, audits, bug records) name 1.98.0 as what they ran on. They stay as
  written: they record what was measured. So do two comments that tell what happened on a date: the cross-lint
  lane's note on a cached `rust:1.98.0` image (`.github/workflows/ci.yml`) and the FUSE ABI test's
  (`crates/bridge-fuse/tests/abi.rs`).
- Only the setup steps' `toolchain:` lines carry the "keep in sync with rust-toolchain.toml" note. The image tags had
  no such note, and the first sweep looked only for those lines.
