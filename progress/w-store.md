# w-store progress (brief: briefs/w-store.md)

## State (2026-11-20 ~13:50 PST)
- A-767 claimed on the ledger (state branch 3a6db5515).
- Worktree: ~/slates-work/w-store-design (detached at origin/main 6337049f6). Design draft:
  ~/fanout-state-w-store/scratch-w-store/A-767-draft.md.
- Prior-art research (Scion, EdenFS, Mononoke/Sapling, Piper/CitC, VFS for Git/Scalar, partial clone) in progress;
  becomes docs/wip/research/vcs-store-prior-art.md.
- Inbox read: w-git PackSource seam, w-svn TextSource seam + changeset log ask, w-sizing numbers (macOS shm give-back:
  purgeable entries, whole-entry granularity).

## Queue
1. Design amendment A-767 + research note -> send to main BEFORE building. (in progress)
2. Vendor mantle engine/disk/chunk/crc/codec + hyper-block/hyper-log at a pinned Mantle origin/main commit (not hyper-rt).
3. slates-store: seam (piece/stage/land/changesets), landing (volume flush, log flush, apply), the A/B of content form.
4. hg onto it (w-hg), 5. git (w-git B1-B4), 6. svn (w-svn): each with an RSS red test.

## For main to delete
(none)
