# w-store progress (brief: briefs/w-store.md)

## State (2026-11-20 ~14:40 PST)
- LANDED 9d74daf2a on main: A-767 design (one unit the changeset; Ada ~13:35) + research/vcs-store-prior-art.md,
  GAPS line, §4.17.1 and R1 status lines. Base origin/main 990c83e77.
- Log format proposed to w-hg, w-git, w-svn (inbox/<t>-from-w-store-changeset-log-2026-11-20.md); awaiting replies.
- Mantle origin cloned: ~/.cache/w-store-mantle at 87ab09e1 (2026-11-08) = the vendoring pin. Closure counts there equal
  e118c09d0's (106k lines, 54 Arc/Mutex/RwLock/Rc, 72 unsafe). hyper-rt is a dep of engine and hyper-block: to be cut (D-9).

- main's review (~15:00): reconcile payload vs w-hg's tool-neutral changeset (~137 MiB vs 4,297 MiB whole); content once
  across pushes/forks/tools; A/B (a) raw payload + work index vs (b) normalized per-path edits/wholes by identity
  (+ hybrid). Agree with w-hg/w-git/w-svn, record in A-767, then build.
- A/B phase 1 script: scratch-w-store/ab/changeset_ab.py (disk bytes, dedup shared vs alone, chain depth, normalize
  CPU; runs under hg's python). Smoke on mantle ranges OK. Phase 2 (Rust: landing CPU/alloc/RSS, serving, old read).
- BLOCKED partly: kiln run from Studio-2 fails ("did not answer in 15s", every runner, 3 tries); ssh to Studio-3/4 host
  keys changed (not bypassed). hg arm + cross-tool need a Studio with hg (datasets ~/.cache/w-interop on the big
  Studio). Running the git half here: git.git cloned to ~/.cache/w-store-ab/.

- ~16:40 LANDED e7522325d: A/B phase 1 (BENCHMARKS "A changeset's stored form" + A-767 item 7 status). Ran on
  Adas-Mac-Studio-2 (kiln unreachable then): hg 57,053 revs, same history as git (repacked 85.2 MB), git v2.0.0/v2.1.0.
  Cross-tool +85.2 MB raw vs +0 normalized; forks +57.9 vs +0.49 MB; git pack recompute 6.3 s vs reuse 0.94 s.
  Proposal (normalized on disk, raw in flight, derived wire forms seeded by received pack) sent to w-hg/w-git/w-svn
  (inbox/<t>-from-w-store-stored-form-2026-11-20.md). Datasets: ~/.cache/w-store-ab (data-hg, data-git).
- NEXT: agreement replies; git normalization timing in Rust (asked w-git); then vendoring (piece 2).

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
- Big Studio: ~/.cache/w-store-ab/hg-as-git-1795210958.git, hg-as-git-1795211602.git (failed/partial conversions); ~/.cache/w-store-ab after A/B
- /tmp/rsch (my research subagent saved fetched sources there against the rules; delete it)
