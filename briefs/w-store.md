# w-store: changes in RAM, repositories on disk, metadata in Mantle's K/V engine (main, 2026-11-20 ~12:45 PST)

## Why
Ada asked why the daemon holds 7.1 GB resident for an 84 MB Mercurial repository, and ruled. Read the STORAGE section
of briefs/common.md for her exact words; it is your mandate. Summary:
- Changes (pushes, changesets, works) live in RAM.
- Full repository content lives durably on disk, reached by landing accepted changes under a grant; a repository's
  grant is standing, so landing is almost automatic.
- An initial clone or import is one changeset, landed under that grant.
- Metadata and changeset tracking go in Mantle's K/V engine, copied into slates.
Also binding: the MEMORY SIZING section (learned, device-respecting, no allocation where design can avoid it).

## Your work, in order
1. **Design first, briefly.** Write the amendment to docs/wip/SLATES_DESIGN.md (§4.17 and R1's reading; the next
   free A-number, claimed per briefs/common.md) and send it to main before building. It must answer:
   - where repository content lives on disk, and in what form (Mantle's content-addressed chunk store holding one
     copy per identity across git, hg and svn, or each tool's native files), argued with evidence; where shapes
     compete, an A/B;
   - what the K/V engine holds (refs, names, records, indexes, the identity-to-location map, changeset tracking) and
     its keys;
   - how a landing under a repository's standing grant writes, and is durable before it is acknowledged (fsync
     semantics per platform, from Mantle's disk crate);
   - how replication at f+1 meets disk (each node's own disk; what replicates);
   - how RAM stays a bounded, learned cache, and how the lint wall's disk-write confinement (R1, CLAUDE.md §2 item 3)
     changes, in the same change.
   **Prior art to examine first** (Ada, ~12:50 PST: "Again, I would offer examining Anthropic's Scion architecture,
   EdenFS, Piper, and similar implementations."). Write research notes in docs/wip/research/ with evidence tiers (A
   paper, B vendor or standard doc, C deployed code, D blog flagged) and cite them in the amendment:
   - **Anthropic's Scion:** "Redesigning source control", anthropic.com/research/redesigning-source-control, already
     held to in A-645 (read A-645 in the design first). What it stores where, and how changes relate to history.
   - **Meta's EdenFS:** a virtual working copy that fetches lazily from the server, keeps a local overlay for the
     files a user changes, a journal, and a LocalStore cache. Note what EdenFS keeps on disk, and in what (RocksDB
     or SQLite).
   - **Meta's Mononoke and Sapling:** a content-addressed blobstore, SQL/K-V metadata (bookmarks, mappings),
     bonsai changesets and derived data. A-645 and A-666 already borrow its commit graph and unodes.
   - **Google's Piper and CitC:** the repository durable in Bigtable/Spanner, and cloud workspaces holding only a
     client's changes overlaid on Piper. That is the closest published match to "changes in RAM, repositories
     on disk".
   - **Microsoft's VFS for Git and Scalar,** and git's partial clone: objects fetched on demand.
   For each, say what slates takes, what it leaves and why, with numbers where they publish them.
2. **Vendor Mantle's crates.**
   - engine (the K/V), disk (the device layer), chunk (the chunk store), and log if the design needs it.
   - Take them from Mantle's current origin/main at a pinned commit, with a vendor manifest entry the way kiln vendors
     slates; slates already copied aws-lc from Mantle, so follow that precedent.
   - Adapt them to slates' rules: no panics (Mantle shares that rule); R2, so every Arc, Mutex or RwLock is either
     removed or listed as a D-8 exception in clippy.toml with its two owners; the lint wall; and the unsafe budget,
     each unsafe site with its SAFETY line.
   - Mantle's local checkout is ~/Projects/mantle. Read it, never write it. Fetch Mantle's origin into a copy of your
     own.
3. **Build the store behind one seam:** in-flight changes read from RAM, landed content from disk. Then move the
   git, hg and svn agents onto it, one tool at a time, each with a red test bounding resident memory against the
   data's size. Coordinate with w-git (its B1-B4: the pack held once, read in place), w-hg (the 7 GB) and w-svn,
   through inbox notes.

## Rules
Follow ~/fanout-state-w-store/briefs/common.md in full. Clone the state branch first:
`git clone --branch agents/fanout-state git@github.com:hyper-light/slates.git ~/fanout-state-w-store`, or use the HTTPS
URL if SSH fails. Land on slates' origin/main and confirm with merge-base. Never force. Never checkout, switch, stash,
reset or commit-tree. No installs or system changes. Keep ~/fanout-state-w-store/progress/w-store.md current and pushed.
Report per piece: landed (commit), ran (numbers, the machine), next.
