# The KIND lane's probe image, for its two DAX legs. Lane-only: built by the lane, loaded into its cluster, never pushed
# or shipped.
# - `slates-pod-probe` (`crates/guest-probe/src/pod.rs`; `cargo xtask kind export`, its map leg; G-mount step 5) maps a
#   file through kubelet's NFS mount and waits for another path's change: the mount route.
# - `slates-pod-direct-probe` (`crates/cli/examples/pod_direct_probe.rs`; `cargo xtask kind direct`) maps a file by the
#   direct map from a container of the daemon's pod and says whether the map is zero-copy: the socket route.
#
# Built from the repository root like the slates image (`Dockerfile`), with the same toolchain and the same cache
# mounts, so the lane's second build reuses the first's dependencies. The runtime base is the slates image's, for the
# same reason (the binaries link the builder's glibc).

# syntax=docker/dockerfile:1.7
FROM rust:1.98.1 AS build
WORKDIR /work
ARG CARGO_BUILD_JOBS
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,target=/work/target,sharing=locked \
    RUSTUP_TOOLCHAIN=1.98.1 cargo build --release --locked -p slates-guest-probe --bin slates-pod-probe \
    && RUSTUP_TOOLCHAIN=1.98.1 cargo build --release --locked -p slates-cli --example pod_direct_probe \
    && cp target/release/slates-pod-probe /slates-pod-probe \
    && cp target/release/examples/pod_direct_probe /slates-pod-direct-probe

FROM gcr.io/distroless/cc-debian13:nonroot
COPY --from=build /slates-pod-probe /slates-pod-probe
COPY --from=build /slates-pod-direct-probe /slates-pod-direct-probe
ENTRYPOINT ["/slates-pod-probe"]
