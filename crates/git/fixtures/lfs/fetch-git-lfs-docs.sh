#!/usr/bin/env bash
# Fetches the git-lfs documents slates-git's LFS codecs are held to (A-610), verbatim, at the pinned release, and checks
# each against the SHA-256 recorded below: git-lfs v3.8.1 (tag object 36e29152, commit a662a6dbf, released 2026-10-22).
# The tests read their JSON examples and the pointer examples from these files, compiled in. Usage:
# fetch-git-lfs-docs.sh OUTDIR. Fetched 2026-11-19 with curl from raw.githubusercontent.com.
set -euo pipefail
OUT=$(cd "$1" && pwd)
COMMIT=a662a6dbfedf706cfa9fc7a5b7bcc7ba2e2c8a65
BASE="https://raw.githubusercontent.com/git-lfs/git-lfs/$COMMIT"
while read -r sum source name; do
  curl -fsS -o "$OUT/$name" "$BASE/$source"
  echo "$sum  $OUT/$name" | shasum -a 256 -c -
done <<'FILES'
eafbead6a8752ea778f9d25451f7676c00885e3f643e773ce8b910bc97f2d96a docs/api/batch.md batch.md
72c293217d844f25d1be1d7ff3daa0e61c4ee042ebabff1bd84541527fdd26c9 docs/api/basic-transfers.md basic-transfers.md
6582abda1c171aae9f0516f348fe51c4b7b8314635c6c7c959d2de5e7a283cb7 docs/api/locking.md locking.md
14ea5fc720fbfe037d4a20991bdf272c3688d84b4cd49ae981805ec5c5e23fcd docs/spec.md spec.md
FILES
