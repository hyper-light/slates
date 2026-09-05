# slates

Copy-on-write workspaces for concurrent agents, served as host paths or guest filesystems.

Slates is a Rust VFS whose target is isolated RAM-backed edits, cheap clones over retained
bases, and disk changes only through a human-granted landing. Host tools use native mounts;
OCI containers and microVMs are planned consumers, with virtio-fs serving Linux guests.
The provisioning target is under 50 µs; base capture and attachment setup are separate costs.

Status, 2026-09-05: local core/server/CLI and part of the Linux bridge exist; no release yet.
Strict capacity reservations, content recovery, complete mounted POSIX behavior, consumer
isolation and fleet correctness have open findings. MCP, SDKs and guest/native non-Linux
attachments remain planned. See the [gap ledger](docs/wip/GAPS.md#8i-a-9-contract-correction-and-open-implementation-gaps-2026-09-05).

Read the [unified design](docs/wip/SLATES_DESIGN.md), [current CLI guide](docs/cli.md),
[Hecate contract review](docs/wip/research/hecate-contract-review.md) and
[recorded benchmarks](docs/wip/BENCHMARKS.md). Project rules are in [AGENTS.md](AGENTS.md)
and [CLAUDE.md](CLAUDE.md).
