# `oci-check` accepts a replaced mount: the kernel reuses the identity it compares

- **Date:** 2026-10-17
- **Area:** `crates/bridge-oci` (mount identity, AUD-29-66), `crates/server/src/oci.rs`, `crates/cli` `oci-check`
- **Status:** found by attacking; fix in this change
- **Class:** TOCTOU identity forgery / confused deputy (CWE-367, CWE-386)

## Description

`slates oci-check SOURCE MOUNT_ID DEVICE` exists to close a TOCTOU window. Its own documentation
states the guarantee: "A harness runs it immediately before its runtime binds the path; a replaced or
missing source refuses (exit 1), so the runtime is never handed a replacement or a bare directory
where the mount was." `mount_table.rs` repeats it: "A mount replaced at the same path — unmounted and
mounted again, or another filesystem mounted over it — has another identity."

That is false on Linux. A real run on the cachyos box (kernel 7.2.9-2-cachyos, 2026-10-17) swapped
one slates volume for another at the same path and `oci-check` certified the attacker's filesystem as
the verified mount:

```
victim mount:   id=927 dev=0:114  content=marker-from-VICTIM
oci-check -> unchanged: /run/user/1000/ociswap-60308   exit=0
--- unmount the victim, mount the attacker's volume at the same path ---
attacker mount: id=927 dev=0:114  content=marker-from-ATTACKER
oci-check with the VICTIM's recorded identity -> unchanged: ...  exit=0
```

Two different volumes, two different mount instances, byte-different content, and the check that
exists to tell them apart says `unchanged` and exits 0. A harness that does exactly what the docs
prescribe binds the attacker's filesystem into the container.

## Root cause

`MountIdentity` on Linux is `mountinfo`'s first field (the mount ID) and third field (`major:minor`).
Both are reused by the kernel:

- The mount ID comes from an IDA (`mnt_alloc_id`), which hands back the **lowest free** id. Unmount a
  mount and the next mount typically gets the same number — 927 in both runs above, first try, with
  no racing or grooming.
- A FUSE superblock's device is an anonymous bdev from `get_anon_bdev`, also a reused IDA number —
  0:114 both times.

This is a known kernel property, not a slates-specific accident: Linux 6.8 added
`STATX_MNT_ID_UNIQUE` precisely because the old mount ID is reused and therefore cannot identify a
mount instance. slates compared the reusable one.

Measured on the same host, same swap:

| | victim | attacker |
|---|---|---|
| `mountinfo` id (what slates compared) | 927 | **927** |
| `STATX_MNT_ID_UNIQUE` | 2156321564 | **2156321579** |

So the primitive that distinguishes them was available on this kernel and was not used.

## Impact

The whole point of AUD-29-66 is defeated on Linux. Anyone who can mount at the source path — which is
the exact actor the check is defending against, since a check against a party who cannot mount there
would be pointless — can have a runtime bind a filesystem of their choosing while the harness
believes it verified slates. It is a container-level substitution, not a slates-internal read: the
container gets the attacker's files for the volume it asked for.

Scope and honesty about the bound: the attacker needs the privilege to unmount and mount at that
path, so on a single-user host this is the user's own uid, and slates' control plane is already
strictly per-uid. The defeat matters where the harness and the mounting party are not the same
trust domain, which is the container case the verb is written for.

Not affected: the daemon's own containment. No slates data crosses a volume boundary here; the
failure is that a *host* path's identity is forgeable, so the wrong filesystem gets bound.

## Fix

Compare the identity the kernel does not reuse.

- `mount_table::mount_identity_at(path)` is a new paired-`#[cfg]` seam that reads the identity of one
  path now. On Linux it is `statx` with `STATX_MNT_ID_UNIQUE` (`AT_STATX_DONT_SYNC`, so a mount with
  a valid attribute cache answers without a round trip); the unique id is the identity and the device
  rides along as corroboration. If the kernel does not set the mask bit — before 6.8 — the seam
  refuses `NoUniqueMountId` rather than falling back to the forgeable id, because a check that cannot
  make its guarantee must say so (banned item 7: no weaker second path).
- `verify_host_mount` and `source_unchanged` stay pure over the table and take the live identity as a
  parameter, so the table is still what detects "missing" and reports what is there now, and the
  unique id is what decides "unchanged".
- The attach path (`server/src/oci.rs`) records the unique identity in the binding's evidence, so the
  two sides of the comparison are the same kind of id.

macOS is unchanged in this commit: the identity there is `statfs`'s `f_fsid`, the Linux escape does
not apply to it, and whether `f_fsid` is reused across an unmount/remount of the NFS loopback is not
established. It is probed separately rather than assumed.

## Verification

The attack re-run against the fix, same host, same two volumes, same path:

```
VICTIM   mountinfo_id=927 unique_id=2156362499 dev=0:114 content=marker-from-VICTIM
oci-check, verified mount still there -> unchanged: ...   exit=0
ATTACKER mountinfo_id=927 unique_id=2156362519           content=marker-from-ATTACKER
oci-check with the VICTIM's identity ->
  refused: SourceReplaced: ... is now mount 2156362519 on device 114, not the mount verified   exit=1
```

The kernel handed out mount id 927 for the replacement again, so the table still could not tell them
apart; the unique id did, and the check refuses. The unchanged case still passes, so the fix does not
simply refuse everything.

Regression tests. `crates/bridge-oci/tests/verify.rs` adds the case the bug is: the table shows the
verified identity and only the unique id differs — `Replaced`. `crates/cli/tests/cli.rs`
`a_container_source_swapped_for_another_mount_is_refused` performs the real swap through the CLI
(mount volume A shared, attach the container form, keep the evidence, unmount, mount volume B at the
same path, check) and asserts the refusal; it skips loudly where `user_allow_other` is not granted,
since the container form binds only a shared mount.

Gates: `cargo clippy --workspace --all-targets -- -D warnings` and `fmt --check` exit 0 on macOS and
on Linux; `slates-bridge-oci` 11/11; `slates-server --test attach_forms` 9/9 on both; the Linux cli
suite 32/32.

## Owed

macOS is not fixed and not proven unaffected. Its identity is `statfs`'s `f_fsid`, and whether that
is reused across an unmount and a remount of the NFS loopback has not been measured. The same probe
that found this — mount, record, swap, compare — decides it, and until it is run the macOS leg of
AUD-29-66 rests on an assumption rather than a measurement.

It was attempted and did not yield a number. An ad-hoc shell probe (2026-10-17) mounted the first
volume successfully — the kernel listed `slates:/victim on .../fsidprobe-pKcS (nfs, nodev, nosuid)` —
and then hung on the first access through the mount, printing nothing, until it was stopped about
two and a half minutes later; a plain `umount` then succeeded and the host was left clean (0 mounts,
0 stray processes). Whether the hang was the mount not yet being ready to serve, the probe's own
shape, or something in the loopback path was not established, so nothing is claimed from it either
way. The probe belongs in the test harness that already knows how to wait for a macOS mount to serve
(`crates/cli/tests/cli.rs`, the `mount_and_check` path), not in a shell script.
