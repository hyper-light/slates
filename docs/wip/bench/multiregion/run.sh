#!/bin/sh
# Two regions on two separate Docker networks, joined only through a router container whose egress on both
# sides is shaped by `tc netem` (condition 10, docs/wip/CONDITIONS.md). Six real daemon processes, one per
# container, from ONE manifest: a0..a2 in region 0 on network mr-a, b0..b2 in region 1 on network mr-b, each
# region the other's mirror. Daemon containers drop every capability and run non-root (the image's user); a
# separate holder container owns each node's network namespace and is the only one given NET_ADMIN, for the
# one route to the other region through the router. Nothing here touches the repository's tree; identities
# and the manifest go to the scratch directory given as the first argument.
#
#   sh docs/wip/bench/multiregion/run.sh SCRATCH_DIR [up|down]
#   DELAY=100ms JITTER=40ms LOSS=3% IMAGE=slates:mr sh docs/wip/bench/multiregion/run.sh SCRATCH_DIR up
#
# The shaping applies to every packet crossing the router in either direction, so a cross-region round trip
# carries 2 x DELAY +- 2 x JITTER and each direction LOSS; traffic inside a region is unshaped.
set -eu
DIR=${1:?scratch directory}; ACTION=${2:-up}
IMAGE=${IMAGE:-slates:mr}; NETEM=${NETEM_IMAGE:-slates-netem:lane}
DELAY=${DELAY:-100ms}; JITTER=${JITTER:-40ms}; LOSS=${LOSS:-3%}
MEMORY=${MEMORY:-1g}; CPUS=${CPUS:-2}
NAME=slates-mr; PORT=7000
NODES="a0 a1 a2 b0 b1 b2"

ip_of() { case $1 in a*) echo "10.77.1.1${1#a}" ;; b*) echo "10.77.2.1${1#b}" ;; esac; }
net_of() { case $1 in a*) echo mr-a ;; b*) echo mr-b ;; esac; }
region_of() { case $1 in a*) echo 0 ;; b*) echo 1 ;; esac; }
router_of() { case $1 in a*) echo 10.77.1.254 ;; b*) echo 10.77.2.254 ;; esac; }
other_subnet() { case $1 in a*) echo 10.77.2.0/24 ;; b*) echo 10.77.1.0/24 ;; esac; }

down() {
  for node in $NODES; do docker rm -f "mr-$node" "mr-$node-net" >/dev/null 2>&1 || true; done
  docker rm -f mr-router >/dev/null 2>&1 || true
  docker network rm mr-a mr-b >/dev/null 2>&1 || true
}

if [ "$ACTION" = down ]; then down; exit 0; fi
down
mkdir -p "$DIR"

# Identities: a self-signed P-256 certificate per node carrying the fleet's TLS name, key as PKCS#8 DER (the
# shape rcgen writes for the in-tree lanes). The curve must be named: LibreSSL encodes explicit parameters by
# default, and rustls then refuses the pair (`KeyMismatch`, measured 2026-10-07).
for node in $NODES; do
  openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-256 -pkeyopt ec_param_enc:named_curve -nodes -days 7 \
    -subj "/CN=$NAME" -addext "subjectAltName=DNS:$NAME" \
    -keyout "$DIR/$node.key.pem" -out "$DIR/$node.crt.pem" 2>/dev/null
  openssl x509 -in "$DIR/$node.crt.pem" -outform DER -out "$DIR/$node.crt.der"
  openssl pkcs8 -topk8 -nocrypt -in "$DIR/$node.key.pem" -outform DER -out "$DIR/$node.key.der"
  rm -f "$DIR/$node.key.pem" "$DIR/$node.crt.pem"
done
chmod 0644 "$DIR"/*.der
{
  printf '{ "name": "%s", "f": 1, "mirrors": { "0": 1, "1": 0 }, "nodes": [' "$NAME"
  sep=""
  for node in $NODES; do
    printf '%s\n  { "node": "%s", "address": "%s:%s", "region": %s, "certificate": "%s.crt.der", "key": "%s.key.der" }' \
      "$sep" "$node" "$(ip_of "$node")" "$PORT" "$(region_of "$node")" "$node" "$node"
    sep=","
  done
  printf '\n] }\n'
} > "$DIR/fleet.json"

docker network create --subnet 10.77.1.0/24 mr-a >/dev/null
docker network create --subnet 10.77.2.0/24 mr-b >/dev/null
docker run -d --name mr-router --network mr-a --ip 10.77.1.254 --cap-add NET_ADMIN \
  --sysctl net.ipv4.ip_forward=1 "$NETEM" sleep 1000000 >/dev/null
docker network connect --ip 10.77.2.254 mr-b mr-router
for dev in eth0 eth1; do
  docker exec mr-router tc qdisc add dev "$dev" root netem delay "$DELAY" "$JITTER" loss "$LOSS"
done
echo "router: netem delay $DELAY $JITTER loss $LOSS on both sides"

for node in $NODES; do
  docker run -d --name "mr-$node-net" --network "$(net_of "$node")" --ip "$(ip_of "$node")" \
    --cap-add NET_ADMIN "$NETEM" sleep 1000000 >/dev/null
  docker exec "mr-$node-net" ip route add "$(other_subnet "$node")" via "$(router_of "$node")"
  docker run -d --name "mr-$node" --network "container:mr-$node-net" --memory "$MEMORY" --cpus "$CPUS" \
    --cap-drop ALL --read-only -v "$DIR":/etc/slates:ro "$IMAGE" \
    anchor --fleet /etc/slates/fleet.json --node "$node" --quick >/dev/null
  echo "node $node: $(ip_of "$node") region $(region_of "$node")"
done
