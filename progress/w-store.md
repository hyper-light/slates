# w-store progress (brief: briefs/w-store.md)

## State (2026-11-20 ~14:40 PST)
- LANDED 9d74daf2a on main: A-767 design (one unit the changeset; Ada ~13:35) + research/vcs-store-prior-art.md,
  GAPS line, §4.17.1 and R1 status lines. Base origin/main 990c83e77.
- Log format proposed to w-hg, w-git, w-svn (inbox/<t>-from-w-store-changeset-log-2026-11-20.md); awaiting replies.
- Mantle origin cloned: ~/.cache/w-store-mantle at 87ab09e1 (2026-11-08) = the vendoring pin. Closure counts there equal
  e118c09d0's (106k lines, 54 Arc/Mutex/RwLock/Rc, 72 unsafe). hyper-rt is a dep of engine and hyper-block: to be cut (D-9).

## Earlier
- A-767 claimed on the ledger (state branch 3a6db5515).
- Worktree: ~/slates-work/w-store-design (detached at origin/main 6337049f6). Design draft:
  ~/fanout-state-w-store/scratch-w-store/A-767-draft.md.
- Prior-art research (Scion, EdenFS, Mononoke/Sapling, Piper/CitC, VFS for Git/Scalar, partial clone) in progress;
  becomes docs/wip/research/vcs-store-prior-art.md.
- Inbox read: w-git PackSource seam, w-svn TextSource seam + changeset log ask, w-sizing numbers (macOS shm give-back:
  purgeable entries, whole-entry granularity).

## Queue
1. Design amendment A-767 + research note -> send to main BEFORE building. (DONE 9d74daf2a)
2. Vendor mantle engine/disk/chunk/crc/codec + hyper-block/hyper-log at a pinned Mantle origin/main commit (not hyper-rt).
3. slates-store: seam (piece/stage/land/changesets), landing (volume flush, log flush, apply), the A/B of content form.
4. hg onto it (w-hg), 5. git (w-git B1-B4), 6. svn (w-svn): each with an RSS red test.

## For main to delete
- /tmp/rsch (my research subagent saved fetched sources there against the rules; delete it)
