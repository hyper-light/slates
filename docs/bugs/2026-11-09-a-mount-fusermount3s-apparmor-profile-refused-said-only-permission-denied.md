# A mount fusermount3's AppArmor profile refused said only "Permission denied"

**Found** 2026-11-09 by main's live-guest lane on GitHub's ubuntu-26.04 runner image (runs 42669607818, 42678193238,
42688024228). Measured by slabs (diag-slabs-51, run 42695696906); the lane fixed separately (`.github/workflows/ci.yml`,
the workspace bound at `/mnt/src`). This record is the product's half, decided by the supervisor on 2026-11-09:
diagnose after the kernel refuses, never predict.

**Description.** On a host whose `fusermount3` is confined by an AppArmor profile, Ubuntu 26.04's
`/etc/apparmor.d/fusermount3`, a `slates mount` at a directory the profile does not allow failed with "the mount
helper returned no device descriptor (HelperExit { code: Some(1) }): fusermount3: mount failed: Permission denied".
Nothing in it said that a security policy, not the directory's permissions, refused the mount, or where to look.

**Root cause.** The daemon passed on the helper's own message (`MountError::NoDevice`, answered `TargetUnavailable`).
The profile is the host's: inside a container the host's kernel still applies it to the container's `fusermount3`
(an exec'd binary with an attached profile enters it, `apparmor=unconfined` notwithstanding), while the profile's
file is not in the container at all. So no file inside the container can say why.

**The evidence the diagnosis rests on** (diag-slabs-57, run on ubuntu26 20261101.186.1, kernel 7.0.0-1019-azure):
`fusermount3` exec'd and held unreaped after it exits (`waitid` with `WNOWAIT`), its label read from `/proc/<pid>/`:
- on the host as the runner's user (uid 1001; fusermount3 3.18.2, setuid): `attr/apparmor/current` and `attr/current`
  both `fusermount3 (enforce)`;
- on the host as root: the same;
- in the lane's container (rust:1.98.1, fusermount3 3.17.2) as root, the container itself `unconfined`: the same.

A process keeps its credentials, and so its label, until it is reaped, and every caller measured could read it, the
unprivileged one too. So the label alone is the evidence; no file fallback is kept, since no leg would run it (§2 item 7).

**The edits.**
- `crates/bridge-fuse/src/mount.rs`: a helper that handed back no device has its label read while it is held unreaped
  (`HelperGuard::exited_label`), before it is reaped; when its message says the kernel refused the mount and the label
  names a profile and mode, the refusal is `MountError::RefusedByHelperPolicy`.
- `crates/bridge-fuse/src/helper_policy.rs`: the label read (`label_of`) and its reading (`policy_of`); the profile's file
  and the local-override file are named by Ubuntu's convention as where to look, never read. No directory list.
- `crates/ipc/src/protocol.rs`: `Refusal::MountRefusedByHelperPolicy { helper, profile, mode, policy_file,
  local_override }`, appended. `crates/server/src/fuse.rs` answers it; `crates/cli/src/verbs.rs` says what refused the
  mount and where an administrator would allow it.
- `docs/cli.md`: the directories Ubuntu 26.04's profile allows, with the image and the date they were measured.
- The test: `a_mount_the_helpers_apparmor_profile_refuses_names_the_profile` (`crates/cli/tests/cli.rs`, Linux), gated
  on the helper's label, run in the 26.04 container as root and on the 26.04 host as the runner's user.
