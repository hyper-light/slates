# w-svn -> main: A-760 (b) 4c+4d, the plan before code (2026-11-23)

One piece, as you ruled: a named block holds no bytes and one hold of the content store, history reads go through the
plane, sync readers refuse a let-go window typed, their async callers bring it back, never pinned, and reanchor checks
by identity.

## Engine (crates/merge)
1. **A byteless block.** A file made whole of one named run, its object whole, becomes a block of no bytes
   (`Blocks`' slot marks it). Its digest is the identity's hash (A-873), and the run's read buffer is dropped, not
   moved. A named file restored from a checkpoint is placed the same way, with nothing read.
   - So `Checkpoint::read_step` and the recovery's `Restoring` phase have no work left for named files. They go
     (replaced, not layered). So does the remake's keeping of bytes for the checkpoint (cf1529f1d).
   - A chain entry's whole-object run needs no read either: `apply_recorded` names the block. Only a partial run or a
     splice still reads.
2. **Holds, drained.** The engine records each byteless block it makes (`Take(identity)`) and frees (`Give(identity)`),
   whatever freed it: a replace, a fold, a retire's release step, a removal.
   - `Green::take_name_holds()` hands the record over, and the server applies it to the content store.
   - Holders reset at a restart (`restore_nodes`: "held by no one until a holder puts or holds its object again"), so a
     replay re-taking its holds counts once.
3. **Readers.** `Content::Named` / `ContentAt::Named` answer for a byteless block, its history values (a superseded
   block in `Source::Block`) and its pages.
4. **The engine's own reads of a byteless block:**
   - (a) `history::reanchor` checks a named whole replace by the two identities (your ruling), never reading bytes.
   - (b) A copy's fix, a branch's revert and a hard link's hand-over of a named value carry the name: a new byteless
     block, one more Take.
   - (c) A splice into a named file, or `lands_on_head` over one, needs the base's bytes. svn never splices (a text
     replaces its node whole), and git and hg greens hold none. A work over an svn green that edits a named file would.
     **Ask 1:** refuse it typed (`ComputeRefused::BaseNamed`, counted), or read the base through an `Objects` given to
     judge/compute, as `read_step` reads runs?
   - (d) The from-scratch identity oracle takes a byteless block's digest.

## Server
5. **Draining.** The holds are drained where a green changes, before anything else lets the text go: the svn landing's
   apply and its recorded batch's slices, every replay step (recovery, remake, re-adoption), `fold_to_readers`, the
   retire's release steps. One helper per site: `vcs_store::named_objects::settle_holds(state, green)`.
   - Order matters at a landing. The green's Take must land before `release_texts` gives the commit's own hold back,
     or the object is freed between them.
6. **A text the store lacks at a restart** (its image held none) still has to come back before the replay can take
   its hold. The pre-pass (`to_bring`, the planner from 9a868c263) narrows to texts absent from the store, and a
   let-go window is left for its reader to bring back.
7. **Sync readers** (`green_tree::read_at` for a mount, `merge_service::read_green_range`, a work's base in
   `work_files`) read a named file through `PlaneObjects`.
   - A window that was let go is refused typed with its name (Away), counted. The async caller (the NFS or FUSE
     request task, the client's read, the work's build) brings it back, as `vcs_store::read_through` does for git, and
     takes the read again. It gives up typed if the same window is met again with nothing between. The bring-backs
     are counted too.
   - **Ask 2:** I'll map the mount's async boundary (where an NFS/FUSE read's task can await). If the volume read
     under it is sync all the way down to `green_tree`, the Away has to surface through `HostError`'s try-again to the
     bridge's task. I'd confirm that path before building.

## Red first
- The A-866/A-869 oracle with every named block reporting zero bytes held (a counter of the bytes byteless blocks
  hold, which must stay 0) and the held texts taken and given back in step.
- A server test reading an svn file over a mount and over a work after the text left the green, each through the
  refuse-and-bring-back, once with its window let go.

## Payoff, yours to measure
The head's texts are held once in all (w-perf's 56 MB of `apply <- make_mapped` at hg's push end is git and hg's, not
this; svn's is the import's green heap).
