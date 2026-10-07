#!/bin/sh
# The generated escape battery over Linux's NFSv4.2 client (privileged container), from the repository root:
#   SEEDS="1 2 3 4 5" STEPS=300 sh docs/wip/bench/escape/run-nfs.sh
HERE=$(cd "$(dirname "$0")" && pwd); ROOT=$(cd "$HERE/../../../.." && pwd)
docker run --rm --privileged \
  -v "$ROOT":/src:ro -v slates-cargo-reg:/usr/local/cargo/registry -v slates-linux-target:/target \
  -v "$HERE/inner-nfs.sh":/inner.sh:ro -v "$HERE/battery.py":/battery.py:ro \
  -e SEEDS -e STEPS rust:1.98.0 bash /inner.sh
