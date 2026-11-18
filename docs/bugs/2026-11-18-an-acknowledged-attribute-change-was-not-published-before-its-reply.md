# An acknowledged attribute change was not published before its reply (2026-11-18, §4.8 barrier, D-18, AUD-29-82; A-290)

## Description

Every transport that serves slates' shared FUSE dispatch (a kernel's `/dev/fuse` mount, slates' own virtio-fs device,
a Shards stream) holds a reply whose change a restart must keep until the owner's barrier has published the volume
(AUD-29-82): `slates_bridge_fuse::bridge::needs_barrier` names those requests. It named every namespace and metadata
change, the allocations and the commit points of data, but not `SETXATTR` and `REMOVEXATTR`. So an extended attribute
set or removed through one of those transports was answered as soon as it was applied, and a daemon that died before
its next publication lost it, while its caller had been told it was done. NFSv4.2's barrier already named its
attribute calls (`crate::nfs::needs_barrier`), so the same change over NFS survived.

## How it showed

Found by reading, while A-290 made a guest kernel's own attributes (`security.capability` from `setcap`,
`trusted.overlay.*` from an overlayfs upper directory) the guest's data on a Shards stream: a guest's `setcap`
acknowledged and then lost would leave a binary without the capability its VM was told it has.

## Measurement

`a_guests_acknowledged_attribute_survives_a_daemon_restart` (`crates/server/tests/shards.rs`): under a daemon whose
anchor segment the test holds, a guest sets `user.gone`, `user.kept` and `security.capability` on a file and removes
`user.gone`, every reply a success; the daemon stops and a second starts over the same segment. Red-checked on macOS
arm64 (kiln run r423, runner f3f618f29b31df31a1f52482b39578c2, 2026-11-18, load average about 52), by removing the two
opcodes from a copy of `needs_barrier` on the runner and restoring it after: 3 of 3 runs failed at the first check, the
second daemon's NFSv4.2 port listing nothing where `user.kept` should be (`left: []`); with the two opcodes the test
passes.

## Root cause

`needs_barrier`'s list was written for the namespace and inode-attribute requests of the FUSE dispatch's first form
(AUD-29-82), before the dispatch served extended attributes (§4.6's FUSE status of 2026-10-06), and nobody added the two
attribute opcodes when they were.

## Edits

- `crates/bridge-fuse/src/bridge.rs`: `needs_barrier` names `Opcode::SetXattr` and `Opcode::RemoveXattr`, and its doc
  says so.
- `crates/server/tests/shards.rs`: the test above, failing first.

## Siblings

- WinFsp's own barrier list (`crates/bridge-winfsp/src/host.rs` `Op::needs_barrier`) has no extended-attribute
  operation, since WinFsp serves no EA (QueryEa is refused); nothing to add there.
- NFSv4.2's barrier names `SETXATTR` and `REMOVEXATTR` already; NFSv3 has no extended attributes.
- A work's declared `set_xattr` and `remove_xattr` (SDKs, MCP) are recorded operations of the work's journal, durable as
  any declaration is; not affected.
