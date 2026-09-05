# The `slates` command

This guide describes the implemented command grammar at `a1059ed`, reviewed 2026-09-05.
The [unified design](wip/SLATES_DESIGN.md) §4.12 describes the target interface; the
[gap ledger](wip/GAPS.md) §8i tracks what is still missing.

The current CLI is a development surface. `--locked` records intent without establishing
locked backing, and bounded/dynamic claims do not yet provide the design's host-capacity
guarantee. Daemon restart recovers metadata but can lose volume bytes and local snapshots.
`attach` returns an attachment record without creating a mounted path. `exec` is Linux-only
and currently expects an externally supplied `SLATES_ROOT` naming an established root mount.
There is no implemented CLI/MCP virtio-fs or OCI attachment flow, MCP server or SDK package.
These limitations are open correctness/integration work, not optional setup steps.

## Running a daemon

```
slates anchor [--instance NAME] [--quick] [--shards N]
```

The anchor measures the machine profile (seconds; `--quick` for tests), creates the shared
segment in RAM, publishes the profile into it, and supervises `slates daemon` as a child with
the segment handed over in its environment. It restarts a daemon that exits, kills one whose
heartbeat lapses, and gives up on a crash loop (the bound is derived from the recovery budget
and the measured daemon start). `SIGINT` or `SIGTERM` stops both. This path is intended to avoid explicit disk writes;
an anonymous shared segment alone does not establish locked residency or no swapping.

`slates daemon` run alone measures a profile and serves without an anchor (development).

## Client verbs

The instance is `--instance`, else `SLATES_ENDPOINT`, else `default`.

```
slates volume create NAME (--bounded SIZE | --dynamic MAX) [--fold] [--locked] [--base DIR]
slates volume list
slates volume stat ID
slates volume snapshot ID
slates volume clone ID SNAPSHOT NAME
slates volume resize ID (--bounded SIZE | --dynamic MAX)
slates volume destroy ID
slates volume placed ID [--snapshot N] [--mirror]
slates land ID TARGET [--snapshot N] [--include P] [--exclude P] [--grant N]
slates grants
slates audit [--since N]
slates exec --volume V --at PATH -- CMD [ARG ...]
slates attach ID [--read | --write] [--snapshot N]
slates detach ATTACHMENT
slates status ID [--drift]
slates base read ID PATH
slates base rewitness ID [PATH ...]
slates base pin ID [PATH ...]
slates profile [--quick] [--json]
```

Sizes take a binary unit (`512MiB`, `4GiB`; `B`, `KiB`, `MiB`, `GiB`, `TiB`) or plain bytes.
Ids are 32 hexadecimal characters as `create` prints them.

Output is plain and stable: one `key: value` per line (`create`, `stat`, `status`, `attach`,
`snapshot`, `clone`, `pin`), one record per line (`list`: id, name, `referenced=`, `unique=`,
`overlay=`; `status --drift` and `rewitness`: one path per line), `ok` for the verbs that
return nothing, and the raw bytes for `base read`.

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
  actionable errors. Today `--json` above belongs only to `profile`.
- Discover the instance/root from enrolled context; report success only when the requested
  path or guest tag is ready, including consumer rights and supported attachment capabilities.
- Distinguish a live overlay's retained base from a complete immutable capture. Cloning shares
  the base plus changed entries; a remote clone must not lose untouched files.
- Show actual quota, reserved capacity, used bytes, retention, placement and freshness;
  report provisioning, capture and mount/device setup costs separately.
- Supply a protected human preview/approval surface; keep issuance absent from MCP and SDKs.

The acceptance cases are AC-5.9–AC-5.11 in the unified design. No new command was implemented
or exercised for this documentation correction.
