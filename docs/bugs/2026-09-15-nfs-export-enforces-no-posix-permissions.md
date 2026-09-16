# The NFS export enforced no POSIX permissions: any caller could read, write, remove and chmod what it could name

Date: 2026-09-15
Area: `crates/bridge-nfs/src/procedures.rs` (every NFSv3 procedure), `crates/bridge-nfs/src/rpc.rs`
(the `AUTH_SYS` credential parser), `crates/bridge-nfs/src/multi.rs` and `crates/server/src/nfs.rs`
(the identity threaded per request); new `crates/bridge-nfs/src/access.rs`
Severity: correctness of §4.6 "POSIX and transparency acceptance" on the macOS mount, and of
`docs/wip/EQUIVALENCE.md` §8 ("not permission for a mounted filesystem to ignore ownership, modes …"):
the NFS-loopback mount applied no discretionary access control at all.

## Symptom

The CI macOS conformance lane (`cargo xtask conformance all`, run as root) reported pjdfstest at
**8,686 cases: 3,350 passed, 5,336 failed, all unexpected** against an empty expected-failure list
(`docs/wip/conformance/records/native-macos-nfs.pjdfstest.json`, 2026-09-14). The failures cluster
where permission is at stake — rename 3,115, chown 1,043, unlink 240, open 214, link 190, chmod 175 —
and begin in `chmod/00.t`. pjdfstest, running as root, drops to uid 65534 for its permission-denied
assertions; every one of them returned success.

## Root cause

`EACCES` appeared nowhere in the vfs, the NFS bridge or the daemon's NFS path. The volume stores a
mode, uid and gid per inode, and the daemon already read the caller's uid from each call's `AUTH_SYS`
credential (`server/src/nfs.rs`, "each request runs as the mounting user"), but nothing compared the
two:

- `ACCESS` answered from the **owner's** permission bits whoever asked (`granted_access(mode,
  requested)`; its comment: "the per-principal check … is owed"). A client answers its own `open(2)`
  and `access(2)` from this reply, so every caller was told it had the owner's access.
- No other procedure checked anything. `LOOKUP` resolved names in unsearchable directories, `REMOVE`,
  `RENAME`, `CREATE`, `MKDIR`, `SYMLINK` and `LINK` changed directories the caller could not write,
  `READ` and `WRITE` ignored the mode, `READDIR` listed unreadable directories, and `SETATTR` let any
  caller `chmod`, `chown` (to any owner) and set times on any object. PATHCONF reported
  `chown_restricted` false, and the volume's `chown` "imposes no privilege check".

The FUSE mount never had this gap because it mounts with `default_permissions`
(`crates/bridge-fuse/src/mount.rs`), so the kernel applies the rules from the attributes the bridge
reports. An NFS server is the one bridge that must apply them itself: its client checks nothing but
`ACCESS`, and the server sees every operation.

The supplementary groups were not read either: `auth_sys_creds` stopped at the primary gid, so even a
correct group-class check would have missed a caller's membership through a supplementary group.

## Fix

**A pure access-control module** (`crates/bridge-nfs/src/access.rs`, no I/O, unit-tested on every
host) states the rules — POSIX.1-2017 §4.5 "File Access Permissions" and the ownership clauses, with
the superuser exemptions every Unix applies:

- the class rule (exactly one of owner, group, other applies; group matches the primary or any
  supplementary group);
- the superuser reads, writes and searches anything and executes a file only if some execute bit is
  set;
- `chmod` needs ownership; `chown` is restricted (`_POSIX_CHOWN_RESTRICTED`: only the superuser
  changes an owner, the owner may set the group to one of its own); explicit times need ownership,
  "now" needs ownership or write permission;
- the sticky bit: an entry is removed or renamed out of a sticky directory only by its owner, the
  directory's owner or the superuser;
- set-id hygiene: a non-superuser's write or `chown` clears a file's set-id bits, and a non-superuser's
  `chmod` cannot set `S_ISGID` on an object of a group the caller is not in;
- the I/O owner override Linux `nfsd` applies (`NFSD_MAY_OWNER_OVERRIDE`): the owner reads and writes
  its own file whatever the bits, so an open descriptor survives a later `chmod`; `ACCESS` still
  reports the exact class verdict.

**Every procedure applies it** before any effect, refused typed — `NFS3ERR_ACCES` for a missing
permission bit, `NFS3ERR_PERM` for an ownership rule: `LOOKUP` (search), `READDIR`/`READDIRPLUS`
(read), `READ`/`WRITE` (the I/O rule, plus the set-id clearing after a write), `CREATE`/`MKDIR`/
`SYMLINK`/`LINK` (write and search on the directory; an UNCHECKED `CREATE` of an existing name is an
open and needs only search), `REMOVE`/`RMDIR` (write and search, then the sticky rule), `RENAME` (both
directories, the sticky rule on the source and on a replaced target, and write permission on a
directory moved to a new parent), `SETATTR` and the `sattr3` a create carries (the ownership rules,
then the set-id side effects). `ACCESS` reports the same verdict. PATHCONF now reports
`chown_restricted` true, on a volume and on the synthetic root.

**The identity is complete.** `auth_sys_identity` reads the uid, the primary gid and the
supplementary gids (bounded at the protocol's `gids<16>`, `AUTH_SYS_MAX_GIDS`; a credential claiming
more is refused before any group is read), and the whole identity rides through `VolumeSet::serve`
and `MultiExport` to the per-request `Export` (`Export::set_groups`, replacing `set_owner_gid`, whose
primary group still stamps created objects through `OpContext::owner_gid`). A request with no
credential (`AUTH_NONE`) still runs as root, as the design says, so a hand-rolled loopback client and
every existing test are unchanged.

## Verification

- `crates/bridge-nfs/src/access.rs` unit tests (8): the class rule, supplementary groups and the
  no-identity caller, the superuser's exemptions, the owner override, the sticky bit, restricted chown,
  the typed SETATTR denials, the set-id side effects.
- `crates/bridge-nfs/tests/auth.rs` (4): the groups are read in order; sixteen are accepted and a
  seventeenth refuses the credential.
- `crates/bridge-nfs/tests/procedures.rs` (8 new, by use over a real `VolumeBridge`):
  `access_answers_the_callers_class`, `a_directory_without_search_permission_refuses_lookup`,
  `chmod_needs_ownership_and_chown_is_restricted`,
  `remove_needs_a_writable_directory_and_honours_the_sticky_bit`,
  `create_and_mkdir_need_write_permission_on_the_directory`,
  `read_and_write_honour_the_mode_with_the_owner_override`,
  `readdir_needs_read_permission_on_the_directory`,
  `rename_checks_both_directories_and_the_sticky_source`; the existing
  `access_reflects_the_mode_not_the_request` now runs as the owner (the superuser is exempt) and
  `pathconf_reports_the_volume_limits` expects `chown_restricted`.
- `cargo test -p slates-bridge-nfs`: 83 passed, 0 failed. `cargo test -p slates-server --test
  nfs_mount` (the daemon's live loopback mount, create/write/read through a real `mount_nfs` as the
  mounting user): 4/4. `cargo clippy -p slates-bridge-nfs -p slates-server --all-targets -D warnings`
  clean; `cargo xtask literals` and `structural` ok.
- The pjdfstest re-run through the conformance harness is recorded in the lane record and the
  expected-failure list (the same change).

## Sibling sweep

- **FUSE** (`bridge-fuse`): unaffected — `default_permissions` has the kernel enforce these rules.
- **FSKit** (`bridge-fskit`): the kernel applies discretionary access control from the attributes the
  handler reports, as for FUSE; confirming it on a live FSKit mount is part of that spike, not this
  fix.
- **WinFsp** (`bridge-winfsp`): a different model (security descriptors), out of this fix's scope.
- **The volume core's `chown`** still "imposes no privilege check" by design: the core is a
  single-owner store and every transport edge is the place identity is known; the check lives at the
  edge, as the FUSE kernel edge does.
- **The conformance harness** could not run at all on this tree before this change: the audit batch's
  `f9c0fe9` made a fresh daemon refuse `volume create` until `bootstrap root`, and the harness's
  `Session::open` had no such step (the CLI and SDK suites did). Fixed in the same change
  (`xtask/src/conformance/slates.rs`), with the lane's per-file pjdfstest output now kept (`--keep`)
  and uploaded from CI as the review input the expected-failure list's header names.
