# A target's landing lease was per shard and per path (2026-09-29, AUD-29-03)

## Description

§4.15 step 4 takes "the landing lease on the canonical target" before a landing validates or writes
anything, so that one landing at a time writes into a host directory. The lease did not do that:

- **One table per shard.** Each owner shard kept its own `Leases` table in its runtime landing state. Two
  volumes owned by different shards, landing into the same directory, each took a lease from their own
  table and wrote at once.
- **Keyed by the spelling.** The key was the target's path as the caller spelled it
  (`LandingTarget::key`). Two spellings of one directory — macOS's firmlinked data volume
  (`/System/Volumes/Data/...`), the other case on a case-insensitive volume, a second mount — were two
  leases.
- **A lease that fenced nothing.** The engine took the lease, ran the whole landing and released it. It
  never compared the lease's term with the clock, so a holder paused past its term would keep writing
  after another holder could have taken the target.
- **Records that nothing wrote.** The database had landing-lease records (`Op::LandingLeaseTaken`,
  `LandingLeaseReleased`), but no path wrote them, so a restart forgot every lease.
- **A holder that was a session.** The lease's holder was the caller's session number, and a holder could
  take its own unexpired lease again. Two landings of one principal into one target, from two shards, were
  one holder.

## Root cause

The lease was built as an engine-local structure handed in by its caller, and the server's caller was the
owner shard. Nothing routed a target to one owner, and the only identity at hand was the path string.

## Impact

- **Concurrent writers.** Two landings could interleave in one directory. The per-entry checks of A-43 still
  refused to overwrite an outsider's file, but each landing's validation assumed no other landing was
  writing, and the two could each end `Partial` with conflicts they made for each other.
- **No fencing.** A paused holder's late writes were not stopped by its term.
- **No restart story.** A crash mid-landing left no lease, so a new landing could start at once over the
  crashed attempt's hidden siblings.

## Exact edits

- **`crates/land/src/grant.rs`:** `lease_key(&TargetIdentity)`, the canonical key: the opened directory's
  device and inode.
- **`crates/land/src/engine.rs`:**
  - `land` no longer takes a lease. It requires the caller's live lease on this very target (the key of
    the binding it builds) and refuses `LeaseRequired` otherwise, before any write.
  - Every entry is fenced by the lease's term: an entry that would start after it is
    `Skipped(LeaseEnded)`, and the landing ends `Partial`.
  - `LandingRequest` loses `holder` and `lease_term_ns`.
- **`crates/db/src/partition.rs`:** a take is refused while the target's lease is unexpired, even by its own
  holder, and a take whose generation does not pass the current one is refused `StaleLease`.
  `landing_leases()` lists the records.
- **`crates/server/src/landing.rs`:**
  - The per-shard `Leases` table is gone.
  - `take_target_lease` and `release_target_lease` run on the control shard, over durable records keyed
    by `lease_key`. The generation is the take's log sequence on the control partition, so it only grows,
    across releases and restarts. A take that arrives after its caller's deadline takes nothing. Each take
    first releases every lease whose term has ended.
  - A granted landing runs as an owned task on its volume's owner shard. The task takes the lease through
    a bounded cross-shard call, runs the engine under it, commits the landing's records together with the
    request's completion, releases the lease, and then delivers the reply.
  - A take whose answer does not come back within its term is compensated by a release keyed to the
    attempt.
  - A retry of a running landing joins it (`join_in_flight`, called beside the merge service's join at
    both points where a request is checked before its verb runs).
  - The landing's id and clock are read when it runs, after the wait for the lease.
  - The holder is the landing attempt (`landing_id`'s shape over a per-shard counter), not the session.
  - A presentation (no grant) still runs in the verb and takes no lease.
- **`crates/ipc/src/protocol.rs`:** `Refusal::LandingLeaseLost` (appended): the lease ended before the
  landing started, and nothing was written. `ShardReport` gains `landings_in_flight` and `target_leases`.
- **`crates/server/src/verbs.rs`, CLI, MCP:** the status report fills and prints both counts; the refusal is
  counted `landing_lease_lost`.

## Evidence

- **Failing tests first.** Against `c5b47cb`'s engine (the old API: the caller's `Leases` table and the
  path key), two histories each held the target's lease for another attempt and landed a granted plan
  replacing two files:
  - through another shard's table: `Done`, both entries replaced;
  - through another spelling of the directory in the same table: `Done`, both entries replaced.

  Both assert the new rule, `LeaseHeld`, and both failed (`red-lease-c5b47cb.log`, scratch).
- **The engine now.** `crates/land/tests/lease.rs`:
  - no lease, another target's lease, and an expired lease are each refused `LeaseRequired` with the disk
    unchanged, and a live lease lands;
  - a holder paused past a short term before its first write lands the entry it started, skips the next
    (`LeaseEnded`), ends `Partial`, and a new holder's lease lands the rest.
- **The control shard.** Four unit tests in `crates/server/src/landing.rs`: one holder until release or
  term (a second attempt and the same attempt's second take refused naming the holder; another attempt's
  release frees nothing; the next generation is greater); a paused holder overtaken whose late release
  frees nothing; a take past its caller's deadline takes nothing; and a take releases the ended leases (five
  records, then one).
- **The daemon.** `crates/server/tests/lease.rs`: two volumes owned by different shards, one landing into
  the target and one through macOS's firmlinked spelling of it, are both refused `LandingLeaseHeld` naming
  the one holder while it holds the target, and the target stays empty; released, both land, each file on
  the disk; afterwards no landing runs and no lease is left. Where the host has no unprivileged second
  spelling (Linux without a bind mount), the alias half says so on stderr and the cross-shard half runs.
- **Restart and cancellation.** `crates/server/tests/recovery.rs`
  `a_target_lease_outlives_its_daemon_and_blocks_landings_until_its_term`: a lease taken under a first
  daemon (standing for an attempt its daemon's end cancelled) refuses a landing under the second daemon,
  with the target empty; a lease taken for the recovery budget stops blocking once its term has passed;
  and the minute lease is the only record left.
- **Suites.**
  - Server: lib 129, daemon 17, recovery 8, lease 1, observe 6, attach forms 4.
  - Client 10; MCP 5; CLI unit 33 and 13; land whole (lease 2, oracle 20, durability 5); db whole.
  - Clippy clean on macOS, Linux and Windows (Docker; CI's Windows crate set less the two SDKs, whose
    napi build cannot run in the cross container); xtask check ok; formatting clean.

## Open

- **Renewal.** The term is the operator's failover bound (10 s), as before, and nothing renews it. §4.15's
  table derives it from the measured landing duration and renews it by keepalive while entries are in
  flight; that comes with the sliced engine (AUD-29-25). Until then, a landing longer than one term ends
  `Partial` and resumes under a new lease.
- **Two daemons on one machine** keep separate control partitions, so their landings into one directory
  do not exclude each other. A deployment runs one daemon per host.
- **Nested targets.** A directory and a directory inside it are two targets with two leases. Their
  overlapping entries are still guarded per entry (A-43), not serialized.
- **The fleet register.** The lease record is not yet written to the host's candidate holders (§4.15
  "Networking table"); it never was.
- **A sibling found on the way.** `xshard::call_on` and the forward path drop a refused reply handoff
  (`let _ = send_control(...)`). The caller's deadline bounds the wait, but the loss is not counted.
