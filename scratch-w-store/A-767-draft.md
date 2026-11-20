### A-767 — Changes in RAM, repositories on disk, metadata in Mantle's K/V engine: the version-control store (2026-11-20, w-store; Ada's storage ruling ~12:40 PST and memory-sizing ruling ~12:15 PST; §4.17.1 "Objects", "Durability and residency"; R1, R10; CLAUDE.md §2 item 3; A-581, A-645, A-662, A-716, A-765)

**The ruling.** Ada, 2026-11-20: "*changes* are kept in ram, and upon grant landed to disk. Full repository content
obviously should be kept upon disk. ... the initial clone should be treated as a unified changeset request, that should
(given grant) almost automatically be transferred from RAM to disk. We keep *changesets and the changes in RAM*.
Likewise, using Mantle's K/V store for metadata or even keeping track of changesets more efficiently and durably is
ideal." On Mantle's code: "Copy into slates". This replaces §4.17.1's "a repository is as durable as every green: in
RAM" and A-662's whole-history store on the owner shard. Prior art: `research/vcs-store-prior-art.md` (Scion, EdenFS,
Mononoke and Sapling, Piper and CitC, VFS for Git and Scalar, git's partial clone).

**Why (measured).** w-hg, release 1d4909821, after a push of Mercurial's own history (an 84 MB store, a 162 MB bundle,
57,053 revisions): RSS 8,520 MiB, 12,055 MiB at idle. w-interop r2 (release 26c98c76d): whole-run peak about 145x the
repository for hg (12,141 MiB, 84 MiB store) and git (7,421 MiB, a 51.3 MiB pack of 164,487 objects). The whole history
is resident because the design kept it in RAM.

**1. Three places, and what lives in each.**
- **RAM: the change in flight, and a learned cache.** A push, a commit, a work and its increment, held as it arrived:
  git's pack, Mercurial's changegroup, Subversion's texts, each in its received (compressed, deltified) form and read in
  place (w-git's B1, `PackSource`; w-svn's `TextSource`). Beside it, one bounded cache over disk (item 6). Nothing of a
  landed history stays resident because it was once pushed.
- **Disk: the repository.** Each node keeps one store per device, under one directory the operator grants (item 7):
  - a **content volume** (`mantle-chunk`): a log of segments holding content by identity, checksummed (CRC-32C) and
    verified on every read, cleaned and scrubbed;
  - a **metadata engine** (`mantle-engine`'s `ShardDb`, one instance a slates shard): the rows of item 3;
  - a **landing log** (`hyper-log`, one a device): the engine keeps no write-ahead log of its own (Mantle
    engine-structure.md §2: "it makes its state durable by checkpoints that record the last applied index ... and replays
    the Raft log from that index"), so a landing's rows are durable in this log before they are acknowledged, and the
    engine's checkpoints let the log go.
- **The fleet: the change, at f+1.** What replicates is the change and its landing record, never pages (item 5).

**2. The form content takes on disk: an A/B before the content half is built.** Three candidates compete, each read
through the same seam:
- **(A) raw objects by identity** (A-581's form): each object's raw bytes once by BLAKE3 across git, Mercurial and
  Subversion, each window compressed. One copy per identity across tools; but raw history is large: the manifests alone
  were 4,495.5 MiB of the 8,520 above for an 84 MB store, about 50x.
- **(B) the received container by identity** (w-git's B1-B4, git's own form, Mercurial's revlog deltas, Subversion's
  svndiff): the pack or changegroup landed as it arrived, objects named by (container identity, offset). Landing writes
  exactly the received bytes; a fetch's pack reuses its deltas. No dedup across tools or across containers.
- **(C) raw identity, delta-stored** (Mononoke's packblob, git's repack): rows name raw identities as (A) does, and the
  volume stores each as a zstd delta against a base the store chooses by identity and path. One copy per identity, and
  near (B)'s bytes.
The A/B (release builds, interleaved, best of 5 with all 5 shown, on this Mac and a Studio): the hg history above, git
v2.0.0 (51.3 MiB pack), a 5,000-revision Subversion import; measured: bytes on disk, landing time, a full clone's and an
incremental fetch's serve time and RSS, and the stock servers' numbers beside them (`hg serve`, `git daemon`,
`svnserve` over FSFS). Until it reports, (B) is what lands first, since git's and Subversion's seams already read in
place and it writes nothing it did not receive; the metadata half (item 3) and the seam (item 8) are the same under all
three.

**3. What the engine holds, and its keys.** One `ShardDb` a shard; a repository's rows live on its owner shard (§4.17.5),
every key prefixed by a one-byte table tag and the repository id (big-endian), so a repository is one contiguous range
and leaving a shard is a range moved:
| Tag | Key | Value |
|---|---|---|
| `R` | repository | the repository record (A-515): name, tool, partition, standing grant id |
| `N` | repository, name | the green a name binds (a ref, a bookmark, a head) |
| `V` | repository, green, version (u64 BE) | the version: head's tool id, tree root identity, D(k) root identity, the changeset that made it |
| `C` | repository, changeset seq (u64 BE) | the changeset record: work id, grant id, state (received, judged, landing, landed, refused), the tool's record bytes (git's push entry, Mercurial's revisions, Subversion's `Record::Revision`) |
| `O` | repository, tool id | the object row: kind, length, place (raw identity, or container identity and offset) |
| `L` | content identity | location: volume, segment, offset, length, holders |
| `D` | derived type, version identity | the derived value's identity (A-647) |
| `G` | grant id | the standing grant: target root, policy, issuer, audit position (R10) |
The changeset table is the changeset tracking Ada asked for: every change has a row from receipt to landing, so a
restart knows which changes were acknowledged and landed, which were in flight (dropped, the client retries), and which
were refused. A-645's derived data stays derived: recomputed or read by `D`, never truth.

**4. A landing, and when it is durable.** A repository's grant is standing (R10's record, issued once at its creation,
item 7), so a change the verdict accepts lands with no further question:
1. its content's writes go to the volume as one group-commit batch, then one flush of the volume (`File::sync_data`
   from Mantle's `hyper-block`: `fdatasync(2)` on Linux, `fcntl(F_FULLFSYNC)` on macOS, since plain `fsync` there
   leaves data in the drive's cache, and `FlushFileBuffers` on Windows);
2. its rows (`C` landed, `O`, `L`, `V`, `N`) go to the landing log as one record, flushed the same way;
3. the rows are applied to the engine's memtable, and the change's RAM is released;
4. only then is the landing acknowledged. Content precedes the rows that name it, so no recovery ever finds a row
   naming bytes the volume lacks; content no row names after a crash is reclaimed by the volume's cleaner.
Concurrent landings on a device share both flushes (group commit). Mantle measured a durable flush on Apple's SSD at about
4.7 ms whatever its size, so a landing batch costs about two flushes (~10 ms on this class of Mac) however many changes
it carries. A one-flush variant (the rows as a record in the volume's own frame) is the A/B's second question. Engine
checkpoints follow Mantle's order (pages, flush, superblock, flush), and the log is trimmed below the applied index.
Recovery opens the volume (its own recovery), opens the engine's newest superblock and replays the log from its index.

**The client's answer.** A push is answered once it is committed at f+1 (§4.16, unchanged) and landed on its owner's
disk. On a laptop (f = 0) that closes "a machine restart loses what was not landed" (§4.17.1); at f >= 1 it adds one
batch's flushes (~10 ms) to a push, small against a push's network time. Decision for main: answer at f+1 in RAM and land
behind (lower latency, a window of loss on a whole-fleet power cut) instead.

**An initial clone or import is one changeset.** It is one `C` row and one grant check, and its names are published
together by its landing record, so it is atomic as any change is. Its bytes are not held whole in RAM: they stream
to the volume in bounded slices as staged content no row names (git's quarantine, A-581 item 4), and the landing record
names them at once. A crashed import leaves only unnamed content, which the cleaner reclaims. RAM during an import is
one slice and its decode window, whatever the repository's size.

**5. Replication at f+1 meets disk.** Each node lands to its own disk. What crosses the fleet is the change (the work and
its content by identity, as now, RAM to RAM) and its landing record in the partition's log; each of the f+1 holders lands
it under the same standing grant. A node that lacks an identity fetches it by identity from a holder, which serves it
from RAM if in flight or from its disk. There is no disk-level replication and no shared disk: Mononoke and Piper
replicate a service, slates replicates the change and lets each disk follow.

**6. RAM: a bounded, learned cache (Ada's memory-sizing ruling).**
- **In flight:** a change is held as received, so RAM is about its wire size plus one decode window. Bound: a push of N
  bytes holds at most k·N + B, with k and B derived (k about 2: the received bytes and one inflated object's scratch).
- **Engine:** memtable and page cache sized by `ShardDb::set_memory` from the shard's learned working set (an EWMA of the
  pages touched a period), given back when it decays; never a fraction of the machine.
- **Content cache:** blocks keyed by (identity, block), S3-FIFO (Mantle's E8 choice), sized by the miss-ratio curve of
  its own traffic, under the device's pressure signal (macOS memory pressure, Linux PSI, Windows commit charge), shrunk
  and refused typed before pressure.
- **The volume's index:** Mantle keeps it whole in memory (Haystack's needle index). Here the identity-to-location map is
  the engine's `L` rows, on disk and cached, so the index costs no RAM per object stored.
What each of w-hg's numbers becomes:
| w-hg, after the hg history push | Today | Under A-767 |
|---|---|---|
| content store holdings | 4,495.5 MiB | 0 after landing; during the push, the 162 MB bundle as received plus a window |
| prefault reserve | 4,931.4 MiB | A-765's decay (w-sizing); no reserve for landed content |
| repository record | 153.9 MiB | rows on disk (`R`, `C`); resident only as engine cache |
| chain entries | 74 MiB | `V` rows on disk; resident only as engine cache |
| mapped reservation | 16,384 MiB | sized from the learned working set, never a constant |
| peak / repository (w-interop r2) | ~145x | at most k·(change in flight) + B; idle: B + the learned cache |
Each tool's move onto the store carries a red test: RSS after a push of N bytes at most k·N + B, and RSS at idle after
landing at most B plus the cache's learned bound.

**7. R1's reading and the lint wall, in the same change.** R1 stays: disk is written only by a landing under a grant.
The store's directory is a landing target: granted once (R10's record; at the daemon's enrollment for the node's store,
named per repository by its `G` row), audited, and written only by landings. CLAUDE.md §2 item 3 becomes "any disk
write outside the `land` and `store` crates under a grant"; the new crate `slates-store` joins `slates-land` in R1's
table of disk-writing crates; the vendored crates (`mantle-engine`, `mantle-chunk`, `mantle-disk`, `hyper-log`,
`hyper-block`) are reached only through it and are listed with their file syscalls; it opens its granted root once and
names every file relative to that handle, so no write can leave it; the hermeticity tracer counts writes under a granted
store root as granted and every other write as a failure.

**8. One seam.** `slates_store::Source`: `piece(identity, len, offset) -> Option<Cow<'_, [u8]>>` (w-git's
`PackSource::piece` shape; w-svn's `TextSource::read_step` calls it), answering from the change in flight in RAM, else
from the cache, else from disk, lending `Cow::Borrowed` from a cached block. Writes: `stage` (content, by identity, into
the change or the import's staged slices) and `land` (item 4's four steps, one `C` row). Changeset tracking: `append`
and `changesets(repository, from)`, the key layout above, so w-svn's `Record::Revision`, w-hg's import entry and w-git's
push entry move onto `C` rows.

**9. Vendoring.** From Mantle's origin/main at a pinned commit, recorded in `vendor/UPSTREAM.md` as aws-lc was: the
closure is `mantle-engine`, `mantle-chunk`, `mantle-disk`, `mantle-crc`, `mantle-codec`, `hyper-block`, `hyper-log`.
`hyper-rt`, Mantle's runtime, is not taken (CLAUDE.md §2 item 2: slates' executor is the only runtime): `ShardDb` is
driven inline on slates' shard (its same-shard path), and `hyper-block`'s issuer is moved onto `slates-rt`'s completion
driver. Each crate is brought under slates' rules: no panic, R2 (every `Arc`, `Mutex`, `RwLock` removed or a D-8
exception with its two owners), the lint wall, and each `unsafe` with its SAFETY line.

**Pieces, in order.** (1) this design and the prior-art note; (2) the vendored crates, building and passing their own
tests under slates' lints; (3) `slates-store`: the seam, the landing, the changeset table, with the A/B of item 2;
(4) Mercurial onto it (w-hg; the 12 GB), (5) git (w-git's B1-B4), (6) Subversion (w-svn), each with its RSS red test.

**Applied in the same change to:** this entry; §4.17.1 "Durability and residency" (a status line); R1's reading (a
status line); GAPS' §4.17 entry; `research/vcs-store-prior-art.md` (new).
