# A file created through the mount lists as root:wheel

Status: **fixed** — a created object is now owned by the mounting user (the request subject's uid)
*and* the mounting user's own group (the `AUTH_SYS` credential's gid, threaded per request), so a file
made through the mount lists as e.g. `adalundhe staff`, exactly as a native NFS server stamps it —
verified live. When a mount carries no credential group (`AUTH_NONE`), the group falls back to the
parent directory's (BSD/macOS create semantics). Date: 2026-09-09.

## Description

A file created through a live slates NFS mount lists as `root wheel` (uid 0, gid 0), no matter which
user created it. Reproduced live on this macOS host (uid 501 `adalundhe`, gid 20 `staff`) with
`cargo run -p slates-cli --example slates_mount`:

```
  ls of the mount:
drwxr-xr-x     2 root       wheel       0 Sep  9 17:51 .
-rw-r--r--@    1 root       wheel      37 Sep  9 17:51 hello.txt   <- created by adalundhe, listed as root
drwx------@ 6478 adalundhe  staff  207296 Sep  9 17:51 ..          <- the host dir above the mount
```

The design's contract is the opposite: "Each request runs as the mounting user (the daemon reads the
uid from the `AUTH_SYS` credential, §4.13; `AUTH_NONE` falls back to root)" (SLATES_DESIGN.md §4.6,
the mount section). A file an ordinary user makes must be owned by that user, not root.

## Root cause

The mounting user's identity is threaded correctly all the way to the operation context, but is never
*applied* to the inode a create makes.

- The NFS server reads the uid from each call's `AUTH_SYS` credential and turns it into the request
  subject: `subject_of` (crates/server/src/nfs.rs) → `auth_sys_uid(body)` → `Principal::Uid { uid }`,
  passed to `MultiExport::new`. So `OpContext.subject` carries the real mounting uid.
- The shared operation layer uses that subject only to *authorize* the write. `VolumeBridge::create`,
  `::mkdir` and `::symlink` (crates/bridge-core/src/volume_bridge.rs) call `authorize_write(cx)` and
  then `Volume::create_file_no` / `mkdir_no` / `symlink_no` — none of which is given an owner. The new
  inode is born with the volume core's default `uid: 0, gid: 0` (`Inode::new`, crates/vfs/src/inode.rs)
  and nothing ever overwrites it.

So the created inode's owner is (0, 0) — root:wheel — and `fattr3` faithfully reports it
(`Export::fattr3` maps `node.uid`/`node.gid`, crates/bridge-nfs/src/procedures.rs). The defect is the
missing ownership stamp on create, not the credential path (which works) or the attribute mapping
(which is faithful). The synthetic host-root directory's `root`/`wheel` in `MultiExport::root_fattr3`
is *intended* (a read-only directory listing volumes) and is unrelated.

Confirmed by a bridge-level test that drives the real create path under a non-root subject
(crates/bridge-core/tests/volume_bridge.rs
`a_created_object_is_owned_by_the_mounting_user_and_its_parent_group`): before the fix it panics with
the created object's uid `0` where the subject's `501` is required.

## Impact

Every object created through a live mount (files, directories, symlinks) is owned by root instead of
the user who made it, on every platform's mount path. It looks to the user as if their own scratch
files need privilege to touch, and it violates the "runs as the mounting user" contract that the whole
per-request credential path exists to honor. It is not a privilege escalation — the daemon holds no
privilege and writes only RAM — but it is a correctness and transparency defect: slates is supposed to
look like a normal path, and a normal path does not hand your new files to root.

## Fix (applied)

The ownership stamp belongs in the shared operation layer, so every transport inherits it (NFS today;
FSKit/FUSE when they mount). `VolumeBridge` gains one helper, `stamp_created_owner`, called by
`create`, `mkdir` and `symlink` immediately after the object is made:

- **uid** ← the request subject's uid (`OpContext.subject`, a `Principal::Uid`). This is the mounting
  user the credential named. A non-uid subject (a Windows SID or a certificate — never on the POSIX
  mount path) carries no uid, so the born owner stands and that platform's own ownership model governs.
- **gid** ← the mounting user's own group when the request's credential named one — the `AUTH_SYS`
  gid, carried on the new `OpContext.owner_gid` — matching what a native NFS server stamps; otherwise
  (no credential group, `AUTH_NONE`) the *parent directory's* group, read with one `Volume::stat` of
  the parent, the BSD/macOS local-filesystem create rule. Applied with the existing `Volume::chown`
  that `setattr` already uses, so no new vfs surface is added.

The vfs core stays identity-agnostic (it never learns about principals); the identity→ownership mapping
lives in the bridge, where the authenticated context already is.

### Threading the credential group (the second half, Ada's call 2026-09-09)

The group is deliberately *not* part of the §4.13 identity `Principal` (which is uid-only — a user is
the same principal for authority regardless of their group). It rides beside the subject as a distinct
"file ownership to stamp" value:

- `bridge-nfs`'s `auth_sys_creds` reads both uid and gid from one `AUTH_SYS` credential (`auth_sys_uid`
  and the new `auth_sys_gid` are thin projections of it).
- The daemon's NFS path reads the group with `owner_gid_of` and carries it beside the subject as one
  `Requester` (subject + group), which rides to a volume's owner shard exactly as the subject already
  did — so a cross-shard request stamps the same group.
- `bridge-core`'s `OpContext` gains `owner_gid: Option<u32>`; the `Export` overlays the mount's group
  onto every request's context (`set_owner_gid`, set per request by the export edge), and
  `stamp_created_owner` uses it when present. This kept `Attachments::attach` and the 40 `Export::new`
  call sites untouched — the group is set on the `Export`, not the authenticated attachment.

Proven by use: `crates/bridge-core/tests/volume_bridge.rs`
`a_created_object_takes_the_request_group_when_the_credential_names_one` (the created object takes the
credential's group over its parent's), the parent-inherited half covered by the sibling test; and live
on a real kernel mount — a file created through the mount now lists as `adalundhe staff` (was
`root wheel`, then `adalundhe wheel` after the uid-only fix).

### Remaining (minor, not the reported defect)

The volume's *root* directory (`.` seen at the mountpoint) is still created `gid 0` / `uid 0` at
provisioning (`Volume::create`) and shared across mounts, so it lists as `root wheel`. This is the
mount root's own ownership, not a "file created through the mount" (the reported and fixed defect); a
native NFS export root is often root-owned too. Stamping it to the provisioning user would need the
creating principal's group carried into provisioning — a separate, larger change, left for later.

## Sibling sweep

The three creating verbs (`create`, `mkdir`, `symlink`) shared the one gap and are all fixed; the test
drives all three. `link` (a hard link) adds a name to an existing inode and correctly does not restamp
ownership. `setattr` already honored `uid`/`gid` (BUG-8) and is unchanged. No other create path exists
in the bridge.
