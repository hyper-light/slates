# The clean-file digest: verified export, invalidation before mutation, bounded discovery (§4.15, GAP-A9-13)

> Status (2026-09-14): built and gated single-node on branch `agent/clean-digest` in four commits
> (`79a75ec` the verified export and its verb surface; `722f91c` the bounded cache invalidated before
> every mutation; `253b53c` watcher hints as revalidation triggers; `4fe303c` the racy rule's clock
> taken from the host), plus one exactly-once fix found on the way (`2c3dd74`). This doc is the
> assistant-owned record; the rule lives in `SLATES_DESIGN.md` §4.15 and the ledger is `GAPS.md`
> (the integrator applies the row text at the end of this document). Nothing here is a benchmark:
> the charter excluded them, and §4.15 says "no performance gain is assumed".

## 1. The rule implemented (§4.15, verbatim)

> A verified content digest xattr, if exported, names exactly the current immutable file bytes. It
> is invalidated before any mutation and absent while content is unsealed or unverified; missing or
> stale cache knowledge cannot produce a clean digest. Discovery uses bounded scans and cooperative
> slices. This is a planned optimization, validated by a counter and a byte oracle before it
> supports a fast path; no performance gain is assumed.

and, from the same section's A-9 paragraph: "Watcher overflow invalidates affected cache knowledge;
hints alone never prove a source unchanged." The ledger's contract for GAP-A9-13: "verified current
digest only; invalidate before mutation; watcher hints backed by revalidation."

## 2. What "clean" means here, and why

A clean file is an **untouched base entry**: a regular file the volume serves from the disk beneath
it, unwitnessed — the complement of the diverged set (§4.4 "witnessed, created, whiteouted and
redirected") for files. Its bytes are exactly the disk's, so a digest of them is a digest of the
source. Every other entry refuses the typed `DigestNotClean`: one the volume created; one it copied
up by a write, truncate, chmod, link or rename (a metadata copy-up too — AC-1.17 says "base metadata
mutations and digest caches obey the same witness rules as content writes", and a chmod'd file's
disk copy may drift while the volume shows the witnessed attributes); one pinned or lost to drift;
and a symlink. An agent that wants a digest of bytes it changed reads and hashes them — the volume
never exports a digest that is not the disk's. Digests of the volume's own *sealed* content (the
design's "immutable file bytes" beyond the base) are out of this charter's scope and remain owed
(§7).

## 3. The three pieces

### 3.1 The verified export (`79a75ec`)

`Overlay::digest(store, path)` (`crates/vfs/src/base.rs`, "digests" section) resolves the path
through the overlay, checks cleanliness, then **verifies current**: the directory's listing is
validated (`follow_live_disk`), the path is opened afresh and its `(dev, ino)` compared with the
descriptor the volume holds — the one case the listing cannot tell is a file replaced beneath a held
descriptor inside the directory's timestamp granularity, which leaves the directory's fingerprint
unchanged and the old inode alive behind the descriptor; when the path holds another inode the
volume adopts it (an untouched entry shows the live disk) and marks the listing for a reread. Two
`fstat`s of one inode that disagree mean it is changing now: `DigestUnverified`. The bytes are hashed
in windows of the store's chunk size (one window of memory whatever the file's length, §4.3 bounded
work — never the whole file, which `copy_up` still reads whole, §8), and the fingerprint is compared
again after the read; a mismatch, or a read that ends short, refuses `DigestUnverified` rather than
export a digest of torn bytes. A read, never a mutation: nothing is journaled and the entry does not
diverge.

The reply is the content facts only — `Digest { identity: [u8; 32], size: u64 }` — so two exports of
unchanged content are byte-identical on the wire even across a rename-over with identical bytes.

The surface: `RequestBody::Digest { volume, path }` and `ReplyBody::Digest { identity, size }`
appended **last** in their enums, `Refusal::DigestNotClean` and `Refusal::DigestUnverified`
appended last (`crates/ipc/src/protocol.rs`) — the schema hashes of `RequestBody`, `ReplyBody` and
`Refusal` change, which is the sanctioned append-only evolution within the major (§4.9); the
server verb `digest` (read right; `crates/server/src/verbs.rs`) with the refusals mapped in
`crates/server/src/error.rs` and named for the counters; `Client::digest` (`crates/client`);
`slates base digest ID PATH [--json]` (`crates/cli`, JSON `{path, identity: hex64, size}`);
`slates.base.digest` over MCP (`crates/mcp`). The volume core's taxonomy gained the three variants
`DigestNotClean` (`ENODATA`), `DigestUnverified` (`EAGAIN`) and `DigestCacheFull` (`ENOSPC`, internal
to the plane, §3.2).

Golden vectors: the published BLAKE3 test vectors for the empty input
(`af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262`) and the one-byte input `0x00`
(`2d3adedff11b61f14c886e35afa036736dcd87a74d27b5c1510225d0f592e213`) are what an empty and a one-byte
base file digest to; both matched at the first run (no `b3sum` on this box, so they are recorded from
the specification's vectors, not from the crate). The byte oracle: a three-window large-class file's
windowed digest equals `blake3::hash` of the whole buffer.

### 3.2 The bounded cache, invalidated before every mutation (`722f91c`)

A kept digest is `CachedDigest { home, fingerprint, identity }` by inode number, with a per-directory
index for hints. It is reused only after the disk re-verified the fingerprint it was computed under
(the fingerprint is the truth; the cache never is); a kept digest the disk no longer matches is
dropped as stale before anything else happens. **Invalidation before mutation**: every mutation of a
base entry — content or metadata — goes through `copy_up`, which drops the kept digest before the
witness is recorded and before the mutation is visible; `base_forget` (the entry leaving the
namespace by unlink, rmdir, rename-away, a landing), a listing refresh whose fingerprint moved, and
the replaced-inode branch of the export drop it too. Every removal returns the shard slot.

**The bound** (§4.2, banned item 8): one counted budget per shard, `Store.digests: DigestBudget`,
derived in `Store::new` from the inode table the store is already sized by —
`digest_capacity(max_inodes) = max_inodes × size_of::<Inode>() / DIGEST_SHARE_OF_INODE_TABLE /
size_of::<DigestRecord>()`, with `DIGEST_SHARE_OF_INODE_TABLE = 16` a `Shape:` constant (one sixteenth
of the inode table's bytes; the inode table is a sixth of a shard's reserve, `STORE_TABLE_DIVISOR` in
the daemon's config, so the cache is about one percent of the reserve). The daemon's boot log carries
the same derivation (`digest_cache_entries`, `crates/server/src/config.rs`). At the bound admission
refuses the typed `DigestCacheFull`; the plane counts it and **still exports** the fresh digest — the
cache is discovery, the export is the contract. An invalidation frees a slot and the next export is
kept again. No eviction policy: the rule asks for a refusal at the bound, and a cache that only ever
returns slots on invalidation is honest about what it holds. A destroy returns every slot
(`Volume::release_digest_cache`); a clone starts with an empty cache.

Measured point (2026-09-14, `cargo test -p slates-vfs --test base
the_digest_cache_refuses_at_its_derived_bound_and_frees_a_slot_on_invalidation -- --exact
--nocapture`): the fixture store (`max_inodes` 65,536) derives **11,264 records**; `size_of::<Inode>()`
= 264, so the record is 96 bytes (65536 × 264 / 16 / 11264). By the same formula the capacity is
`reserve_per_shard / 9216` records: a 64 GiB, 8-shard box (reserve ≈ 2.67 GiB per shard) would hold
about 310,000 digests (≈ 30 MiB) per shard — arithmetic from the formula, not a measurement.

**The racy rule, applied to the cache** (§4.5): a digest computed while the file's timestamp tick is
still open — `now − max(mtime, ctime) ≤ granularity` — could be silently invalidated by a same-tick
write the fingerprint cannot show, so it is exported but never kept (`racy_uncached`). "Now" must be
the filesystem's clock, so the host seam gained `HostFs::now_ns` (`CLOCK_REALTIME` on Unix, FILETIME
nanoseconds since 1601 on Windows to match `filetime_ns`, the simulated clock in `SimHost`, a
delegation in `OsLand`). That verb also exposed the sibling defect of §6.

### 3.3 Watcher hints as invalidation triggers backed by revalidation (`253b53c`)

`process_hints` (`Overlay::status` drains hints) now re-verifies, for a `Changed(dir)` hint, every
digest kept beneath that directory: a fresh open and `fstat` at the entry's disk path, compared with
the fingerprint the digest was kept under; only a mismatch drops it (`hint_rechecked`, `stale`). The
hint triggers the check; the fingerprint decides; a hint alone never yields a digest, and a hint with
nothing changed beneath the kept digests keeps them all, so the next export is reused. Bounded by the
digests kept beneath the directory, which the shard's budget bounds. An `Overflow` drops every kept
digest (`dropped_on_overflow`): the watcher lost events, so nothing kept can be protected by hints,
and the next exports rebuild knowledge lazily — "watcher overflow invalidates affected cache
knowledge" taken literally, O(kept) memory operations and no syscalls on the shard.

A relist (the hint invalidated the listing, or its fingerprint moved) refreshes *every* untouched
entry (`stale_entries` pushes all of them, §8), so the refresh hook drops a kept digest only when the
listing's fingerprint differs from the kept one — the first cut dropped unconditionally and the
"neighbour reused after a hint" test caught it (`revalidated` 0 ≠ 1).

## 4. Counters (every path, so a silently dead path can never pass as a working one)

`BasePlane::digest_stats()` → `DigestStats { computed, revalidated, invalidated, stale, unverified,
cache_full, racy_uncached, hint_rechecked, dropped_on_overflow, cached }`; the shard's
`Store.digests.{capacity, live}`. The daemon counts the two wire refusals by name
(`digest_not_clean`, `digest_unverified`) in its refusal map.

## 5. Tests (do X, expect Y), commands and results, 2026-09-14, this macOS box under load 7–8

| Command | Result |
|---|---|
| `cargo test -p slates-vfs --test base` | 19 passed (the 10 that existed, plus: verified export and typed refusals; never stale beneath outsider edits; determinism and the two published BLAKE3 vectors plus the windowed byte oracle; reuse then invalidation by write/chmod/rename/unlink with every slot returned; refusal at the derived bound and a slot freed by invalidation; nothing kept inside the racy window; a hint drops only the changed neighbour and a no-change hint keeps and reuses; an overflow drops all; a witness racy only in the host's clock) |
| `cargo test -p slates-vfs` (every binary) | all green: 22, 19, 1, 3, 1, 1, 8, 9, 7 (+1 ignored), 31, 1, 3, 0 |
| `cargo test -p slates-base --test host` | 6 passed: the always-on differential (an overlay over `crates/` digests `vfs/Cargo.toml` as `std::fs::read` hashes it; `now_ns` is in the fingerprints' domain), and the `SLATES_TEST_RAMDIR`-gated rename-over-through-the-real-watcher test, which **skipped loudly here** (no RAM-backed directory on this box) |
| `cargo test -p slates-land --test oracle` | 13 passed (the landing's witnesses under the physical racy rule; AC-1.14 equality holds) |
| `cargo test -p slates-server --test daemon the_daemon_serves_the_lifecycle_verbs_exactly_once_with_leases_and_typed_refusals -- --exact` | ok, 14.45 s (its new `digest_scenario`: the wire digest equals BLAKE3 of the bytes `read_base` returned, two exports encode byte-identically, a pinned entry refuses `DigestNotClean`, a scratch volume `Unsupported`) — failing 3/3 before `2c3dd74` in `rifl_scenario`, §6 |
| `cargo test -p slates-mcp --test mcp the_mcp_surface_serves_the_tools -- --exact` | ok, 0.92 s (`slates.base.digest` over the crate's own manifest: 64 hex digits, a positive size, identical twice) |
| `cargo test -p slates-cli --bins` | 19 passed (the grammar parses `base digest ID PATH --json` and refuses a missing path; the JSON rendering of a digest is the hex identity, size and path, identical twice) |
| `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo xtask check` | clean (structural ok over 26 shipped crates, literals ok, unsafe budgets unchanged at every crate) |
| cross-lints (`cargo clippy -p slates-base -p slates-land -p slates-vfs --target x86_64-unknown-linux-gnu`; `-p slates-base -p slates-vfs --target x86_64-pc-windows-msvc --no-default-features --features slates-machine/pure-hash`) | see §5a |

Failing-first, per piece: piece 1's tests failed to compile (no `Digest`, no `Overlay::digest`, no
`DigestNotClean`, no `digest_stats`); piece 2's (24 errors, the counters and `Store.digests`); piece
3's (`hint_rechecked`, `dropped_on_overflow`); the clock fix's test failed at runtime ("a file a
second older than the listing, at a microsecond granularity, is not racy"). Each passed after its
change.

### 5a. Cross-lint results

Not obtainable on this box (2026-09-14): both `cargo clippy -p slates-base -p slates-land -p slates-vfs
--target x86_64-unknown-linux-gnu -- -D warnings` and `cargo clippy -p slates-base -p slates-vfs
--target x86_64-pc-windows-msvc --no-default-features --features slates-machine/pure-hash -- -D
warnings` stop in `zstd-sys v2.1.0`'s build script ("failed to run custom build command", exit
status 1) — the archive codec's C dependency needs a cross C toolchain this machine does not have,
before any crate of ours is compiled. The Unix branch (`crates/base/src/unix.rs` `now_ns`) is the
code macOS compiles and ran here; the Windows branch (`crates/base/src/windows.rs` `now_ns`, ten
lines over `std::time`) is owed to the `windows-latest` CI lane's clippy, as the WinFsp work was.

## 6. Defects found and fixed on the way (each with its record under `docs/bugs/`)

- **`2026-09-14-ack-keyed-on-ephemeral-member-id.md`** (`2c3dd74`): the daemon scenario failed
  deterministically at `rifl_scenario` (3/3 on the branch, 1/1 on the unmodified base `c80b6f9`,
  reproduced by exporting the base tree with `git archive` and running the same command): commit
  `cefb159` (task #22) moved the completion *record* key to the stable cert-anchor but left
  `acknowledge` and the deferred-reply record on the ephemeral member id, so an acknowledgement landed
  in a window the retry check never reads and the retry returned the retained reply instead of
  `DuplicateRequest`. Both writers now key on the anchor; passing after.
- **`2026-09-14-racy-rule-compares-monotonic-with-wall-clock.md`** (`4fe303c`): `load_listing` stamped
  the listing's read time from the volume's monotonic clock (`HostClock`: nanoseconds since its
  creation) and `copy_up` subtracted a wall-clock `mtime` from it, so every production witness was
  racy and every drift check re-hashed the file. The read time now comes from `HostFs::now_ns`;
  the worked-example fixture lets its files' tick close before the volume lists them.

## 7. Owed (with the evidence that sizes each)

- **Cooperative slicing of the hash across shard steps.** `digest` hashes synchronously on the
  owner shard; memory is one window, time is proportional to the file at the machine's measured
  BLAKE3 throughput (`profile.hash.blake3_bytes_per_second`, the anchor the archive walk already
  slices by — `archive_slice_bytes` in the daemon's config). A resumable digest (the
  `SnapshotArchiver::advance` shape) with the server's deferred-reply mechanism is the next piece;
  `read_base` and `pin` have the same shape today.
- **Digests of the volume's own sealed content** ("immutable file bytes" beyond the base): needs the
  background hasher's identities (§4.5 seal), Phase 7.
- **SDK exposure** (`crates/sdk-python`, `crates/sdk-node`): neither exposes `read_base` today, so
  parity did not require `digest`; adding it is mechanical.
- **The RAM-directory differential** was not run here (no RAM-backed directory on this macOS box;
  it skips loudly). CI Linux runs it under `/dev/shm`.
- **The record neighbourhood of the bound**: `DIGEST_SHARE_OF_INODE_TABLE` is a ratified shape until
  the digest hit rate is measured; the counters exist for that measurement.

## 8. Siblings reported, not changed (outside this charter's edit)

- `Overlay::stale_entries` pushes **every** listed untouched file into `changed`, so each relist
  closes and reopens every untouched entry's descriptor (`refresh_unloaded`) whether or not its
  fingerprint moved. Correct, wasteful; the digest hook compares fingerprints itself.
- `Overlay::copy_up` reads a large-class file **whole** (`read_whole(file, fp.size)`) to hash the
  witness identity before deciding the class — an allocation the size of the file on the shard.
  The digest's windowed hasher (`hash_file`) is the shape to reuse.
- `Volume::discard_partial` does not return digest slots; a partial volume (a failed create) has
  never served a digest, so nothing leaks today.
- The Windows host's `now_ns` adds the FILETIME epoch offset to `SystemTime`; a `GetSystemTimeAsFileTime`
  call would read the same clock the FSD stamps with, at the cost of one more FFI site.

## 9. For the integrator: the ledger rows and the design status paragraph

GAPS.md, the `Volume/namespace/base (4.4–4.5, 4.15)` row's second cell, appended: "The clean-file
digest is built single-node: `digest` exports a verified current BLAKE3 of an untouched base file
(identity and fingerprint checked before and after a windowed hash; typed `DigestNotClean` /
`DigestUnverified`, never stale), kept in a bounded shard cache derived from the inode table and
invalidated before every mutation, with watcher hints as revalidation triggers (docs/wip/clean-digest.md)."

GAPS.md, the `GAP-A9-13` row: replace the status cell with "**Built single-node (2026-09-14,
`docs/wip/clean-digest.md`):** verified current digest only (`Overlay::digest`, the `Digest` verb on
the wire/CLI/MCP); invalidated before any mutation (`copy_up`, `base_forget`, listing refresh);
bounded cache discovery (`Store.digests`, `digest_capacity`, typed counted `DigestCacheFull`);
watcher hints backed by revalidation, overflow drops all. Owed: cooperative slicing of the hash,
sealed-content digests, SDK exposure." and keep the AC-1.17/T-1.21 reference.

SLATES_DESIGN.md §4.15, a status blockquote after the digest paragraph (line 2386):

> **Status (2026-09-14, digest).** The clean-file digest of this paragraph is implemented
> single-node (`crates/vfs/src/base.rs` "digests"; `docs/wip/clean-digest.md`): `digest(volume,
> path)` exports the BLAKE3 of an untouched base entry's bytes verified current — the listing
> validated, the path re-opened and matched by identity to the held descriptor, the fingerprint
> compared before and after a windowed hash — and refuses typed (`DigestNotClean` for any diverged
> entry or symlink, `DigestUnverified` for a file changing under the hash) rather than ever export
> a stale digest. Verified digests are kept in a per-shard cache bounded by a derived share of the
> inode table (a typed, counted refusal at the bound; the export still succeeds), dropped before
> every mutation of the entry and whenever the disk no longer matches, never kept when computed
> inside the racy window (the host's own clock supplies "now" through `HostFs::now_ns`); a watcher
> hint re-verifies the digests beneath the named directory by fingerprint and an overflow drops
> them all. Validated by the counters (`DigestStats`) and the byte oracle (a windowed digest equals
> the whole-buffer hash; the published BLAKE3 vectors). Owed: cooperative slicing of the hash
> across shard steps, digests of sealed overlay content, SDK exposure.
