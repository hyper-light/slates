# The KIND lane's network-shaping init image (deploy/kind/values-netem.yaml): alpine with iproute2's `tc`,
# nothing else. Runs as root with CAP_NET_ADMIN for one `tc qdisc add` in the pod's network namespace and
# exits; the slates container that follows runs non-root with every capability dropped. Lane only.
FROM alpine:3.20
RUN apk add --no-cache iproute2
