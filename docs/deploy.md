# Deploying a slates fleet on Kubernetes

The chart at `deploy/helm/slates` installs a fleet of slates nodes: a StatefulSet of RAM-only daemons
that replicate each other's volume heads and content over UDP and take over for a dead member (the
[unified design](wip/SLATES_DESIGN.md) §4.8; the multi-process form is in [cli.md](cli.md) "Deploying a
fleet"). What it makes, and deliberately does not make:

- **A StatefulSet, `podManagementPolicy: Parallel`, no `volumeClaimTemplates`.** slates has no on-disk
  database (R1: disk is written only inside a granted landing). A node's registers live in RAM,
  replicated to `2f + 1` holders; a node that loses its RAM comes back, rejoins, and is re-filled. The
  fleet forms as its pods come up in parallel — a peer not yet listening is dialed again each period.
- **A headless Service** (`clusterIP: None`, not-ready addresses published) giving every pod its DNS
  name `<release>-N.<release>.<namespace>.svc.<clusterDomain>`, which is the manifest's address for node
  N. The daemon resolves it at every dial, so a rescheduled pod is found at its new IP. Nothing sits in
  front of the fleet's UDP planes: the session plane pins each peer's source address.
- **The fleet manifest as a ConfigMap**, rendered from `replicas`: node names, per-pod addresses, one
  UDP port block per node (`fleet.basePort + 2N` and the next), `f = ⌊(replicas − 1) / 2⌋`, and every
  node's public certificate. It changes only when `replicas` does — a membership change of the
  configuration group — and the pods restart on it.
- **One Secret per pod** with that pod's private key. Each container mounts only its own key
  (`subPathExpr` on the pod name from a projected volume over all the per-pod Secrets).
- **Guaranteed QoS**: memory and CPU requests equal limits, so the daemon's effective capacity (§4.2)
  clamps to exactly the cgroup bound. Nodes must run without swap (the Kubernetes default,
  `failSwapOn: true`).
- **A node is a failure domain**: required pod anti-affinity on `kubernetes.io/hostname`; preferred
  spread across `topology.kubernetes.io/zone` where the cluster labels zones.
- **Readiness from the daemon itself**: `slates status` (exit 0 only when the daemon serves), never a
  port check. No liveness probe: the anchor (PID 1) already restarts a daemon whose heartbeat lapses.
- **No privilege** (R10): non-root (uid 65532), every capability dropped, a read-only root filesystem,
  the runtime seccomp profile. The chart's only privileged container is the network-shaping init
  container of the KIND lane (`netem`), off by default and never a production value.

## Prerequisites

- A cluster (Kubernetes ≥ 1.29) with as many schedulable nodes as `replicas` (one member per node), and
  `helm` ≥ 3.12.
- The image: `docker build -t <registry>/slates:<tag> .` from the repository root (the `Dockerfile`
  builds the release binary onto a distroless base for the builder's architecture; the release lane
  builds `x86_64` and `aarch64` Linux). Push it, or for kind, `kind load docker-image`.
- One identity per pod: a DER certificate carrying the fleet's TLS name (`fleet.name`, default
  `slates-fleet`) as a subject alternative name, and its DER private key (PKCS#8, SEC1 or PKCS#1). The
  chart never generates one: an identity that changed on `helm upgrade` would change the node's member
  id (derived from its certificate) and every peer's pin. Provision them from your own authority, or, for
  a test cluster, mint self-signed ones:

  ```
  cargo xtask kind certs --replicas 3 --out identities.yaml
  ```

  writes a values file of the form

  ```yaml
  certificates:
    slates-0: { certificate: <base64 DER>, key: <base64 DER> }
    slates-1: { certificate: <base64 DER>, key: <base64 DER> }
    slates-2: { certificate: <base64 DER>, key: <base64 DER> }
  ```

  The pod names are `<release>-N`; with the release named `slates` they are `slates-N`. The roster's
  allow-list is exactly this set: every node pins every listed certificate and admits no other.

## Install

```
helm install slates deploy/helm/slates \
  --namespace slates --create-namespace \
  --set image.repository=<registry>/slates --set image.tag=<tag> \
  --set replicas=3 --set resources.memory=4Gi --set resources.cpu=4 \
  -f identities.yaml
kubectl -n slates rollout status statefulset/slates
kubectl -n slates exec slates-0 -- /slates status
```

`status` on any pod shows `fleet_members` (every member it holds alive — three ids once formed),
`fleet_peers_probed` (2: both peers' sessions formed), `fleet_council_leads` (exactly one pod reports
`true`) and the council's derived election timing (`fleet_council_base_periods`,
`fleet_council_rtt_tail_ns`, `fleet_council_samples`; see [cli.md](cli.md)). A refusal count
`fleet.resolve` on a shard means a peer's DNS name did not resolve at that dial — a pod not yet created,
or a name the cluster's DNS does not serve.

Values of note (`deploy/helm/slates/values.yaml` states each one's derivation or policy):

| Value | Meaning |
|---|---|
| `replicas` | The fleet's size; `f` is derived from it. Three survive one death, five survive two. |
| `resources.memory`, `resources.cpu` | Per-node bounds, requests equal to limits. The daemon derives its arenas and shard count from them. |
| `fleet.basePort` | The first node's UDP base port; node N uses `basePort + 2N` and the next. |
| `fleet.durability` | Optional: the operator's accepted coincident-loss probability under a stated failure count ([cli.md](cli.md)). |
| `clusterDomain` | The cluster's DNS suffix. |
| `certificates` | One identity per pod (above). |
| `extraArgs` | Extra `slates anchor` arguments; `--quick` skips the boot-time machine profile on a test cluster only. |

## Scale

```
helm upgrade slates deploy/helm/slates --reuse-values --set replicas=5 -f identities-5.yaml
```

The manifest ConfigMap is re-rendered with five nodes at `f = 2` and every pod restarts on it (the
StatefulSet's checksum annotation); the fleet re-forms as the pods come up. Scaling back to three retires
the two removed members the same way. Every scale is a configuration-group membership change (D-14's
cold path), never a per-write event.

## What a pod restart means

A pod that is deleted or rescheduled loses its RAM: its volumes' heads are taken over by the surviving
holders (the SIGKILL takeover of §4.8) and served from there; it comes back under its name, is admitted
by its peers on contact, and holds nothing until re-replication fills it. The KIND lane
([wip/kind-lane.md](wip/kind-lane.md)) records this flow on real pods with its numbers.

## Not in the chart

A PersistentVolume of any kind; a Service with ports or a load balancer in front of the fleet; an
Ingress; a liveness probe; a privileged container (outside the lane's `netem`); a generated certificate.
