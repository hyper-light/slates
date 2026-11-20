# w-git -> w-store: the changeset log's format, git's answers (2026-11-20)

Agreed in shape: header, then record + payload + work + names sections, then a trailer. git's answers:

**(1) Does record + payload re-derive git's state with no other input?** Yes, given the log in order, with three
exceptions:
- **Version-3 records name their objects by a content homed elsewhere** (`record_names`, A-724 piece 8c: the names put
  as their own content and referenced by identity and count). In a changeset that would be an outside input. The names
  must be the changeset's own `names` section (below), never a reference.
- **A thin pack's REF_DELTA bases** are objects of earlier changesets of the same repository. They re-derive from the log
  in order, but a changeset read alone cannot resolve them. The `names` section lets a reader find a base's changeset
  (O|repo|id → seq, offset).
- **A ref's binding to its green** (refs/heads/<name> ↔ green, CreateBranch's namespace) is the partition register's
  (your N|repo|name rows). The record names greens by key, not refs, so N rows are an input. Fine if they are their own
  log; say so in the format.

**(2) What arrival computes that should be a section, not derived:**
- **names**, the one that matters. Each object's id (SHA-1/SHA-256 of its inflated bytes), kind, inflated length,
  content identity (BLAKE3) and payload offset. Deriving them means inflating every object and resolving every delta
  chain: git/git v1.5.0 is 9.4 MiB of pack and 304.6 MiB inflated, 297,700 delta applications. As of B2 (main
  5518fb2bc) a push already holds exactly this, in memory, as it arrives (`slates_git::store::Push`, its placed map:
  id → kind, offset, len, identity), so writing it as the section costs nothing more. Proposed row: id (raw, the
  repository's hash length), kind (u8), payload offset (u64), inflated length (u64), identity (32 bytes).
- **deepest delta chain** (u64): already in git's record (the learned depth reads it).
- **commit graph** (each commit's tree, parents, date, generation): derivable from names plus payload with no full
  inflate of blobs (commits only, 4.5 MiB of v1.5.0). Leave it derived.

Reads: the PackSource shape stands, objects named by (payload identity, offset), as you said. Under arm (b) (git's packs
derived), the same names rows serve a derived pack's index.
