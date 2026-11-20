# w-work-mount progress (A-774)

Brief: briefs/w-work-mount.md. Checkout: ~/slates-work/w-work-mount (clone, --reference w-balance --dissociate).
Target dir: ~/.cache/w-work-mount-target.noindex (CARGO_INCREMENTAL=0).

## Design (2026-11-20 ~13:50)
What exists: a Work is a delta over its Green's version (crates/server/src/work_files.rs, since 2026-11-04), changed only by
`Edit`/`Declare`; `Submit` composes the increment; the Green has no VFS volume, so `land` refuses (landing.rs ~268) and a
Work cannot be attached. The VFS deriver (crates/vfs/src/derive.rs) already turns a volume's journal since a base
snapshot into an ops document (files with hunks, removals, dirs, symlinks): the declaration path §4.16 names.
Pieces:
1. Mounted work's journal -> its work's ops: derive(volume, base snapshot) -> Edit/Declare on the WorkState (pure
   translator + apply), so Submit is unchanged. Whole-file rewrites (temp + rename) become precise ops through
   w-rewrite-ops' minimiser when it lands; until then one hunk per file.
2. Attach a Work: a companion plain volume over the green's base version (w-work-cow's copy-on-write base when it lands),
   mounted by FUSE/NFS/WinFsp like any volume; `SubmitMounted` = piece 1 + Submit; verdict back.
3. A Green version's store-backed snapshot (companion volume advanced by each accepted version's changed paths,
   snapshotted) -> `land` plans and lands it under a grant (R10, CLI/confirmation surface only).
4. By use: ten editor processes on ten mounted Works over one Green; accepted / identical / conflict; grant lands;
   landed bytes == oracle. Kernel variant gated (root), run-request for main; in-process NFS-protocol variant always runs.

## Status
- 13:50 A-774 claimed. Writing the red end-to-end test first (main 13:45).
