#!/bin/sh
# linux.sh in a privileged Linux container, for a host whose own kernel is not Linux or where root is not to be had:
# slates (FUSE) against tmpfs and the kernel's overlayfs over one base, in one run. From the repository root:
#
#   sh docs/wip/bench/fsbench/run.sh
#
# It expects a Linux release build of slates and the benchmark in the `slates-linux-target` volume (a debug binary
# measures the compiler, not slates: a quick profile reads 0.07 s release against 0.51 s debug). Build it with:
#
#   docker run --rm --memory 10g -v "$PWD":/src:ro -v slates-linux-target:/target -v slates-linux-cargo:/cargo \
#     -e CARGO_TARGET_DIR=/target -e CARGO_HOME=/cargo -e CARGO_BUILD_JOBS=4 -e AWS_LC_SYS_NO_JITTER_ENTROPY=1 \
#     -w /src rust:1.98 cargo build --release --locked -p slates-cli --bin slates --example mount_bench
#
# A container on a Mac runs in Docker Desktop's VM, whose round trips are not bare metal's: record which it was. Until
# 2026-11-01 this script ran inner.sh and fsync.py (creates, and create+fsync, at p50 and p99); the 2026-10-19 records
# in BENCHMARKS.md came from them, which commit 957cef9b holds (`git show 957cef9b:docs/wip/bench/fsbench/inner.sh`).
HERE=$(cd "$(dirname "$0")" && pwd); ROOT=$(cd "$HERE/../../../.." && pwd)
docker run --rm --privileged --device /dev/fuse \
  -v slates-linux-target:/target:ro -v "$ROOT":/src:ro -e FILES -e ROUNDS \
  rust:1.98 bash -c 'apt-get update -qq > /dev/null && apt-get install -y -qq fuse3 > /dev/null &&
    SLATES=/target/release/slates BENCH=/target/release/examples/mount_bench RUNNER_TEMP=/mnt \
    bash /src/docs/wip/bench/fsbench/linux.sh'
