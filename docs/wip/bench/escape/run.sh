#!/bin/sh
# The generated escape battery (condition 4) in a Linux container, from the repository root:
#   SEEDS="1 2 3 4 5" STEPS=300 sh docs/wip/bench/escape/run.sh
# The container builds slates-cli into the named cache volume, mounts a volume --shared as `tester`, runs
# battery.py per seed as `tester` (the agent) and `other` (a tool), and hashes the tree outside the mount
# before and after. Exits non-zero on any violated invariant.
HERE=$(cd "$(dirname "$0")" && pwd); ROOT=$(cd "$HERE/../../../.." && pwd)
docker run --rm --device /dev/fuse --cap-add SYS_ADMIN --security-opt seccomp=unconfined --security-opt apparmor=unconfined \
  -v "$ROOT":/src:ro -v slates-cargo-reg:/usr/local/cargo/registry -v slates-linux-target:/target \
  -v "$HERE/inner.sh":/inner.sh:ro -v "$HERE/battery.py":/battery.py:ro \
  -e SEEDS -e STEPS rust:1.98.0 bash /inner.sh
