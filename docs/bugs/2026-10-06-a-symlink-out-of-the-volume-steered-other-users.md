# A symlink out of the volume steered other users' writes

Date: 2026-10-06.
Area: `crates/bridge-core` (`readlink`) and every mount transport's error map. A-107.
Conditions: 3 and 4 (no disk escape; escapes adversarially tested).

## Description

In an adversarial container run on Linux FUSE, an agent created `s -> /home/tester/target-outside` inside its
volume. `echo x > s` through the mount then created the host file. The daemon wrote nothing; the kernel followed the
link with the writer's own permissions. Any other process writing into the volume, including root, would be steered
the same way. This is the confused-deputy pattern Linux's `fs.protected_symlinks` exists to stop (CWE-61).

## Root cause

`VolumeBridge::readlink` returned every target to every caller. The volume is a tree an agent controls, so an
absolute or upward-climbing target is a pointer the agent chose into the host, and the kernel resolves it for
whoever asks.

## Edits

- `crates/bridge-core/src/links.rs` (new): `leaves_volume(link_path, target)`. A target leaves the volume when it is
  absolute, when its leading `..` components climb above the link's directory depth, or when it has a `..` after a
  name.
- `crates/bridge-core/src/volume_bridge.rs` `readlink`: when the transport names the caller's uid and it differs from
  the link's owner, a target that leaves the volume is refused `VfsError::LinkProtected`.
- `crates/vfs/src/error.rs`: `LinkProtected` (`EACCES`), mapped on FUSE, NFS, WinFsp and FSKit.
- `crates/bridge-fuse/src/bridge.rs`: `ReadLink` now carries the request header's uid into the context.

## Tests

- `a_link_out_of_the_volume_resolves_only_for_its_owner` (bridge-core): an operator is refused on an absolute and a
  climbing link, the owner is answered, an inside link is answered to both, and an SDK context with no uid is
  answered. Mutation check: with the condition disabled it fails with `left: Ok("/etc/cron.d/x")`.
- `targets_are_classified_by_where_they_can_lead` (bridge-core unit test, 12 cases).
- Linux FUSE end to end (numbers in A-107): root and a second user are refused on links out of the volume and
  answered on links inside it; a root-planted link out is refused to the mount's user.

## Siblings

- The NFS client's symlink cache can serve a target to a second local user without a request, so on NFS the rule is
  best-effort (recorded in A-107).
- WinFsp and FSKit contexts carry no Unix uid, so the rule does not engage there. Windows reparse points need
  `SeCreateSymbolicLinkPrivilege` or developer mode to create, and FSKit is a secondary backend.
