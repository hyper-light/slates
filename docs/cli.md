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

## Deploying a fleet

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
  node's member id is derived from its certificate), only this node's key is read. Material the TLS
  stack cannot use (a key that does not match the certificate, a key shape it does not take) stops
  the boot naming the node.
- `f` is the fault tolerance: a write commits once `f + 1` nodes hold it, so a fleet of `2f + 1`
  keeps committing through `f` deaths. A manifest that could never commit (`fewer than f + 1`
  nodes) is refused.
- `address` is the IP peers dial and the node's base port: it serves probes on that UDP port and
  records on the next, every peer on the same two sockets, so open those two ports. Every node
  computes the same map from the same file.
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

`slates status` on any node shows its place in the fleet: `fleet_host` (its member id),
`fleet_f`, `fleet_host_epoch`, `fleet_members` (the members it holds alive) and
`fleet_peers_probed` (peers with a formed session; the mesh is up when this is the member count
less one). A single daemon shows the same lines, degenerate: `f` 0, itself the one member.

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
slates green NAME [--json]
slates versions GREEN [--json]
slates changed-since GREEN VERSION [--json]
slates work GREEN NAME [--json]
slates edit WORK PATH AT DELETE TEXT [--json]
slates submit WORK [--json]
slates rebase WORK [--json]
slates mount ID PATH
slates unmount PATH
slates land ID TARGET [--snapshot N] [--include P] [--exclude P] [--grant N] [--json]
slates grants [--json]
slates audit [--since N] [--json]
slates exec --volume V --at PATH -- CMD [ARG ...]
slates attach ID [--read | --write] [--snapshot N] [--json]
slates detach ATTACHMENT [--json]
slates status [--json]
slates status ID [--drift] [--json]
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
one exception is `base read`, which streams a file's raw bytes with or without `--json`.
`slates mount ID PATH` mounts the volume at an existing user-owned directory over the loopback
NFS bridge; `slates unmount PATH` removes it. `slates mcp` serves the MCP tools over stdio, or
loopback Streamable HTTP with `--http PORT`.

Exit codes: 0 done; 1 the daemon refused (the refusal is named on stderr, e.g.
`AlreadyExists`); 2 usage; 3 no daemon at the instance; 4 the command itself failed.

There is no `slates grant` issuance verb yet. The server has grant records and a control
transport, while the ring refuses the grant kind. Passing `--grant N` consumes an existing
grant; it cannot create one. Do not treat a control-channel label or the caller's uid as
proof of human approval; the protected issuer contract remains open (§4.13).

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
