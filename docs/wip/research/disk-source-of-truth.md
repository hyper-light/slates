# Disk as the source of truth: overlay volumes, drift, and landing under a human grant

Status: complete (written serially by the architect, 2026-09-04, for accepted amendment A-4).
Evidence tiers as in `README.md`. Items marked "verify" were not fetched in this session and are
listed again in §7. Every hecate quotation was read directly from `../hecate/docs` on 2026-09-04
and is cited to file and line.

## 0. The question and the verdict

Ada's rule (2026-09-04): "*Disk is the source of truth* period. true, we may be starting from
scratch, we may also be operating on an existing path with data/code/etc. in it." and "The goal
is to *only* write to disk on user permission grant." Two facts follow for the design:

1. A volume is either **scratch** (nothing beneath it) or an **overlay** over an existing host
   directory. For an overlay, untouched entries are served from disk on demand; the agent's
   changes live in memory as exactly the entries that diverged; disk changes under witnessed
   entries are detected and reported, never absorbed.
2. The only disk-writing verb is a **landing** under a **grant** a human issued outside the agent's
   channel, executed with a pure per-entry verdict, one holder per target, per-file
   compare-and-swap against outsiders, and an audit trail.

Both replace the earlier draft, in which volumes had no host-path base and the archive export was
the only egress.

## 1. hecate's rules, verbatim (read 2026-09-04)

- ADR-0005 (`docs/adr/0005-canonical-rebase-merge.md`): "Each increment declares its base green
  version; the per-session merge serializer position-maps its operations one-directionally
  through the canonical deltas between base and head, then runs a **pure, deterministic conflict
  verdict**: disjoint ⇒ splice into green; identical-both-sides ⇒ accept via content identity;
  overlapping or ambiguous ⇒ **reject into a corrective claim** carrying the exact conflict
  window." Why it rejects: "Auto-merge is safe when an attentive human sees the merged result
  instantly; Hecate's authors are agents that testified and moved on — a silent interleave
  produces code nobody wrote or reviewed". Amendment 2026-08-16: "The one law: **no automatic
  resolution of concurrent code edits, anywhere**".
- `docs/specs/MERGE.md:179-198` (the verdict, two pure passes): classes "**Accept** (all mapped
  ranges disjoint from intervening effect ranges) · **AcceptIdentical** (same-range concurrent
  inserts with identical bytes — recognized, not resolved) · **Conflict** (overlap/containment;
  edit anchored in a concurrent delete; same-position differing inserts; rename/rename;
  create/create differing)"; "No diff inference exists anywhere — the diff3 pathology family
  stays structurally unreachable"; "Common path: zero content reads. Rare path: one
  range-compare"; "Leases fast-path, never guard."
- `docs/specs/SESSIONS.md:17-25`: "The target is a local tree (laptop binding) or a source-control
  ref (fleet binding) behind one port"; "**The materialization lease is lineage-scoped and
  single-holder**: exactly one session at a time may materialize to a given target; the lease
  carries a fencing generation; supersession (a winner landing) bumps it and kills stale holders
  at the chokepoint."
- `docs/specs/SESSIONS.md:65-70` (the physical contract): "Session working state = (pinned
  baseline manifest, witnessed delta overlay, green chain). The baseline never moves under a
  session; the overlay contains exactly the materialized-iff-diverged entries (EdenFS's
  contract)"; "**No reconcile operation exists**: every write is witnessed at the serving
  boundary and lands in the overlay transactionally".
- `docs/specs/SESSIONS.md:89-125` (the landing engine): "1. **Manifest prune** — three-way against
  the shared baseline; identical hashes short-circuit; single-side files land by reference; only
  **doubly-touched** files proceed. Receipt: conflict probability tracks simultaneously-changed
  files (ρ≈0.6), not divergence duration"; "2. **Per-file event-graph replay** (eg-walker ...)
  **emitting conflict values on intersection, never interleaving** ... Detector, never resolver";
  "3. **Conflicts as first-class algebra values** ... **a resolution is itself a change**";
  "structural merge (mergiraf-class) runs as an optional, version-pinned, provenance-recorded
  pass over conflict values — its output must pass tests before a conflict counts resolved
  (receipts: 84.1% resolution, 28 false conflicts, **403 silently-missed real conflicts**). LLM
  resolution is an advisory proposer only; the chokepoint validates (receipt: an LLM judge
  accepted 4/5 structurally broken merges). Neither is ever a silent resolver."
- `docs/specs/SESSIONS.md:128-133` (review gates): "**materialization to a real target defaults to
  prompt, always, and requires zero unresolved conflict values**"; the review surface is "evidence
  to rule on, not a yes/no prompt". Test matrix rows `SES5` ("materialization blocked absent user
  validation + zero unresolved conflicts") and `SES12` ("single materialization holder per target;
  supersession bumps generation; stale holders die at the chokepoint — disk split-brain")
  (`SESSIONS.md:222, 229`).
- `docs/specs/VFS.md:100-108`: "work volume = RWO (one pod, one node, witnessed journal — the
  mutable case, never shared)"; "RWX does not exist in the system; nothing mutable is ever shared."
- `LEDGER.md:230-232` via `survey-hecate.md:407-409`: "**Leases guide, never guard.** ... Lease
  staleness informs; the conflict authority is the merge."

What differs in slates: hecate's baseline is a manifest it owns and its pods run in microVMs, so
the baseline can be pinned by construction. slates' base is the user's live disk, which editors,
git and other agents change at will. The pin therefore becomes a per-entry **witness** (fingerprint
plus hash recorded at copy-up), and "the baseline never moves" becomes "movement is detected and
reported, never absorbed".

## 2. sylk's flusher: the shape to avoid (from `survey-sylk-vfs.md`)

- Whole-overlay flush (`disk_flusher.go:62-111`, survey §1.7 step 7): stages every persistent
  modification as a temp file next to the target (`os.CreateTemp` + write + `Sync`), renames into
  place, rolls back by rewriting old contents, appends a checkpoint with full old and new
  contents, then `ResetOverlay` (survey lines 84, 122-123). Faults: the flush writes the union of
  all green modifications, so an accepted merge behind a rejected one is still written when the
  rejected head is superseded (survey §4, line 214; AVOID 8 at line 476); rollback rewrites old
  bytes (a second unguarded write); no validation that the disk still holds what the overlay was
  based on (PLATFORM.md via survey-hecate.md:410: "*No conflict detection at merge.*").
- The gate: `AllowDiskExport` is "the only switch that makes a session truly write-free towards
  the project tree" (survey line 123); the confirmation gate lived in the commit resolver
  (`flushConfirmed`, line 122) and offered Approve / Always flush / Hold (CONFIRMATIONS.md in the
  docs corpus). slates keeps the human gate and makes it a database record bound to a manifest.
- Base reads: `PipelineVFS` fell through to `os.ReadFile`/`os.Stat`/`os.ReadDir` whenever
  `baseFS == nil` (line 80), and `memorySnapshotFS` walked and read every file of the root
  (line 89). slates serves untouched entries lazily and never walks.
- The leased write basis (`prepare_write` returning `WorkspaceWriteBasis` with `disk/global/
  pipeline layer states` and typed staleness reasons, line 276) is the ancestor of the witnessed
  base and of the typed `BaseDrift` report.

## 3. Precedents for the overlay model

- **EdenFS** (`edenfs-scale-distribution.md:22-26, 46`): "An inode is not materialized if we have a
  source control object ID that can be used to fetch the inode contents"; materialization
  propagates to the root; on Windows "materialized" means "disk ... is the source of truth"; a
  fresh checkout is entirely non-materialized; ProjFS writes happen first and notifications
  arrive later with no ordering guarantee, so EdenFS "reconciles by inspecting disk" (fsck at
  startup). slates adopts lazy loading, materialized-iff-diverged, and disk inspection as the
  truth. [C: eden/fs/docs/Inodes.md, Overlay.h, Windows.md, WindowsFsck.md]
- **overlayfs** [B: kernel `Documentation/filesystems/overlayfs.rst`, fetched 2026-09-04]:
  "A whiteout is created as a character device with 0/0 device number or as a zero-size regular
  file with the xattr 'trusted.overlay.whiteout'"; when merging, "any matching name in the lower
  level is ignored, and the whiteout itself is also hidden"; opaque directories hide the lower
  directory; copy-up happens "when a file in the lower filesystem is accessed in a way that
  requires write-access, such as opening for write access, changing some metadata"; renaming a
  merged or lower directory needs `redirect_dir`, which records "the path of the original
  location from the root of the overlay", and without it "rename(2) on a lower or merged
  directory will fail with EXDEV"; and the rule that decides slates' posture: "Offline changes to
  the lower tree are only allowed if the 'metacopy', 'index', 'xino' and 'redirect_dir' features
  have not been used. If the lower tree is modified and any of these features has been used, the
  behavior of the overlay is undefined." slates' base is modified online by design, so slates
  detects (fingerprints) instead of assuming.
- **git's index and the racy-clean rule** [B: `git-scm.com/docs/racy-git`, fetched 2026-09-04]:
  git compares "the file type ... and executable bits ... from `st_mode` member, `st_mtime` and
  `st_ctime` timestamps, `st_uid`, `st_gid`, `st_ino`, and `st_size`" (`st_dev` and nanoseconds
  behind compile options); a "racily clean" entry is one modified in place without changing size
  within the timestamp granularity; git defends by comparing contents "when the `st_mtime` is
  the same as (or newer than) the timestamp of the index file itself" and by smudging `st_size`
  to zero. slates' witness uses the same fields plus nanoseconds where the filesystem has them,
  and re-hashes racy entries.
- **CitC** [A: Potvin & Levenberg, CACM 2016]: workspaces average "fewer than 10 files" of private
  state; the delta is small relative to the tree, which is why landing cost must follow the
  delta and never the tree.
- **Optimistic concurrency control** [A: H. T. Kung and J. T. Robinson, "On Optimistic Methods for
  Concurrency Control", ACM TODS 6(2), 1981]: a read phase without locks, a validation phase, and
  a write phase; slates' read phase is the agent's work in RAM, validation is the landing
  verdict against the disk as it is now, and the write phase is the granted landing. Validation
  of write sets is load-bearing; read-set staleness is advisory, matching hecate's receipt that
  conflict probability tracks files changed on both sides.
- **Why no inferred merge** [A: S. Khanna, K. Kunal, B. C. Pierce, "A Formal Investigation of
  diff3", FSTTCS 2007]: diff3 is not always stable and can produce results that neither side
  wrote; hecate's ADR-0005 cites the same family. [A: J. Gentle and M. Kleppmann, "Collaborative
  Text Editing with Eg-walker", EuroSys 2025 (arXiv 2024)]: hecate's landing detector; slates does
  not need it because the verdict is per entry and conflicts are surfaced whole, not positioned.
- **Conflicts as values** [C: jj (Jujutsu) conflict term lists; C: Pijul's "a resolution is a
  change"]: the shape hecate adopted; slates records conflicts in the landing manifest and the
  audit log, and a resolution is an ordinary write in RAM followed by `rewitness`.

## 4. OS primitives per platform, with what was verified

| Need | Linux | macOS | Windows |
|---|---|---|---|
| Bulk directory listing with attributes | `getdents64` then batched `statx` (verify: no bulk stat call exists) | `getattrlistbulk`: "iterates over the items in a directory and returns information about each directory entry"; `ATTR_CMN_NAME` and `ATTR_CMN_RETURNED_ATTRS` mandatory; "some file systems may return entries in lexicographic sort order and others may not"; mixing with `readdir` on one descriptor is undefined [B: macOS getattrlistbulk(2), fetched] | `NtQueryDirectoryFile` / `GetFileInformationByHandleEx(FileIdBothDirectoryInfo)` returning ids, sizes and times per entry (verify) |
| Temporary file with no visible name | `O_TMPFILE` since 3.11: "an unnamed inode will be created in that directory's filesystem ... lost when the last file descriptor is closed, unless the file is given a name"; link with `linkat(fd, "", AT_FDCWD, path, AT_EMPTY_PATH)` or via `/proc/self/fd/N` with `AT_SYMLINK_FOLLOW`; supported by ext2/3/4, tmpfs, XFS (3.15), Btrfs and F2FS (3.16), ubifs (4.9) [B: open(2), fetched] | none; a hidden sibling name carrying the landing id | none; a hidden sibling name carrying the landing id |
| Atomic swap of two names | `renameat2` with `RENAME_EXCHANGE` (since 3.15): "Atomically exchange oldpath and newpath. Both pathnames must exist but may be of different types"; `EINVAL` when "The filesystem does not support one of the flags" [B: rename(2), fetched] | `renamex_np` / `renameatx_np` with `RENAME_SWAP`: "the source and target [are] atomically swapped. Source and target need not be of the same type"; support advertised by `VOL_CAP_INT_RENAME_SWAP`; `ENOTSUP` otherwise [B: macOS rename(2), fetched] | no exchange; `FileRenameInformationEx` with `FILE_RENAME_REPLACE_IF_EXISTS \| FILE_RENAME_POSIX_SEMANTICS`: "allow replacing a file even if there are existing handles to it. Existing handles to the replaced file continue to be valid for operations such as read and write. Any subsequent opens of the target name will open the renamed file" (guarded by `_WIN32_WINNT_WIN10_RS1`, Windows 10 1607) [B: Microsoft ntifs `FILE_RENAME_INFORMATION`, fetched]; the source of a rename "cannot be renamed if it has any open handles" other than the renaming handle, so the temporary is opened once |
| Verify the displaced file after the swap | `fstat` on the descriptor we held open on the old file before the exchange | same | our handle on the old file, opened without write sharing, stays valid across the POSIX-semantics replace; a sharing violation from another process's handle is reported as `TargetInUse` |
| Reflink instead of copy | `FICLONE` / `FICLONERANGE` since 4.5 (formerly `BTRFS_IOC_CLONE`); `EOPNOTSUPP`, `EXDEV`, `EINVAL` for unaligned ranges [B: ioctl_ficlone(2), fetched] | `clonefile` / `clonefileat` / `fclonefileat`: "The cloned file dst shares its data blocks with the src file but has its own copy of attributes"; support via `VOL_CAP_INT_CLONE`; `ENOTSUP`, `EXDEV` [B: macOS clonefile(2), fetched] | `FSCTL_DUPLICATE_EXTENTS_TO_FILE` on ReFS ("Minimum supported client: None supported"; Windows Server 2016) [B: Microsoft Learn, fetched]; optional, probed |
| Durability of a written file | `fdatasync` per file then `fsync` on each touched directory descriptor (verify against fsync(2): directory fsync needed for the entry) | `fcntl(F_BARRIERFSYNC)` per file ("issues a barrier command to the drive ... operations flushed before the barrier are guaranteed to be persisted before any other I/O that would follow the barrier") then one `fcntl(F_FULLFSYNC)` at the end ("asks the drive to flush all buffered data to the permanent storage device ... drains the entire queue of the device") when the grant asks for media durability [B: macOS fcntl(2), fetched] | `FlushFileBuffers` per file and on the directory handle (verify) |
| Never write outside the target | `openat2` with `RESOLVE_BENEATH` (since 5.6): "Do not permit the path resolution to succeed if any component of the resolution is not a descendant of the directory indicated by dirfd"; `EXDEV` on escape; `RESOLVE_NO_SYMLINKS` gives `ELOOP` on any symlink [B: openat2(2), fetched] | per-component `openat` with `O_NOFOLLOW` and `O_DIRECTORY` from the target descriptor (no `openat2`) | per-component opens with `FILE_FLAG_OPEN_REPARSE_POINT` and a reparse-tag check; `NtCreateFile` relative to the target handle (verify) |
| Change hints (never the truth) | inotify: "not recursive"; identical events "are coalesced"; "the event queue can overflow. In this case, events are lost ... it may be necessary to rebuild part or all of the application cache"; `IN_Q_OVERFLOW` always generated when events exceed `max_queued_events` [B: inotify(7), fetched]; fanotify for a whole mount (verify) | FSEvents: `kFSEventStreamEventFlagMustScanSubDirs` means "Your application must rescan not just the directory given in the event, but all its children, recursively"; `kFSEventStreamEventFlagUserDropped` and `KernelDropped` say where the overflow happened [B: Apple FSEvents flags, fetched] | `ReadDirectoryChangesW` on an overlapped directory handle bound to a completion port; on overflow "the entire contents of the buffer are discarded and the lpBytesReturned parameter will be zero" or `ERROR_NOTIFY_ENUM_DIR`, and "you should compute the changes by enumerating the directory or subtree"; 64 KB buffer limit over the network [B: Microsoft Learn, fetched] |
| Reads of untouched base files without a daemon copy | FUSE passthrough: negotiate `FUSE_PASSTHROUGH` with `max_stack_depth`, register a backing descriptor with `FUSE_DEV_IOC_BACKING_OPEN`, reply to `OPEN` with `FOPEN_PASSTHROUGH` and the `backing_id`; passes through `read`, `write`, `splice`, `mmap`; "currently requires the FUSE daemon to possess the `CAP_SYS_ADMIN` capability" [B: kernel fuse-passthrough.rst, fetched; also `os-filesystem-bridge.md:35`]. slates does not use it (decided 2026-09-04): it requires a capability the user may lack, and slates never depends on such a privilege; the daemon reads the backing file into arena pages (one copy) | the FSKit module or NFS server reads the backing file in the daemon (one copy) | the WinFsp binding reads the backing file in the daemon (one copy) |
| Zero-copy write from arena pages | `IORING_OP_WRITEV` with registered buffers (the chunk regions are already registered) | `pwritev` from a pool thread (no asynchronous file I/O on macOS) | `WriteFile` overlapped on a completion port with the arena page pointer |
| Timestamps for incremental builds | `futimens` on the temporary before the swap | `futimens` | `SetFileInformationByHandle(FileBasicInfo)` |
| Sparse ranges | `SEEK_DATA`/`SEEK_HOLE` on read; write only data extents | same | `FSCTL_SET_SPARSE` then write data extents (optional, probed) |
| Preallocation | `fallocate(FALLOC_FL_KEEP_SIZE)` | `fcntl(F_PREALLOCATE)` | `FileAllocationInfo` |

Filesystem timestamp granularity (for the racy-clean rule): ext4, XFS, Btrfs, tmpfs, APFS and
NTFS carry nanosecond or 100 ns fields; HFS+ has one-second granularity; FAT has two seconds
(verify per filesystem; the daemon reads the type from `statfs`/`getattrlist`/`GetVolumeInformation`
and takes the granularity from a cited table, never from a guess).

## 5. Decisions this file supports

- D-25 (overlay volumes with witnessed bases): §1 (hecate physical contract), §3 (EdenFS,
  overlayfs, git, CitC), §4 (listing, hints, passthrough rows).
- D-26 (landing under grant): §1 (verdict, landing engine, review gate, materialization lease),
  §2 (sylk faults), §3 (OCC, diff3, conflicts as values), §4 (temporary file, swap, verify,
  reflink, durability, containment rows).
- The concurrency theory in plain terms: inside a volume, one writer (the owner shard);
  between a volume and the disk, optimistic validation at landing with per-entry witnesses;
  between volumes over one base, independence until landing; between a landing and outsiders,
  a single-holder lease for slates' own sessions and per-file compare-and-swap for everyone else.

## 6. What must be measured (never assumed)

Per OS: stat cost per entry; listing cost per directory size; watcher latency and overflow rate;
copy-up cost per size class; exchange support per target filesystem (probe with two temporary
names inside the target during a granted landing, or by capability query on macOS); reflink
availability and gain; per-file swap and verify cost; data-sync and directory-sync cost; the
`syncfs` versus per-directory break-even on Linux; `F_BARRIERFSYNC` versus `F_FULLFSYNC` cost
on macOS; the in-flight depth at which landing throughput stops rising (the online ramp); the
stage-and-exchange break-even; crash-resume cost. All of it inside granted landings, because a
disk probe at boot would itself be a disk write outside a grant.

## 7. Risks and items to verify

- Not fetched this session (from memory): batched `statx` shape on Linux; `NtQueryDirectoryFile`
  information classes; `FlushFileBuffers` on directory handles; fanotify mount-wide marks;
  Windows reparse-tag checks for containment; per-filesystem timestamp granularity table;
  `FSCTL_SET_SPARSE`; `F_PREALLOCATE`. Each is marked "verify" above and in `GAPS.md`.
- FUSE passthrough needs `CAP_SYS_ADMIN`; slates never requires it, so the base read path is a
  daemon copy everywhere (decided 2026-09-04).
- `RENAME_EXCHANGE` support varies by filesystem; where absent, the fallback is verify-then-
  rename-over, which has a small unverified window that the outcome record reports. Tripwire in
  `GAPS.md`.
- Watchers overflow under `git checkout` bursts; fingerprints remain the truth, and a full
  re-validation of witnessed entries is scheduled after any overflow.
- Very large directories under a base (`node_modules`, `target/deps`) make the union listing the
  cost to watch; the listing cache is keyed by the directory's change time. Tripwire in `GAPS.md`.
