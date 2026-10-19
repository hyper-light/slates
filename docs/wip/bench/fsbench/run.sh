#!/bin/sh
# Condition 12, like-for-like: slates (FUSE) against the kernel's overlayfs and a tmpfs, same workload, same
# base, same host, one run. From the repository root:
#
#   sh docs/wip/bench/fsbench/run.sh
#
# It expects a Linux **release** build in the `slates-linux-target` volume (a debug binary measures the
# compiler, not slates — a quick profile reads 0.07 s release against 0.51 s debug). Build it with:
#
#   docker run --rm --memory 10g -v "$PWD":/src:ro -v slates-linux-target:/target -v slates-linux-cargo:/cargo \
#     -e CARGO_TARGET_DIR=/target -e CARGO_HOME=/cargo -e CARGO_BUILD_JOBS=4 -e AWS_LC_SYS_NO_JITTER_ENTROPY=1 \
#     -w /src rust:1.98 cargo build --release --locked -p slates-cli --bin slates
#
# Record the load with the numbers: all three arms run in the same contended minute, so the ratios hold even on
# a busy host, but the absolutes do not travel. CREATES, FSYNCS and BASE_FILES override the defaults.
HERE=$(cd "$(dirname "$0")" && pwd); ROOT=$(cd "$HERE/../../../.." && pwd)
OUT=${OUT:-$(mktemp -d)}
echo "out: $OUT"
docker run --rm --privileged --device /dev/fuse \
  -v slates-linux-target:/target:ro \
  -v "$ROOT/docs/wip/bench/fuse":/c:ro \
  -v "$HERE":/f:ro \
  -v "$OUT":/out \
  -e CREATES -e FSYNCS -e BASE_FILES \
  rust:1.98 bash /f/inner.sh
