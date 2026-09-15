# The `slates` command

This guide describes the implemented command grammar (the A-9 review was 2026-09-05;
`mount`/`unmount`, the merge verbs, `--json` on every verb, and `mcp` were
added 2026-09-09). The [unified design](wip/SLATES_DESIGN.md) §4.12 describes the target
interface; the [gap ledger](wip/GAPS.md) §8i tracks what is still missing.

The current CLI is a development surface. `--locked` records intent without establishing
locked backing, and bounded/dynamic claims do not yet provide the design's host-capacity
guarantee. Daemon restart recovers metadata but can lose volume bytes and local snapshots.
`attach` returns an attachment record without creating a mounted path. `exec` is Linux-only
and currently expects an externally supplied `SLATES_ROOT` naming an established root mount.
There is no implemented CLI/MCP virtio-fs or OCI attachment flow; the MCP server (`slates mcp`)
and the Python and Node SDKs are implemented (§4.12). On macOS a volume can be mounted for real
over the loopback NFS bridge (`slates mount`) with no privilege, kernel extension or Apple
entitlement. These limitations are open correctness/integration work, not optional setup steps.

## Running a daemon

```
slates anchor [--instance NAME] [--quick] [--shards N] [--fleet PATH --node NAME]
```

The anchor measures the machine profile (seconds; `--quick` for tests), creates the shared
segment in RAM, publishes the profile into it, and supervises `slates daemon` as a child with
the segment handed over in its environment. It restarts a daemon that exits, kills one whose
heartbeat lapses, and gives up on a crash loop (the bound is derived from the recovery budget
and the measured daemon start). `SIGINT` or `SIGTERM` stops both. This path is intended to avoid explicit disk writes;
an anonymous shared segment alone does not establish locked residency or no swapping.

`slates daemon` run alone measures a profile and serves without an anchor (development).

## Creating the first consensus group

After starting a new standalone daemon or the first node of a new fleet, run from another terminal:

```
slates --instance NAME bootstrap root
```

### Recovering a lost consensus quorum

Warm daemon restarts recover both groups from anchor RAM automatically. Whole-anchor loss
joins under a fresh member identity when each group still has a quorum.

If a group has lost its quorum, compare `slates recovery-plan root --json` (or `region`) on
the surviving nodes. The plan identifies the retained committed position, application version,
uncommitted tail and former voters. Choose the most complete retained copy of the same group.
Fence the entire former group and prevent its clients from continuing before recovery.
Unreachable copies may contain newer committed state; recovery cannot rule out that loss.

```sh
slates recovery-plan root --json
slates recover root --confirm PLAN --fenced --accept-loss --json
```

Before starting the anchor, operators on any platform can set `SLATES_RECOVERY_KEY` to a
read-only file containing exactly 32 random bytes (not all zero), provisioned separately for that node.
Set the same path in the recovery CLI's environment. A Kubernetes Secret mount, a VM secret
mount or an operator-owned local file uses the same reader. Never share this key across nodes;
it authorizes consensus recovery only. The daemon reads it at startup. An unreadable, zero or
wrong-sized configured key refuses; restart with the new key to rotate it.

With no configured recovery key, approval requires the anchor's human capability (the named
issuer surface on macOS/Windows; Linux operators should provision the recovery key). The
second command refuses if the reviewed state or recovery authority has changed. Its reply names the replacement `GROUP`. On each other
survivor, explicitly authorize joining that group:

```sh
slates recovery-plan root --join-group GROUP --json
slates recover root --join-group GROUP --confirm PLAN --fenced --accept-loss --json
```

A pending join stops voting and proposing in the old group but retains its state until the
replacement validates. It survives a warm restart and refuses a replacement older than the
node's known application version. Discovery cannot authorize recovery. Use `region` in the
same commands when recovering a regional council.

Run it once on one node of the first region. For each additional region, wait for the root to admit
that region, then run `slates bootstrap region` once on one of its nodes. The authenticated local
account may issue bootstrap; an enrolled consumer or forwarded fleet request cannot. The CLI binds
the request to the current daemon member id, so automatic retries cannot bootstrap a replacement.

Startup alone serves discovery and status but refuses writes until initialized and admitted.
Every daemon restart currently loses its Raft state and uses a fresh member id. It joins through a
surviving voting quorum. A standalone restart loses the sole quorum, requiring explicit new-group
creation; that does not restore a lost consensus history. See [AUD-07](bugs/2026-09-14-raft-voter-state-loss.md).
Never add bootstrap to a pod's recurring startup command or readiness probe.

## Deploying a fleet

The discovery and admission protocol is the same for local processes, bare-metal hosts, VMs,
and Kubernetes pods (R8). Use loopback addresses with distinct port pairs for a local fleet,
or routable IPs/DNS names for separate hosts. There is no Kubernetes API call in the protocol.

Configured peers discover each other's current addresses and boot identities automatically.
DNS names are resolved again on reconnect; authenticated replacements fetch the existing
group's state and join through Raft. Only creating the first group requires `bootstrap`.
Discovery currently uses the manifest's pinned identities. Finding and enrolling previously
unlisted nodes requires a discovery provider and a trust-enrollment protocol; those are not
implemented. A discovery response alone cannot grant voting authority or create a new group.

A fleet is several daemons on several machines that replicate each other's volume heads and
content and take over for a dead member. Every node is started from **one shared manifest** with
its own `--node`:

```
slates anchor --fleet /etc/slates/fleet.json --node a
```

```json
{
  "name": "slates-fleet",
  "f": 1,
  "nodes": [
    { "node": "a", "address": "10.0.0.1:7000", "certificate": "a.crt.der", "key": "a.key.der" },
    { "node": "b", "address": "10.0.0.2:7000", "certificate": "b.crt.der", "key": "b.key.der" },
    { "node": "c", "address": "10.0.0.3:7000", "certificate": "c.crt.der", "key": "c.key.der" }
  ]
}
```

- `name` is the TLS name every node's certificate carries (its subject alternative name); peers
  verify each other's sessions under it. Certificates and keys are DER files the operator
  provisions, named relative to the manifest; every certificate is read (peers pin them, and a
  node's stable anchor is derived from its certificate), only this node's key is read. Its live
  member id also includes a fresh boot nonce. Material the TLS
  stack cannot use (a key that does not match the certificate, a key shape it does not take) stops
  the boot naming the node.
- `f` is the regional fault tolerance: a write commits once `f + 1` nodes hold it. Regional
  consensus uses `2f + 1` voters; the separate root group must also retain a quorum (see
  "Creating the first consensus group" above). A manifest that could never commit (`fewer than f + 1`
  nodes) is refused.
- `address` is the IP **or DNS name** peers dial and the node's base port: it serves probes on that
  UDP port and records on the next, every peer on the same two sockets, so open those two ports.
  Every node computes the same map from the same file. A name
  (`slates-0.slates.default.svc.cluster.local:7000` — the Kubernetes deployment of
  [deploy.md](deploy.md) names each node by its per-pod DNS name) is resolved by the daemon at every
  fresh dial through the nameservers of this host's `/etc/resolv.conf`, so a peer that came back
  under a new address (a rescheduled pod) is reached on the next re-dial; a name that does not
  resolve is counted under the `fleet.resolve` refusal in `status` and dialed again next period. A
  named node binds its own two sockets on every interface. A manifest that names a node on a host
  with no IPv4 nameserver in `/etc/resolv.conf` stops the boot naming the file.
- `domain` and `region` are optional per-node non-negative integers, both unset by default and set
  only for a real topology. `domain` is the node's failure domain (a rack or zone id): placement forms
  each object's copyset across distinct domains, so nodes that share a `domain` are treated as
  co-located. `region` is the node's region: the root group across regions agrees on which regions
  exist and routes cross-region, and a node with no `region` is in the single default region (a fleet
  that declares none is one region). Neither is derived from the address — several nodes may share one
  host and IP in a test or dev fleet without sharing a domain or region.
- `durability` is an optional fleet-level object, `{ "accepted_loss": <0..1>, "coincident_failures": <n> }`,
  the operator's accepted probability of losing some object when `coincident_failures` hosts fail at once.
  It is unset by default (no check). When set, every configuration change is checked against it and a breach
  is **surfaced** as a health signal (never silently over-scattered): a breach is a recovery-vs-durability
  conflict to resolve by raising `f`, the re-replication bandwidth, or the failure-domain granularity. It is
  a policy, so it is stated, never derived.
- `mirrors` is an optional fleet-level object mapping a region id to its mirror region id, e.g.
  `{ "0": 1, "1": 0 }` (region 0's data is mirrored to region 1). A region with a mirror is **not** failed
  over automatically when its hosts are lost — a region that is merely partitioned would be failed over while
  still serving, creating a second owner — so it stays in the fleet until an operator deliberately promotes
  its mirror; a region with no mirror is simply retired when its hosts are all lost. Empty by default. The
  operator promotes a lost region's mirror with `slates promote-region REGION`, issued on **any** node: it
  commits `PromoteRegion` on the root group and every node re-homes the lost region's volumes to the mirror. A
  node that does not lead the root group forwards the command to the leader it knows, so the operator need not
  find the leader first; `NotRootLeader` is returned only when no leader is currently known (retry), and a
  region with no declared mirror is refused `Unsupported`.

`slates status` on any node shows its place in the fleet: `fleet_host` (its member id),
`fleet_f`, `fleet_host_epoch`, `fleet_members` (the members it holds alive) and
`fleet_peers_probed` (peers with a formed session; the mesh is up when this is the member count
less one); then the two consensus groups as this node drives them — `fleet_council_leads` (whether
this node is the regional configuration council's elected leader), `fleet_council_base_periods` and
`fleet_council_span_periods` (the election timeout it derived, in coordinator periods: base
`⌈10 × max(broadcast RTT tail, heartbeat) / heartbeat⌉`, the span the same over the RTT variation),
`fleet_council_rtt_tail_ns` and `fleet_council_rtt_spread_ns` (the measured tail and spread they
came from; zero before any sample) and `fleet_council_samples` (the round trips behind them — a
loopback fleet derives the ten-period floor from its samples, a WAN fleet a larger base), and the
same six `fleet_root_*` lines for the root group across regions. A single daemon shows the same
lines, degenerate: `f` 0, itself the one member, leading both groups after explicit bootstrap,
at the floor with no sample.

Then one block per shard: its counters (`shard N: clients=… volumes=… served=…`), its refusals by
kind, its health signals (`shard N catalog.volumes: 3 (age 0 ns)` — a signal that is absent prints
`absent/unknown` or `absent/degraded`, never a bare `0`), and its telemetry. `status` **drains** each
shard's bounded telemetry ring (the chokepoint spans recorded since the previous `status`, up to what
one reply carries): `shard N drain: spans=… window_ns=… horizon_ns=… shed_before=… dropped_total=…
remaining=… missing_links=…` — `shed_before` is the loss marker (spans the ring shed since the
previous drain, before this batch), `remaining` what the bound left for the next `status`,
`missing_links` spans whose cause was not carried across a boundary — followed by one line per
chokepoint of the registry (`shard N span shard.op: spans=3 latest_age_ns=812`). A chokepoint whose
newest span is older than the horizon (the failover SLO), or that has none, prints
`absent/unknown (…)` with its last sighting as an age and whether any producer of it runs on this
host, never a stale age as a live value.

## Client verbs

The instance is `--instance`, else `SLATES_ENDPOINT`, else `default`.

```
slates volume create NAME (--bounded SIZE | --dynamic MAX) [--fold] [--locked] [--base DIR] [--json]
slates volume list [--json]
slates volume stat ID [--json]
slates volume snapshot ID [--json]
slates volume destroy-snapshot ID SNAPSHOT [--json]
slates volume clone ID SNAPSHOT NAME [--json]
slates volume resize ID (--bounded SIZE | --dynamic MAX) [--json]
slates volume destroy ID [--json]
slates volume placed ID [--snapshot N] [--mirror] [--json]
slates green NAME [--require-evidence] [--base VOLUME --snapshot N] [--json]
slates versions GREEN [--json]
slates changed-since GREEN VERSION [--json]
slates work GREEN NAME [--json]
slates edit WORK PATH AT DELETE TEXT [--json]
slates submit WORK [--evidence HEX] [--json]
slates rebase WORK [--json]
slates advance ATTACHMENT [VERSION] [--json]
slates read VOLUME PATH [--version N | --attachment A]
slates mount ID PATH
slates unmount PATH
slates land ID TARGET [--snapshot N] [--include P] [--exclude P] [--grant N] [--json]
slates grants [--json]
slates grant LANDING MANIFEST [--session] [--term SECONDS] [--json]
slates audit [--since N] [--json]
slates enroll [--account UID] [--json]
slates revoke CONSUMER [--json]
slates share ID PRINCIPAL [--read] [--write] [--admin] [--json]
slates run [--keep] [--json] -- CMD [ARG ...]
slates exec --volume V --at PATH -- CMD [ARG ...]
slates attach ID [--read | --write] [--snapshot N] [--json]
slates detach ATTACHMENT [--json]
slates status [--json]
slates status ID [--drift] [--json]
slates promote-region REGION [--json]
slates base read ID PATH
slates base rewitness ID [PATH ...] [--json]
slates base pin ID [PATH ...] [--json]
slates profile [--quick] [--json]
slates mcp [--instance NAME] [--http PORT]
```

Sizes take a binary unit (`512MiB`, `4GiB`; `B`, `KiB`, `MiB`, `GiB`, `TiB`) or plain bytes.
Ids are 32 hexadecimal characters as `create` prints them.

Output is plain and stable: one `key: value` per line (`create`, `stat`, `status`, `attach`,
`snapshot`, `clone`, `pin`), one record per line (`list`: id, name, `referenced=`, `unique=`,
`overlay=`; `status --drift` and `rewitness`: one path per line), `ok` for the verbs that
return nothing, and the raw bytes for `base read`.

`--json` emits a machine-readable form of every verb — the read and query verbs (`status`,
`status ID` / `volume stat`, `volume list`, `versions`, `changed-since`, `submit`, `rebase`), the
volume-lifecycle verbs (`create`, `snapshot`, `clone`, `resize`, `destroy`, `destroy-snapshot`,
`placed`, `attach`, `detach`, `pin`, `rewitness`, `grants`, `audit`, `land`), and the merge create
verbs (`green`, `work`, `edit`) — with the same fields as the text form and the same schema the MCP
surface emits (one definition, two surfaces). A creating verb returns `{"id": "<hex>"}` (the same
key across `create`, `clone`, `green` and `work`); an outcome-only verb returns `{"ok": true}`. The
one exception is `base read`, which streams a file's raw bytes with or without `--json`. `status
--json` carries every shard's block under `shards` (an array: the counters, `refusals`, `signals`
with each signal's `value` — `null` when absent — and its `absence` meaning, and `telemetry`: the
drain's markers, the `chokepoints` registry with each chokepoint's `fresh`/`latest_age_ns`/`absence`/
`expected`, and the `spans` with their `request`, `trace`, `span` and `cause` identities), exactly what
the text form prints and the MCP `slates.status` tool returns.
`slates mount ID PATH` mounts the volume at an existing user-owned directory over the loopback
NFS bridge; `slates unmount PATH` removes it. `slates mcp` serves the MCP tools over stdio, or
loopback Streamable HTTP with `--http PORT`.

The merge flow (§4.16): `green NAME` starts a green from scratch, or from a **complete immutable
base** with `--base VOLUME --snapshot N` — a snapshot of a volume whose whole tree is in memory
(`base pin VOLUME` first for an overlay; a snapshot still served from the host directory is refused
`ConsistentBaseUnavailable`), and `--require-evidence` makes every submit carry an evidence reference
(`submit WORK --evidence HEX`, refused `EvidenceRequired` without one). A green is written by nothing
but its merge task: `edit`, a write `attach`, `volume snapshot` or `resize` on one refuse
`ReadOnlyVolume`. `attach GREEN --read` pins the green's head version (the attachment's `version`);
the attached view never moves until `advance ATTACHMENT [VERSION]`, which prints the version now
pinned and the paths it invalidated. `read VOLUME PATH` streams a file's raw bytes at the head,
`--version N` at a green version, `--attachment A` at the version an attachment pins.

A fleet whose deployment manifest declares a `durability` policy (`accepted_loss`, `coincident_failures`)
refuses a volume create, clone, snapshot or merge submit with `DurabilityUnmet { coincident_loss,
accepted_loss, coincident_failures }` while its committed configuration's coincident-loss probability is
above the accepted one — the resolution is the operator's (more copies, more re-replication bandwidth,
tighter failure domains, or a policy that accepts the loss); reads, destroys, resizes and `status`
continue, and `status` counts the refusals under `durability_unmet`.

Exit codes: 0 done; 1 the daemon refused (the refusal is named on stderr, e.g.
`AlreadyExists`); 2 usage; 3 no daemon at the instance; 4 the command itself failed.

`slates grant LANDING [--session] [--term SECONDS] [--json]` issues the grant a presented landing
needs (§4.13, §4.15 step 3). It is the human's surface: the command runs as the user who started
the daemon's anchor, reads the grant-issuer secret the daemon minted into the anchor segment at
start, and proves it with a keyed hash over the exact landing — its id, the manifest hash the
human saw, the scope and the term — which the daemon recomputes before issuing. A proof that does
not verify (a forged, replayed or modified-plan approval) is refused `GrantIssuerUnverified` and
counted; the MCP server and the SDKs carry no proof by construction and are refused by kind.
Passing `--grant N` to `land` then consumes the grant; the landing must present the same manifest.
Neither a control-channel label nor the caller's uid is proof of human approval — only the secret
is, and only the anchor's user maps it. `slates anchor` prints the variables to export for that
(`slates anchor: issuer surface: export SLATES_ANCHOR=… SLATES_ANCHOR_LEN=…`) on macOS and Windows,
where the segment is a named object of the user; on Linux it is a descriptor only the anchor's
children hold.

`slates run [--keep] [--json] -- CMD [ARG ...]` is the harness verb of §4.13: it runs `CMD` as a
consumer enrolled for the command's lifetime. The command's own `slates` client (the CLI, an SDK, the
MCP server) finds the capability on an inherited descriptor named by `SLATES_CONSUMER_FD` and binds its
channel to the consumer before any verb — the capability is never in an argument, never in the
environment, never printed — so what it creates is the consumer's, and the account (or another
consumer) is refused on it until `share` says otherwise. `run` announces the consumer first
(`consumer: N`, or `{"consumer": N}` under `--json`) so a human can `share` volumes with it while it
runs, then hands the command the terminal and exits as the command exited; the enrollment is revoked
when the command ends unless `--keep`. `slates enroll [--account UID]` enrolls a consumer and shows its
capability once — for a harness that delivers it by its own means (`Delivery` in `slates-ipc`; the SDKs'
`pass_fds`/`stdio`/`handle_list` spawns); `slates revoke CONSUMER` ends an enrollment (every later verb
from its channels refuses `ConsumerRevoked`); `slates share ID PRINCIPAL [--read] [--write] [--admin]`
sets a principal's rights on a volume (`uid:N`, `consumer:N` under your account, or
`consumer:ACCOUNT/N`; no switch removes the entry). `enroll`, `revoke` and `run` are the anchor user's
surface like `grant` and need its variables; `share` is the volume owner's.

## Planned interface corrections

The following are requirements, not additional runnable commands:

- One typed operation definition supplies CLI help, MCP schemas, SDK calls and refusals.
- Scoped names as well as ids; consistent `--help`, structured `--json`, pagination and
  actionable errors. `--json` now covers every verb (above) with the MCP schema; pagination
  (cursors) and scoped names are still open.
- Discover the instance/root from enrolled context; report success only when the requested
  path or guest tag is ready, including consumer rights and supported attachment capabilities.
- Distinguish a live overlay's retained base from a complete immutable capture. Cloning shares
  the base plus changed entries; a remote clone must not lose untouched files.
- Show actual quota, reserved capacity, used bytes, retention, placement and freshness;
  report provisioning, capture and mount/device setup costs separately.
- Supply a protected human preview/approval surface; keep issuance absent from MCP and SDKs.

The acceptance cases are AC-5.9–AC-5.11 in the unified design. No new command was implemented
or exercised for this documentation correction.
