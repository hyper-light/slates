# The slates image (docs/deploy.md; the KIND lane of docs/wip/kind-lane.md): the `slates` binary alone,
# release profile (`panic = "abort"`, LTO — the workspace's `[profile.release]`), on a distroless base with
# no shell, no package manager and a non-root user. The anchor is PID 1: it installs its own SIGTERM/SIGINT
# handlers (`crates/cli/src/signal.rs`), supervises exactly one child (the daemon, `try_wait`-reaped by
# `crates/anchor/src/supervise.rs`) and spawns nothing else, so no init shim is needed to reap orphans —
# there are none — and a pod's SIGTERM stops the daemon and the anchor together (exit 0).
#
# Built for the builder's own architecture (the release lane builds x86_64 and aarch64 Linux the same way);
# `docker build` from the repository root. The three cache mounts keep the registry, the git checkouts and
# the target directory across builds, so a rebuild after a source edit is incremental.
#
# The image runs as `nonroot` (uid 65532): the daemon asks for no capability (R10). Everything it needs at
# run time is RAM: the anchor segment is a memfd, the client rendezvous an abstract socket, the fleet planes
# UDP sockets (R1; nothing under /etc/slates is written — the manifest and certificates are mounted read-only).

# syntax=docker/dockerfile:1.7
FROM rust:1.98 AS build
WORKDIR /work
# The caller can bound build concurrency for a supervised validation run.
ARG CARGO_BUILD_JOBS
COPY . .
# `--locked`: the lock file is the contract. Select the image's installed toolchain explicitly
# so a release build does not download the development components in rust-toolchain.toml.
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,target=/work/target,sharing=locked \
    RUSTUP_TOOLCHAIN=1.98.0 cargo build --release --locked -p slates-cli \
    && cp target/release/slates /slates

# The runtime base is the builder's own Debian release (13, glibc 2.41): the binary links the builder's
# glibc, and a distroless of an older release refuses it at exec (`GLIBC_2.39 not found` on
# cc-debian12, measured 2026-09-14). Bump both together.
FROM gcr.io/distroless/cc-debian13:nonroot
COPY --from=build /slates /slates
USER nonroot:nonroot
ENTRYPOINT ["/slates"]
# With no arguments the image runs a solo node — the laptop degenerate (R8). The chart passes
# `anchor --fleet /etc/slates/fleet.json --node $(POD_NAME)` to make it a fleet node.
CMD ["anchor"]
