#!/bin/bash
# The hot-directory storm through a slates NFSv4.2 volume in Docker, against the container's own overlay and a host
# bind as references. Arguments: workers, files per worker.
set -u
ROOT=/Users/adalundhe/Projects/slates
SLATES=${SLATES_BIN:-$ROOT/target/release/slates}
S=/private/tmp/claude-501/-Users-adalundhe-Projects-slates/5bcbc28d-64b0-4c59-8918-79563c747af3/scratchpad
W=${1:-16}; N=${2:-500}
I=slates-hot-$$
VOL=slates-hot-$$
"$SLATES" --instance "$I" anchor --quick --shards 4 >/dev/null 2>$S/hot-anchor.err &
A=$!
cleanup() { docker volume rm -f "$VOL" >/dev/null 2>&1; kill "$A" 2>/dev/null; wait "$A" 2>/dev/null; }
trap cleanup EXIT
for _ in $(seq 1 200); do "$SLATES" --instance "$I" volume list >/dev/null 2>&1 && break; sleep 0.1; done
sleep 1; "$SLATES" --instance "$I" bootstrap root >/dev/null 2>&1
ID=$("$SLATES" --instance "$I" volume create hot --bounded 2GiB 2>&1 | awk '/^id/{print $2}')
"$SLATES" --instance "$I" export "$ID" > $S/hot-export.txt 2>&1
PORT=$(awk '/^port:/{print $2}' $S/hot-export.txt); EXPORT=$(awk '/^export:/{print $2}' $S/hot-export.txt)
docker volume create --driver local --opt type=nfs --opt "o=addr=host.docker.internal,vers=4.2,proto=tcp,port=$PORT,hard,timeo=600" --opt "device=:$EXPORT" "$VOL" >/dev/null
echo "== slates NFSv4.2"
docker run --rm -v "$VOL":/work -v $S/hotdir:/bench:ro python:3.12-slim sh -c 'python /bench/hotdir.py /work "$0" "$1"; awk "/^device .* mounted on \/work/{f=1} f&&/per-op statistics/{p=1;next} p&&NF==0{exit} p{print}" /proc/self/mountstats | awk "\$2>0{printf \"%s %d rtt %.3f\\n\", \$1, \$2, \$8/\$2}"' "$W" "$N"
echo "== container overlay"
docker run --rm -v $S/hotdir:/bench:ro python:3.12-slim python /bench/hotdir.py /tmp/x "$W" "$N"
"$SLATES" --instance "$I" status 2>/dev/null | grep -E "nfs.local_p99|nfs.local_p50|delegation.granted|nfs4.delay" | head -8
