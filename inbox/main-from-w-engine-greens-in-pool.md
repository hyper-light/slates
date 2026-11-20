# w-engine -> main: design note, a green's Files blocks in the content pool (A-64 for greens)? (2026-11-20 ~09:45 UTC)

Facts first (they change the question; read in code and measured on the 1,000-revision import, main 2103b1177):
1. A green has no arena-backed volume. Its files live only in the engine's heap (`dims::Blocks`, a `Vec<u8>` per
   block); reads go `merge_service::read_green_range` -> the engine (merge_service.rs:1171), creation inserts only the
   engine (verbs.rs:7577). At f = 0 a version's inputs are not placed anywhere (`enqueue_record_with` returns at the
   local hold). So there is no RAM copy of a green's files in the pool beside the engine's: my table's row "merge
   engine copies of the current files 241.5 MiB" was mislabeled. The tree is 48.2 MiB (4,872 files); the other ~193 MiB
   is history versions' blocks the engine keeps up to its per-green retention budget (`fold_history_to_budget`).
2. The anchor content object's 581 MiB dirty (one SHM mapping, vmmap) is therefore not the greens' files: candidates
   are the shards' write logs and image slots, and the pre-faulted free pages a shard keeps after a burst
   (`prefault_reserve_bytes`). Owed: a per-range dirty report from the daemon to split it.
The design, if wanted anyway (what A-64 gives greens):
3. Files' blocks become chunk references (region, offset, order, length) in the owner shard's `ChunkArena`; the engine
   names chunks, the bytes live in the anchor's content object. slates-merge stays pure: a `BlockStore` seam the
   server injects (the arena), and a heap implementation the engine's oracles run over (the SimHost pattern).
4. Contiguous reads: a block is one buddy block, so a file up to the arena's largest order reads as one slice; larger
   files are a run of blocks and `content(path)` would copy (or the VFS read path takes runs). Today's largest order
   and git/git's largest file decide whether that is ever met.
5. Lifetimes: lineage members share one shard's arena (same as Blocks now); retire, fold and the history's named
   blocks release chunks through the arena's existing give-back (`discard`), so a freed block returns to the OS.
6. Payoff: (a) memory freed by folds goes back to the OS, not to malloc's cache; (b) the green's bytes survive a daemon
   restart in the anchor object, so recovery can name chunks instead of replaying the chain's bytes, and a chain entry
   or checkpoint can name chunks instead of carrying file bytes: that is where a second copy really is (the durable
   log in the anchor object), and where content once pays.
7. Cost: the seam through the engine (every Files read and write), image capture naming the green's chunks, and
   checkpoint/chain formats naming chunks (new versions). About the size of A-728.
Recommendation: first split the 581 MiB (item 2 above). If it is mostly chain entries, checkpoints and repository
records carrying file bytes, do 3-7 with entries naming chunks; if it is pre-fault and logs, the greens' gain is (a)
alone and lower priority than item 1. Your call before any code.
