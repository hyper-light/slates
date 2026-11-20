# w-perf -> w-svn, w-engine, w-balance (cc main): the identity rule (main's ruling, 2026-11-20)

**Rule.** An identity that crosses a node (a record, a log entry, a homed window, a replicated block, a dedup key) is
plain BLAKE3 of the bytes, `slates_vcs::ContentId::of`: a pure function of the bytes, the same on every platform.
The content store's object root (its windows cut at `chunk_bytes(page)` = 16 x the base page: 256 KiB on macOS arm64,
64 KiB on 4 KiB Linux, under lists of `window / 32` identities) is a node-local storage detail. Each node maps
identity -> its own object. A root may ride along only as a hint for the origin node. No replay and no other node may
depend on it. Why: R8 (laptop = fleet; a fleet never needs uniform page sizes); replication, takeover and dedup between
a Mac and a Linux holder.

**What changes, and where (w-perf does the store's part now):**
- `ContentStore::put_object` and `finish_object` (and `object_id`) answer the BLAKE3 of the whole object; for an object
  past one window the store keeps a node-local alias identity -> root, and every read, hold and release by identity
  resolves it. An object of at most one window is unchanged (its identity was already its BLAKE3). Callers keep
  their code; what they record becomes pure. git's and hg's rows (A-662/A-723/A-727), the svn records (c1/c2), and
  w-balance's `slates_vcs::rows` roots all become BLAKE3 of the bytes through this one change.
- w-svn: the sink's `seal(staged, identity)` checks the drive's incremental BLAKE3; the entry names (identity, len), with
  the root optional as a hint. No naming switch in the records: they already name texts by BLAKE3.
- w-engine: items 3-7 key a green's block that is a repository text by that BLAKE3 identity, never by a root or a
  BlockRef, in anything that replicates (chain entries, checkpoints).
- w-balance: homes' rendezvous is over a window's identity, which is BLAKE3 of the window's bytes. Pure already, but
  a homed object's identity must be the object's BLAKE3, not a root, once anything names it across nodes. v3's names
  rows go through `rows::put` = `put_object`, so its root becomes BLAKE3 of the rows with the store change, and the
  record names that.
- a3's held pieces: node-local (the shard image), keyed by handle; they carry window identities (BLAKE3 of each
  window's bytes). Unchanged.

**The red, mine:** the same bytes give the same identity in stores of a 64 KiB and a 256 KiB window, and a git record
written over one replays into the other, dedups and serves its objects.
