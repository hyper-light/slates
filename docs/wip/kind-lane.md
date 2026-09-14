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

**Not run yet.** On 2026-09-14 the `all` sequence stopped at the scale step (Piece 5), which precedes
`netem`; the profiles are built, lint-clean and driven by the lane, but carry no measurement. Owed with the
first lane run after the defect-5 fix.

## Piece 5 — scale (built; run under `scale`)

`cargo xtask kind scale` installs the chart **fresh** at 5 replicas (`f = 2`) and again at 3 (`f = 1`),
asserting the manifest ConfigMap re-renders with the new fault tolerance and the fleet forms clean at each
size. It uses a fresh install (uninstall + install) rather than an in-place `helm upgrade --set replicas=5`
on the running fleet **on purpose**: the chart *does* re-render the ConfigMap and carries a checksum
annotation that rolls the pods, but a StatefulSet RollingUpdate restarts pods one at a time, and a staggered
whole-pod restart hits the rejoin gap below (each reborn pod comes up on its gen-0 seed id and forms a solo
view). Booting every pod together forms cleanly, so that is how the two sizes are measured; a live in-place
scale onto a running fleet is owed with the rejoin fix.

**Cut on 2026-09-14.** `helm upgrade --install … --set replicas=5` at 15:57:36 CDT: four pods Ready within
seconds; `slates-4` never Ready inside the 300 s bound (16:02 CDT, 0 restarts, its daemon alive and meshed
at `peers_probed 4` on its peers) — defect 5. The lane's not-Ready diagnostics (03ff718) captured the
cause; the 3-replica reinstall and the netem profiles were not reached. No scale numbers yet; owed with
the fix.

## The one gap left — a whole-pod restart does not rejoin (a Kubernetes design question)

When the owner's pod is deleted, the StatefulSet recreates it under the same name at a new IP. Because a
whole-pod restart loses the **RAM anchor segment** (the anchor is PID 1, killed with the pod), the
replacement boots at **incarnation 0** with the **same manifest seed member id** its retired predecessor
held — RAM-only leaves no durable start count to advance across a pod restart (R1). The survivors, which
retired that id, do not re-admit it: SWIM's two-id learn-on-contact (task #22) treats a same-generation
id of a dead member as stale (a restart is expected to announce a *higher* generation = a new id), so the
replacement's probes are not acknowledged and it forms a solo view (`fleet_members` = itself only). The
takeover **stands** — the survivor serves the volume — but the replacement pod is isolated.

This is the "restart = join" path for Kubernetes, and it is a genuine design tension, not a code slip: the
design gets "a fresh fleet forms with no exchange" from a **precomputable seed** (incarnation 0), and
"a restart is a new id" from an **advancing incarnation** — reconcilable when the anchor segment persists
the generation across *daemon* restarts (a bare-metal deployment), but not across a *pod* restart, which
loses it. Options for the fix owner, none free: derive the incarnation from a source that survives a pod
restart (wall-clock boot time — but then the manifest cannot precompute the seed, so formation needs a
first-contact exchange); or let a dead member be re-admitted by SWIM refutation at the same id (the
replacement bumps its *SWIM incarnation* past the death it hears — A-15 — without a member-id change,
which needs the learn-on-contact stale check to stop refusing a same-generation probe from a dead member).
It needs Ada's call on which, as the charter flagged the in-pod mount as a design question. Recorded here;
the lane's `prove` reports the rejoin as a best-effort observation, not a gate.

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
  Owed: the task-budget fix (a fleet node's tasks outside `tasks_per_shard` left one pod of five unable
  to admit a client — root-caused, failing test written), with it the five-replica scale and the netem
  timing numbers; a whole-pod restart rejoining the mesh (the RAM-only same-seed-id restart, a design
  question for Kubernetes); and a byte-level read-back through a mount inside a pod."*
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
  netem` (80 ms ± 20 ms, with 1 % loss, and the 350 ms handshake ceiling) and not yet measured: the
  2026-09-14 run was cut at the five-replica scale step by that defect. Owed: that fix with the scale and
  netem numbers; a whole-pod restart rejoining (the RAM-only same-seed-id restart is a Kubernetes design
  question — the seed is precomputable only at incarnation 0, while a restart wants an advancing
  incarnation, and a pod restart loses the anchor segment that would carry it); and a mount-read-back
  inside a pod."*
