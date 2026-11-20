### A-767 — One unit, the changeset: changes in RAM, the repository's history an append-only changeset log on disk, everything else derived, metadata in Mantle's K/V engine (2026-11-20, w-store; Ada's storage ruling ~12:40 PST, her changeset ruling ~13:35 PST, her memory-sizing ruling ~12:15 PST; §4.17.1 "Objects", "Durability and residency"; R1, R10; CLAUDE.md §2 item 3; A-581, A-645, A-647, A-662, A-716, A-765)

**The rulings.** Ada, ~12:40: "*changes* are kept in ram, and upon grant landed to disk. Full repository content
obviously should be kept upon disk. ... the initial clone should be treated as a unified changeset request, that should
(given grant) almost automatically be transferred from RAM to disk. We keep *changesets and the changes in RAM*.
Likewise, using Mantle's K/V store for metadata or even keeping track of changesets more efficiently and durably is
ideal." On Mantle's code: "Copy into slates". ~13:35: "More than even storing it as diffs, would it be better to store
it as *changesets*? This way we aren't introducing a second underlying unit." Main's ruling: yes, one unit, the
changeset, everywhere. This replaces §4.17.1's "a repository is as durable as every green: in RAM", A-662's
whole-history store on the owner shard, and A-581's object store as the truth (it becomes a derived form). Prior art:
`research/vcs-store-prior-art.md`.

**Why (measured).** w-hg, release 1d4909821, after a push of Mercurial's own history (an 84 MB store, a 162 MB bundle,
57,053 revisions): RSS 8,520 MiB, 12,055 MiB at idle. w-interop r2 (release 26c98c76d): whole-run peak about 145x the
repository for hg (12,141 MiB against an 84 MiB store) and git (7,421 MiB against a 51.3 MiB pack of 164,487 objects).
The whole history is resident, and expanded, because the design kept it in RAM in derived forms.

**1. One unit.** A changeset is what a client sent and the verdict accepted: git's ref updates and the pack that came
with them, Mercurial's bundle (its changegroup and parts), Subversion's commit (its `Record::Revision` and the texts'
svndiff windows), and an import, all of a repository's history at once. It is the only unit in three places:
- **In RAM, as the change in flight:** held as it arrived, read in place (w-git's `PackSource`, w-svn's `TextSource`),
  never expanded into another form before it lands.
- **On disk, as the repository's history:** an append-only log of landed changesets. Landing appends the bytes held in
  RAM with an envelope (item 2); there is no conversion.
- **Everything else is derived** from the log and cached by identity (A-645, A-647): the head tree, git's trees and
  packs, Mercurial's file revisions and manifests, Subversion's node revisions, blame, path history, and checkpoints of
  full texts. A derived value can always be dropped and computed again; nothing derived is truth.

**2. The changeset log on disk (format proposed to w-hg, w-git and w-svn, to be agreed before it is built).** One log
a device, of segments, group-committed (Mantle's `hyper-log` design: frames, CRC-32C, the stop-at-the-first-hole rule).
A record:
- **Header:** magic `SLCS`, format, tool (git, hg, svn), repository id, the changeset's sequence in its repository
  (u64), the sequence of the changeset it was judged over, the grant id, the work id, the time, its section count;
  CRC-32C.
- **Sections**, each kind, length and CRC-32C:
  - `record`: the tool's own record bytes, as the tool's agent writes them today (git's update commands and push entry,
    Mercurial's heads, bookmarks, phases and obsolescence parts, Subversion's `Record::Revision`);
  - `payload`: the bytes received, unchanged (git's pack, Mercurial's changegroup, Subversion's svndiff texts in order);
  - `work`: the work's path changes, each a path, its change (add, modify, remove, copy-from) and its content's
    identity: the tool-neutral form the verdict already computes (§4.16), Scion's and Mononoke's changeset (Bonsai),
    from which the head tree and every tool-neutral derived type are computed without parsing the payload;
  - `names` (optional): each object's tool id and its offset in the payload, when the agent already computed them on
    arrival, so the index (item 3) is filled without reading the payload again.
- **Trailer:** the changeset's identity (BLAKE3 of the header and sections) and a commit mark.
A changeset larger than a bounded slice (an import, a large push) is appended in slices and is visible only once its
commit mark is durable; a log holding slices with no commit mark after a crash is trimmed of them. So an initial clone
of any size is one changeset, one grant check and one atomic publication, and RAM holds one slice at a time.

**3. What the K/V engine holds, and its keys.** One `ShardDb` a slates shard; a repository's rows on its owner shard
(§4.17.5), every key a one-byte tag then the repository id (big-endian), so a repository is one contiguous range:
| Tag | Key | Value |
|---|---|---|
| `R` | repository | the record (A-515): name, tool, partition, standing grant id |
| `N` | repository, name | the green a name binds (a ref, a bookmark, a head) |
| `C` | repository, sequence (u64 BE) | the changeset index: identity, log segment and offset, length, state (in flight, landing, landed, refused), the work id |
| `V` | repository, green, version (u64 BE) | the version: the head's tool id, its full tree's identity, the changeset that made it |
| `O` | repository, tool id | where an object is: its changeset sequence and payload offset, kind, length |
| `D` | derived type, version identity | where a derived value is held (A-647): its identity and its place in the derived store |
| `G` | grant id | the standing grant: target root, policy, issuer, audit position (R10) |
Every row is derived from the log, so the engine needs no write-ahead log of its own (Mantle's engine has none:
engine-structure.md §2). The changeset log is that log: the engine's checkpoint records the last log position it
applied, and recovery replays the log from there (Mantle's own order: pages, flush, superblock, flush). The `C` table is
the changeset tracking Ada asked for: every change has a row from receipt to landing, so a restart knows what was
landed, what was in flight (dropped; the client retries) and what was refused.

**4. A landing, and when it is durable.** A repository's grant is standing (R10's record, issued at its creation, item
7), so an accepted change lands with no further question:
1. its record is appended to the device's changeset log;
2. one flush of the log (`File::sync_data` from Mantle's `hyper-block`: `fdatasync(2)` on Linux, `fcntl(F_FULLFSYNC)`
   on macOS, since plain `fsync` there leaves data in the drive's cache, and `FlushFileBuffers` on Windows);
3. its rows (`C` landed, `O`, `V`, `N`) are applied to the engine's memtable, and its RAM is released;
4. the landing is acknowledged.
One flush a landing batch, shared by every landing on the device (group commit): Mantle measured a durable flush on
Apple's SSD at about 4.7 ms whatever its size (Mantle chunk-store.md §4). Derived values are written to the derived store
(Mantle's chunk volume, by identity) off the landing's path, and losing them loses nothing.

**The client's answer.** A push is answered once it is committed at f+1 (§4.16, unchanged) and landed on its owner's
disk; on a laptop (f = 0) that closes "a machine restart loses what was not landed". Decision for main: answer at f+1 in
RAM and land behind instead (one flush less on the push's path; a window of loss on a whole-fleet power cut).

**5. Replication at f+1 meets disk.** Each node lands to its own disk. What crosses the fleet is the changeset (RAM to
RAM, as now) and its order in the partition's log; each of the f+1 holders appends the same changeset, so their logs
hold the same changesets with the same identities. A node that lacks one fetches it by identity from a holder. Derived
data is never replicated: any node derives it, or fetches it by identity as a cache fill.

**6. The derivation seam, and old versions.**
- **Reads:** `slates_store::Source::piece(identity, len, offset) -> Option<Cow<'_, [u8]>>` (w-git's `PackSource::piece`
  shape; w-svn's `TextSource::read_step` calls it): a changeset's bytes from RAM if in flight, else from the block cache,
  else from the log, `Cow::Borrowed` from a cached block where it can.
- **Derivation:** A-647's mechanism, unchanged in shape: a derived type for a version is computed from its parents'
  values and the changeset that made it, and kept by (type, version identity) in `D`. What a tool serves (a pack, a
  bundle, a node revision, a file at a version) is a derived type.
- **Checkpoints:** an old version's file is read from the nearest checkpoint of full texts at or before it, then the
  changesets after it. Their spacing is learned by A-581's ski-rental rule: the rent is the replay time reads have spent
  past the last checkpoint, the price the measured time to write one; so a hot old version gets one, and a cold history
  costs nothing.

**7. The A/B: changesets plus derived forms against per-file deltas.** (A) this design; (B) per-file deltas, revlog's
form (w-hg's paused change: each file's revisions as deltas against the previous, fulltexts at a chain bound). Release
builds, interleaved, best of 5 with all 5 shown, on this Mac and a Studio, base commit named. Datasets: Mercurial's own
history (57,053 revisions), git v2.0.0 (164,487 objects, 51.3 MiB pack), a 5,000-revision Subversion import.
Measured: import time; RSS, peak and idle; disk bytes; clone throughput; an old version's read latency (a file at the
history's first, middle and last tenth, cold and warm); checkpoint spacing as learned and its disk cost. Beside them the
stock servers' numbers (`hg serve`, `git daemon`, `svnserve` over FSFS). The numbers go in BENCHMARKS and this entry.

**8. RAM: the change in flight and a bounded, learned cache (Ada's memory-sizing ruling).**
- **In flight:** held as received, so a push of N bytes holds at most k·N + B, with k and B derived (k about 2: the
  received bytes and one decode window). An import holds one slice.
- **Engine:** memtable and page cache sized by `ShardDb::set_memory` from the shard's learned working set (an EWMA of
  pages touched a period), given back as it decays.
- **Block cache** over the log and the derived store: keyed by (identity, block), S3-FIFO (Mantle's E8), sized from its
  own miss-ratio curve under the device's pressure signal (macOS memory pressure, Linux PSI, Windows commit charge);
  shrunk and refused typed before pressure. On macOS its give-back unit is a purgeable entry (w-sizing's finding:
  `MADV_FREE_REUSABLE` on shared memory leaves RSS).
What each of w-hg's numbers becomes:
| w-hg, after the hg history push | Today | Under A-767 |
|---|---|---|
| content store holdings | 4,495.5 MiB | in flight: the 162 MB bundle as received; after landing 0, the bundle is one changeset in the log |
| prefault reserve | 4,931.4 MiB | A-765's decay (w-sizing); nothing reserved for landed history |
| repository record | 153.9 MiB | `R` and `C` rows on disk; resident only as engine cache |
| chain entries | 74 MiB | `V` rows on disk; resident only as engine cache |
| mapped reservation | 16,384 MiB | sized from the learned working set |
| peak / repository (w-interop r2) | ~145x | at most k·(change in flight) + B; idle: B + the learned caches |
Each tool's move onto the store carries a red test: RSS after a push of N bytes at most k·N + B, and at idle after
landing at most B plus the caches' learned bounds.

**9. R1's reading and the lint wall, in the same change.** R1 stays: disk is written only by a landing under a grant.
A node's store directory is a landing target granted once (R10's record at the node's enrollment; each repository's
`G` row names it), audited, and written only by landings and by the derived store's fills under it. CLAUDE.md §2 item 3
becomes "any disk write outside the `land` and `store` crates under a grant" (Ada's ruling authorizes it); `slates-store`
joins `slates-land` in R1's table of disk-writing crates; the vendored crates are reached only through it, listed with
their file syscalls; it opens its granted root once and names every file relative to that handle; the hermeticity
tracer counts writes under a granted store root as granted and any other write as a failure.

**10. Vendoring.** From Mantle's origin/main at a pinned commit, recorded in `vendor/UPSTREAM.md` as aws-lc was:
`mantle-engine`, `mantle-chunk` (the derived store), `mantle-disk`, `mantle-crc`, `mantle-codec`, `hyper-block`,
`hyper-log` (the changeset log's frames, group commit and recovery). `hyper-rt` is not taken (CLAUDE.md §2 item 2):
`ShardDb` is driven inline on slates' shard, and `hyper-block`'s issuer moves onto `slates-rt`'s completion driver. Each
crate is brought under slates' rules: no panic; R2 (every `Arc`, `Mutex`, `RwLock` removed or a D-8 exception with its
two owners); the lint wall; each `unsafe` with its SAFETY line. Counted at Mantle e118c09d0 (2026-11-05; counted again at
the pinned commit): about 106,000 lines (the engine 65,687), 54 lines naming `Arc`, `Mutex`, `RwLock` or `Rc` (chunk
30, engine 22, disk 1, block 1), 72 `unsafe` sites (block 32, disk 21, engine 19).

**Pieces, in order.** (1) this design, the prior-art note, and the log format agreed with w-hg, w-git, w-svn; (2) the
vendored crates under slates' lints; (3) `slates-store`: the changeset log, landing, the `C`/`O`/`V`/`N` rows and the
seam; (4) the A/B of item 7; (5) Mercurial onto it (w-hg), (6) git (w-git's B1-B4), (7) Subversion (w-svn), each with
its RSS red test.

**Applied in the same change to:** this entry; §4.17.1 "Objects" and "Durability and residency" (status lines); R1's
reading (a status line); GAPS' §4.17 entry; `research/vcs-store-prior-art.md` (new).
