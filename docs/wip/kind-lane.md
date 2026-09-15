# The KIND fleet lane — the Helm chart and the fleet on real Linux pods

> **Status (2026-09-14).** The fleet **forms and serves on real multi-node Linux pods**, installed by the
> Helm chart. Formation (every pod probes both peers, one council leader elected with measured timing),
> content placement at `f + 1` across pods, and the SIGKILL takeover (the owner's pod deleted, a survivor
> serves the volume) are all proven here for the first time — the §4.8 WAN status of 2026-09-14 owed
> exactly "the KIND lane over a real path". Five real defects were found on the way: four fixed (three
> Linux-only blockers plus the fleet-formation off-by-one below) and a fifth root-caused with its failing
> test written — a fleet node's own tasks are outside its task budget, which left one pod of five unable to
> admit any client — whose fix is pending a free box (defect 5 below). Because of it the **five-replica
> scale step was cut on 2026-09-14 at 16:02 CDT and the netem profiles never ran**: Pieces 4 and 5 carry
> no numbers yet. One gap is left, precisely characterized: a **whole-pod restart** does not rejoin the
> mesh (the RAM-only same-seed-id restart — a design question for Kubernetes, §"The one gap left").

Hardware: Apple silicon (arm64) laptop; Docker Desktop's Linux VM (`docker version` → `linux/arm64
29.3.1`, 18 CPUs, 67 303 636 992 bytes). Tools: kind v0.33.0, kubectl 1.34, helm 4.3.0. Lane scratch is
under `$HOME/.cache/slates-kind-lane/<pid>` (outside the tree), removed at the end unless `--keep`. The
cluster is `slates-lane` (created and deleted by the lane; the box's `desktop`/`focal` clusters untouched).

## Run it

```
cargo xtask kind all                 # image → up → install → prove → scale → netem → delete cluster
cargo xtask kind image | smoke | up | install | prove | scale | netem | down | certs --out FILE
```

Deliverables: `Dockerfile` (+ `.dockerignore`); `deploy/helm/slates/` (chart) and its gate
`xtask/tests/helm_chart.rs`; `deploy/kind/{cluster,values-lane,values-netem}.yaml` and `netem.Dockerfile`;
`xtask/src/kind.rs` (the lane); `.github/workflows/ci.yml` `kind:` job; `docs/deploy.md` (operator guide).

## The five defects the lane found (four fixed, one root-caused)

The lane exercised the daemon on real Linux pods under a container's default seccomp profile, across
network namespaces, under an anchor, under a 1 GiB memory bound — none of which any prior test did — and
found five real bugs:

1. **The anchored first boot ran one generation ahead of its manifest seed** — the fleet could not form
   on *any* real deployment. A node's member id is `member_id(anchor, incarnation)`; the manifest seeds
   `member_id(anchor, 0)`, but the daemon used the anchor's `SUP_GENERATION` (a *start* count, 1 on a
   first boot) directly, so it ran as `member_id(anchor, 1)`. Every peer probed the seed and was answered
   under the gen-1 id, so no probe was credited and the fleet never converged (`peers_probed 0/1/1`).
   Fixed: the incarnation is the *restart* count (`SUP_GENERATION − 1`, 0 on a first boot). The in-process,
   sim and three-process-loopback tests all run daemons with no anchor (generation 0), so they never hit
   it. `docs/bugs/2026-09-14-anchored-first-boot-generation-off-by-one.md`.
2. **The epoll driver re-added an already-registered socket** (`EEXIST`), so every receive loop died after
   its first datagram in any container under the default seccomp profile (io_uring refused) — the fleet's
   serve sockets never survived one packet. Fixed to re-arm one-shot interest with `EPOLL_CTL_MOD`.
   `docs/bugs/2026-09-14-epoll-readiness-re-add-eexist.md`.
3. **The core matrix left the anchor's thread pinned to one core**, so its spawned daemon counted one core,
   hashed a different machine identity and refused the anchor's segment (a crash loop): every full-profile
   anchor on Linux. `docs/bugs/2026-09-14-core-matrix-leaves-the-anchor-pinned-to-one-core.md`.
4. **A Linux segment-handoff descriptor was owned by every attach**, so the daemon's second attach closed
   the first's (`mmap EBADF`, or the SIGABRT the Linux CI anchor test showed — the open "anchor fd
   double-close" item). `docs/bugs/2026-09-14-segment-handoff-descriptor-owned-twice.md`.
5. **A fleet node's own tasks are outside its task budget, and a refused client admission poisons the
   node** (root-caused, fix pending). The task arena is sized `clients_per_shard × 2 + 5` — 25 at the
   pod's 1 GiB — but a fleet node spawns `5 + 6 × peers` more (its receive, accept and coordinator loops,
   two dial tasks per peer, up to two serve tasks per plane per peer): 29 at four peers. When the arena is
   full, the task that seats a client on its shard is dropped unrun by the runtime (counted, unlogged);
   its control socket closes, the client reconnects under its own id, the control shard still holds that
   id, so the client is refused `SessionTaken` — and each probe burns two of the ten client ids until
   `TooManyClients`, which the accept loop swallows and the client reads as "short handoff message",
   forever. One pod of five never became Ready (2026-09-14 15:57–16:02 CDT), its daemon alive and meshed;
   the 3-replica 14-minute stall of 14:53 has the same signature. Failing test written (ignored until the
   fix); `docs/bugs/2026-09-14-fleet-tasks-outside-the-task-budget-poison-client-admission.md` has the
   evidence chain and the exact edits.

And one feature the Kubernetes deployment needs, built and proven: **dialing a peer by its DNS name**,
resolved at every dial through the daemon's own async A-record client over the runtime's UDP (no
`std::net`, no blocking `getaddrinfo`), so a rescheduled pod is reached at its new IP; `slates status`
carries the council's and root group's leader and derived election timing (`fleet_council_*`).

## Piece 1 — the image (PROVEN)

`Dockerfile`: `cargo build --release --locked -p slates-cli` in `rust:1.98` (release profile — `panic =
"abort"`, LTO — with BuildKit cache mounts), copied onto `gcr.io/distroless/cc-debian13:nonroot` (the
builder's own Debian release; `cc-debian12` refuses the binary at exec, `GLIBC_2.39 not found`, measured
2026-09-14). The anchor is PID 1. Measured 2026-09-14: image **14 179 867 bytes**, arm64, non-root; the
release build stage **~25 s** warm; `smoke` (one node in Docker, `--memory 1g --cpus 2`) answered `status`
**0.34 s** after `docker run` (shards 1, f 0, the solo council at the ten-period floor,
`effective_capacity_bytes = 1073741824` — the cgroup bound), and a volume was created and listed.

## Piece 2 — the chart (PROVEN)

`deploy/helm/slates/` renders Ada's decided shape (docs/deploy.md): a StatefulSet
`podManagementPolicy: Parallel` with **no `volumeClaimTemplates`** (R1); a headless Service publishing
not-ready addresses (per-pod DNS `slates-N.slates.<ns>.svc.<clusterDomain>`); the fleet manifest as a
ConfigMap rendered from `.Values.replicas` (per-pod addresses, a UDP block per node at `basePort + 2N`,
`f = ⌊(replicas−1)/2⌋`, every public certificate) with a checksum annotation restarting pods on a scale;
one Secret per pod with its key (mounted by `subPathExpr` at one fixed path); Guaranteed memory/CPU QoS;
required host anti-affinity + zone spread; readiness from `slates status`; non-root, all capabilities
dropped, read-only root filesystem. The chart never generates a certificate; a missing identity is refused
by name at render time.

Gates (`cargo test -p xtask --test helm_chart`, helm 4.3.0): `helm lint` clean; the render **equal to
`ci/golden.yaml` byte for byte** (249 lines, deterministic, with an `--ignored regenerate` writer); a
missing identity refused by name; `f = 2` at five replicas and `f = 0` at one — **4 passed, 1 ignored**.

## Piece 3 — the lane end to end (PROVEN: formation, placement, takeover)

Measured 2026-09-14 (`cargo xtask kind all` on the clean image; the six-node cluster is 1 control-plane +
5 workers, since a node is a failure domain):

- **Cluster** up in ~26 s, images loaded ~5 s. **Install** at 3 replicas rolled out in ~6 s (readiness =
  `slates status`); every pod scheduled on a distinct worker; every per-pod DNS name resolves to the pod's
  IP (checked with `nslookup` and with the daemon's own query bytes to CoreDNS `10.96.0.10:53`).
- **Formation in 0.2 s**: every pod holds the same three members (its **host id is its manifest gen-0
  seed** — the fix), `fleet_peers_probed 2`, exactly one `fleet_council_leads true`, `fleet_council_samples
  6452 / 12374 / 6463` (measured, not the floor), `fleet_council_rtt_tail_ns ≈ 3.6–6.0 ms` (the real
  cross-node pod path), no refusals.
- **Placement**: a volume sealed on one pod placed at `f + 1` across pods in **1.1 s**; before the delete a
  survivor holds the head but serves no such volume (`volume stat → NotFound`).
- **SIGKILL takeover**: the owner's pod deleted with `--grace-period=0 --force`; both survivors **retired
  the dead owner in 6.9 s**, and the first-ranked survivor **serves the volume** (`volume stat → name:
  lane, placed region: true`) — the takeover the design describes, on real pods.
- **Second run, 2026-09-14 15:55 CDT** (`cargo xtask kind all --keep`, log
  `~/.cache/slates-kind-lane/all-run.log`; the image rebuilt at 3d2aea7's tree in 27.2 s, 14 179 867
  bytes): cluster up in 26.9 s, images loaded in 3.4 s; install at 3 replicas rolled out in **8.5 s**;
  **formation in 0.2 s** (every pod `peers_probed 2`, one leader, `rtt_tail_ns` 8.0 / 9.2 / 10.0 ms,
  samples 134 / 247 / 126, no refusals); volume placed at `f + 1` in **1.1 s**; the survivors retired the
  deleted owner **7.0 s** after `kubectl delete pod --grace-period=0 --force`, and `slates-1` served the
  volume region-placed **7.1 s** after; the replacement `slates-0` did not rejoin within 120 s (the gap
  below, reproduced: `members=[self]`, `peers_probed 0`, the survivors meshed at `peers_probed 1`).

## Piece 4 — the WAN timing measurements (built; run under `netem`)

`cargo xtask kind netem` installs a lane-only `CAP_NET_ADMIN` init container (**never** in the chart's
defaults — `values-netem.yaml`) that runs `tc qdisc add dev eth0 root netem …` on pods 0 and 1 before the
daemon starts, forms the fleet under each profile, then watches over a three-minute window: the leader
must not change and every pod's reported `fleet_council_base_periods` must equal
`⌈ELECTION_MARGIN × max(tail, heartbeat) / heartbeat⌉` on its measured `rtt_tail_ns`. Profiles: `wan`
(80 ms ± 20 ms — the Japan East → East US one-way profile the fabric proof used, so pod-0 ↔ pod-1 ≈ 160 ms
round trip), `wan-loss` (the same with 1 % loss), and `ceiling` (350 ms one way — above ~330 ms, the
handshake retransmit ceilings the §4.8 WAN status flagged; formation is reported, not required).

**Measured 2026-09-14 18:24–18:47 CDT** (`cargo xtask kind all --keep` on main `0772228`, log
`~/.claude/jobs/9fdd24ce/tmp/kind-all.log`, scratch `~/.cache/slates-kind-lane/23409`; the image built in
26.6 s, 14 200 004 bytes; cluster up in 25.4 s; the whole lane 19 min 55 s wall). Each profile is a fresh
3-replica install, a formation, then the three-minute watch (18 samples over 183 s):

| profile | rolled out | formed | `base` periods (pods 0 / 1 / 2) | `rtt_tail` ms | spread ms | leader changes |
|---|---|---|---|---|---|---|
| `wan` (80 ms ± 20 ms, 0 % loss) | 9.6 s | 0.2 s | 20 / 20 / 13 | 195 / 200 / 124 | 47 / 46 / 39 | 0 |
| `wan-loss` (the same, 1 % loss) | 7.5 s | 0.2 s | 25 / 24 / 14 | 245 / 232 / 139 | 83 / 63 / 55 | 0 |
| `ceiling` (350 ms one way, no jitter) | 7.5 s | **4.9 s** | 79 / 77 / 36 | 785 / 763 / 357 | 83 / 61 / 4 | 0 |

Read: the two netem'd pods (0 and 1) measure the ~160 ms round trip and derive a 20-period election base
where the un-netem'd pod (2, whose paths to them are one-way delayed) derives 13; 1 % loss lifts the tails
by ~40 ms and the base to 24–25; under the 350 ms ceiling the base reaches 77–79 periods on 760–785 ms
tails — and the fleet still forms (in 4.9 s, the handshake retransmit ceilings crossed) and holds its
leader for the whole window. No leader changed under any profile: the derived timing is stable on a real
delayed, jittered and lossy path, which is what the §4.8 WAN status owed. The election-timing status
paragraph of §4.8 carries these numbers.

## Piece 5 — scale (built; run under `scale`)

`cargo xtask kind scale` installs the chart **fresh** at 5 replicas (`f = 2`) and again at 3 (`f = 1`),
asserting the manifest ConfigMap re-renders with the new fault tolerance and the fleet forms clean at each
size. It uses a fresh install (uninstall + install) rather than an in-place `helm upgrade --set replicas=5`
on the running fleet **on purpose**: the chart *does* re-render the ConfigMap and carries a checksum
annotation that rolls the pods, but a StatefulSet RollingUpdate restarts pods one at a time, and a staggered
whole-pod restart hits the rejoin gap below (each reborn pod comes up on its gen-0 seed id and forms a solo
view). Booting every pod together forms cleanly, so that is how the two sizes are measured; a live in-place
scale onto a running fleet is owed with the rejoin fix.

**Cut on 2026-09-14 15:57** (`helm upgrade --install … --set replicas=5`: four pods Ready within seconds;
`slates-4` never Ready inside the 300 s bound, 0 restarts, its daemon alive and meshed at `peers_probed 4`
on its peers — defect 5, the fleet's tasks outside the task budget). **Measured 2026-09-14 18:24–18:47 CDT**
on main `0772228`, with the task share (`5de244d`) and the roster-sized flight fix (`d94d1cc`) in the
image: **5 replicas installed and formed in 7.5 s** — every pod Ready, `f = 2`, all five holding the same
five members, `peers_probed 4` on every pod, one leader, `rtt_tail_ns` 10 µs–1.0 ms (the un-netem'd pod
path); then the fresh 3-replica reinstall **installed and formed in 7.5 s**, formation 0.2 s. The
five-replica node that could not admit a client at 1 GiB admits it now.

## The one gap left — a whole-pod restart does not rejoin (open; not the cause first recorded)

When the owner's pod is deleted, the StatefulSet recreates it under the same name at a new IP. The
replacement boots at incarnation 0 with the same manifest seed member id its predecessor held (a pod
restart loses the RAM anchor segment, so there is no durable start count to advance, R1). It does **not**
rejoin within the 120 s window: it holds a solo view (`peers_probed=0`, `fleet_members` = itself only)
while the survivors hold each other (`peers_probed=1`). The takeover **stands** — the survivor serves the
volume.

**What the diagnostics show (2026-09-14, `prove` dumps the fleet logs and the Service endpoints on the
no-rejoin path).** All three pods are published Service endpoints (`publishNotReadyAddresses: true`), the
replacement's new IP among them. The replacement's `peers_probed=0` with **no fleet-error log lines**: it
forms *no probe session at all* to its peers — the failure is at session formation, **below** the
membership layer, not a membership refusal. One earlier run also showed a survivor's resolver time out on
the replacement's name (`fleet: resolving slates-0…: no answer within the timeout`), but a second run did
not reproduce that and still failed to rejoin, so the DNS timeout is a separate, transient flake, not the
cause.

**Correction to an earlier note in this file.** The failure was recorded here as the survivors refusing
the replacement's id because "learn-on-contact treats a same-generation id of a dead member as stale."
That is **wrong**: `server::fleet::classify_announced` returns `Current` for an equal generation, not
`Stale`, and an in-process probe (`crates/server/tests/fleet.rs`, a retired node's replacement at
generation 0 with the same identity) re-admits it once contact is made — the A-15 self-refutation
(`Membership::apply` refutes a death about the local id, bumping the SWIM incarnation past it) is correct.
So the incarnation scheme is not refusing the id; the mesh never gets far enough to exchange membership
because the probe sessions do not form.

**Owed: the session-formation diagnosis.** Why the replacement forms no probe session to peers that are up
and resolvable needs session-level instrumentation on a live cluster — the candidates are the survivors'
demultiplexer still holding the dead pod's session (keyed by its old source, on a plane the retirement path
may not close) so the replacement's dial from the new IP is refused or unmatched, and the probe loop's
re-dial of a returned peer at a possibly-new address. This is a fleet-transport diagnosis, not the
incarnation design tension first recorded; scope it with the session-lifecycle owner. The lane's `prove`
reports the rejoin as a best-effort observation, not a gate, so the lane stays green while it is open.

The other item the charter names — a **byte-level read-back through a kernel mount inside a pod** after the
takeover — needs `mount_nfs` in the image and the NFS-loopback mount inside a pod (the charter's own "design
question"); the lane proves the takeover at the record/serve level (`volume stat` on the successor), which
is the §4.8 takeover. The in-pod mount read-back is owed with that mount decision.

## The CI lane

`.github/workflows/ci.yml` `kind:` job (push cadence): `azure/setup-helm@v4` (v4.3.0) and
`helm/kind-action@v1` (v0.33.0) install the tooling — the versions validated here; non-Rust tooling
admitted for this lane by the charter, as the conformance lane's suites are — then `cargo test -p xtask
--test helm_chart` (the chart gates, also in `gates` on every PR) and `cargo xtask kind all` (the lane,
deleting its cluster at the end).

## Integrator notes (§4.8)

- **Registers/configuration (§4.8) row** for the ledger: *"The fleet deploys on Kubernetes through the
  Helm chart `deploy/helm/slates` — a Parallel StatefulSet with no PersistentVolume (RAM only, R1), a
  headless Service giving each node its per-pod DNS name (resolved by the daemon at every dial), the
  fleet manifest as a ConfigMap rendered from the replica count with f = ⌊(replicas−1)/2⌋, one certificate
  Secret per pod, Guaranteed memory QoS, node anti-affinity as the failure domain, and readiness from
  `slates status`. The KIND lane proves it on real multi-node pods: the fleet forms (every pod probes both
  peers, one council leader with measured timing), a volume places at f + 1 across pods, and the owner's
  pod deleted (SIGKILL) is retired by the survivors while the first-ranked successor serves the volume.
  Measured on main (2026-09-14 18:24–18:47): five replicas install and form in 7.5 s (`f = 2`, every pod
  probing four peers), and under `tc netem` the derived election timing holds its leader for a
  three-minute window on every profile (80 ms ± 20 ms: base 20 periods on 195–200 ms tails; with 1 % loss:
  24–25 on 232–245 ms; 350 ms one way: 77–79 on 763–785 ms, the fleet still forming in 4.9 s), with zero
  leader changes. Owed: a whole-pod restart rejoining the mesh (the RAM-only same-seed-id restart, a design
  question for Kubernetes) and a byte-level read-back through a mount inside a pod."*
- **§4.8 status paragraph (2026-09-14):** *"The KIND lane runs the fleet on real Linux pods over a real
  network, installed by the Helm chart — the WAN status owed exactly this. It proves the image, the chart
  and its render, per-pod DNS resolution, cross-node UDP, mutual-TLS session establishment on both planes,
  fleet formation (peers probed, one council leader, election timing derived from the measured cross-node
  RTT tail, ≈ 3.6–6 ms), content placement at f + 1, and the SIGKILL takeover (the owner's pod deleted,
  the survivors retire it in ≈ 7 s, the successor serves the volume). It found five real defects the
  loopback and simulation harnesses never reached — four fixed: the anchored-first-boot generation
  off-by-one that blocked all fleet formation, the epoll one-shot re-add, the core-matrix affinity leak,
  and the segment-handoff double-close; one root-caused with its failing test written and its fix pending:
  a fleet node's own tasks are outside its task budget (`clients_per_shard × 2 + 5`, 25 at 1 GiB, against
  `5 + 6 × peers` fleet tasks), and a client admission the runtime refuses is dropped unrun, closing the
  client's channel and leaking its id until the node refuses every client — one pod of five never Ready —
  and built DNS-name dialing. The election-timing measurements the WAN status owes are built under `tc
  netem` (80 ms ± 20 ms, with 1 % loss, and the 350 ms handshake ceiling) — measured on main on 2026-09-14
  once the task share landed: five replicas form in 7.5 s, and every profile holds its leader for 183 s
  with the base derived from the measured tails (20 / 24–25 / 77–79 periods) and zero leader changes.
  Owed: a whole-pod restart rejoining (the RAM-only same-seed-id restart is a Kubernetes design
  question — the seed is precomputable only at incarnation 0, while a restart wants an advancing
  incarnation, and a pod restart loses the anchor segment that would carry it); and a mount-read-back
  inside a pod."*
