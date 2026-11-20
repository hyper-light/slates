# Where version-control systems keep changes, history and metadata: prior art for A-767

Status: 2026-11-20, w-store, for A-767 (Ada's storage ruling ~12:40 PST and changeset ruling ~13:35 PST). Evidence
tiers: A paper, B vendor or standard documentation, C deployed code or its design docs, D blog (flagged). Items marked
"unverified" were recalled, not read from a source this session.

## 1. Anthropic's Scion (B: Anthropic engineering, "Redesigning source control",
https://www.anthropic.com/engineering/redesigning-source-control, read 2026-11-20; A-645 cites it under /research/)
- **The unit is the changeset**, "an entry in a write-ahead log": the files changed relative to its first parent, with
  content-addressed pointers to compressed file contents in blob storage.
- **Nothing is materialized:** "There is no fully materialized copy of the repo anywhere, even on the server", and there
  are no packfiles. Servers are "essentially large caches that can also compute derived data on the fly and write the
  results back to Spanner". Blame, path history and the commit graph's skip lists are derived data, each defined from
  its parents' values, eager or deferred.
- **Refs are separate and cheap:** moving one "is a compare-and-swap on a single Spanner row that happens separately
  from creating changesets".
- **Tiers:** a client's bounded local object cache; a node daemon starting from a refreshed snapshot; regional caches
  streaming objects "directly from memory, in the disk format that the daemon will consume"; Spanner and cloud storage
  behind them. Every tier's hit rate is above 99%.
- **Numbers:** merges went from about 2,500 a day (May 2026) to over 30,000 (September); a history of about 100 GB; a
  compressed working copy over 25 GB; about 25 million checkouts a day; CPU and memory 200x below the git-mirror fleet
  it replaced. Its footnote credits Mononoke's Bonsai model.
- **slates takes:** the changeset as the one unit and the log as the history (Ada's ruling); everything else derived and
  cached by identity; refs as separate rows moved by compare-and-swap (A-767's `N` rows). **Leaves:** Spanner, a global
  service (CLAUDE.md §2 item 12: ids route to owners); slates' changesets replicate at f+1 and each node's disk follows.
  **Differs:** Scion's changeset is a tool-neutral file diff; slates keeps the bytes the native client sent (git's pack,
  Mercurial's changegroup, Subversion's svndiff) so landing converts nothing, beside the work's tool-neutral path changes
  the verdict already computes.

## 2. Meta's EdenFS (C: facebook/sapling `eden/fs/docs/InodeStorage.md`, `Caching.md`)
- **The overlay is on disk:** "an on-disk map from inode number to its timestamps plus the file's or tree's contents";
  unchanged files hold only a source-control id and load lazily. A child's overlay data is written before its parent
  marks it materialized, so a crash between them is safe. Not power-loss consistent: commit cloud covers that.
- **Caches:** an in-memory blob LRU (40 MiB default, `Caching.md`, dated 2018), a blob-id to (size, SHA-1) LRU of 1M
  entries "under 100 MB"; "Eden has no on-disk object store of its own" (the Sapling hgcache serves).
- **slates takes:** the write order (content before what names it, A-767 item 4); caches of fixed meaning sized by
  measurement. **Leaves:** the on-disk overlay: a volume's private delta stays in RAM (R1, A-50), as Ada ruled again for
  changes. (Further EdenFS evidence: `edenfs-scale-distribution.md` §2.1.)

## 3. Meta's Mononoke and Sapling (C: `eden/mononoke/docs/2.1-bonsai-data-model.md`, `2.3-derived-data.md`,
`2.4-storage-architecture.md`, `4.1-pushrebase.md`)
- **Bonsai changesets:** a flat list of path-to-change entries with explicit copy-from, Thrift-serialized, hashed with
  Blake2b: the canonical unit; git and Mercurial forms are derived and mapped.
- **Stores split by mutability:** an immutable content-addressed blobstore (changesets, file contents, derived data); a
  SQL database (MySQL, SQLite in open source) for bookmarks, the bookmark update log, hash mappings, and the commit
  graph's index with generation numbers and skip pointers. Blobstore writes go to several backends and succeed on a
  quorum; a write-ahead log in SQL lets a healer repair the copies that failed.
- **Landing:** validate, convert to Bonsai, store, update mappings, add to the commit graph, run hooks, move the
  bookmark; derived data is off the critical path. A warm-bookmark cache delays a bookmark's visibility until its
  derived data exists ("a few seconds"). Pushrebase rebases non-conflicting pushes on the server.
- **slates takes:** one canonical changeset, derived data off the landing's path, a K/V store for the mutable and
  mapping rows (A-767 item 3: `N`, `C`, `O`, `D`), the commit graph's fields as derived data (A-666). **Leaves:** a
  quorum blobstore plus healer: slates' f+1 holders each append the same changeset.

## 4. Google's Piper and CitC (A: Potvin and Levenberg, "Why Google stores billions of lines of code in a single
repository", CACM 2016; the page refused this session's fetch, so the points below are from its summary at
https://en.wikipedia.org/wiki/Piper_(source_control_system), tier D until read)
- Piper's storage was Bigtable, later Spanner, across 10 data centres, replicated by Paxos. CitC: a cloud backend and a
  local FUSE filesystem; only modified files are stored; a local copy averages under ten files; every write is a
  snapshot. 2016: 86 TB, 9M files, 25,000 developers, 16,000 human and 24,000 automated changes a day.
- **slates takes:** the closest published match to "changes in RAM, repositories on disk": a client's state is only its
  change, overlaid on durable history. **Leaves:** a global service.

## 5. Microsoft's VFS for Git and Scalar; git's on-disk indexes (B: microsoft/VFSForGit `Readme.md`,
`Protocol.md`; git-scm.com `commit-graph`, `multi-pack-index`, `bitmap-format`, `scalar`)
- VFS for Git "only downloads objects as they are needed" (`GET /gvfs/objects/{id}`, `/gvfs/prefetch`); Scalar replaced
  it with partial clone, sparse checkout and background maintenance.
- git's history stays on disk in packs; RAM is bounded by indexes read in place: the commit graph (generation numbers:
  "if N <= M, then A cannot reach B"), the multi-pack index ("O(log N) lookup time for any number of packfiles"),
  EWAH reachability bitmaps. A received pack is kept as it arrived (receive-pack's quarantine, then migration).
- **slates takes:** the received pack kept unconverted and read in place (w-git's B1); fetches naming immutable objects;
  indexes into history rather than history in RAM. **Leaves:** per-repository pack files and repacking as truth: the
  changeset log is truth, packs are derived.

## 6. Others
- **GitHub Spokes** (D: github.blog, "Introducing DGit", "Building resilience in Spokes"): three replicas on local
  SSDs, replicated at the application layer; it "will refuse to accept writes that it cannot commit, synchronously, to
  at least two replicas". The nearest match to A-767 item 5: replicate the change, each disk its own.
- **GitLab Gitaly and Praefect** (B: docs.gitlab.com/administration/gitaly): local storage only; Praefect broadcasts
  writes and tracks replica state in a database.
- **Perforce shelves** (B: help.perforce.com, shelve-changelists): a pending change kept on the server, outside the
  depot, submitted later: the change in flight as a server object.
- **Jujutsu's operation log** (C: jj-vcs/jj `docs/technical/concurrency.md`, `operation-log.md`): each operation names a
  view and its parents, and is written without locks; divergent operations merge later. The model for the `C` rows'
  states being recoverable.

## 7. What A-767 takes, in one line each
1. The changeset is the one unit and the log is the history (Scion; Bonsai), stored as the native client sent it.
2. Mutable rows (names, the changeset index, mappings, derived-data locations) in a K/V store, separate from immutable
   content (Mononoke; Scion's Spanner rows), routed to owners, never global.
3. Derived data off the landing's path, computed anywhere, cached by identity (Scion; Mononoke).
4. Content before what names it (EdenFS's overlay order; git's quarantine).
5. Replicate the change; each disk follows (Spokes, Gitaly).
6. RAM is the change in flight plus caches with bounds (EdenFS, Scion's tiers): here learned, not fixed.
