# The KIND lane's map probe image (`cargo xtask kind export`, its map leg; G-mount step 5): the test-only
# `slates-pod-probe` binary of `crates/guest-probe` (`src/pod.rs`), which maps a file through kubelet's NFS mount and
# waits for another path's change. Lane-only: built by the lane, loaded into its cluster, never pushed or shipped.
#
# Built from the repository root like the slates image (`Dockerfile`), with the same toolchain and the same cache
# mounts, so the lane's second build reuses the first's dependencies. The runtime base is the slates image's, for the
# same reason (the binary links the builder's glibc).

# syntax=docker/dockerfile:1.7
FROM rust:1.98 AS build
WORKDIR /work
ARG CARGO_BUILD_JOBS
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,target=/work/target,sharing=locked \
    RUSTUP_TOOLCHAIN=1.98.0 cargo build --release --locked -p slates-guest-probe --bin slates-pod-probe \
    && cp target/release/slates-pod-probe /slates-pod-probe

FROM gcr.io/distroless/cc-debian13:nonroot
COPY --from=build /slates-pod-probe /slates-pod-probe
ENTRYPOINT ["/slates-pod-probe"]
