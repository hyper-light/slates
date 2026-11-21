# w-perf -> main: items closed (Ada's CLOSE order, 2026-11-20 16:20)

## C1, goal 93 shared: the svn storage A/B vs FSFS: CLOSED
- Commits on origin/main: **a4b6a9a22** (svn's arm of the changeset A/B, `crates/svn/examples/changeset_ab.rs`, and
  its BENCHMARKS section), **756104912** (stock FSFS's bytes for the same history).
- Measurement: git/git's newest 2,000 commits (436.9 MiB dump, 427.4 MiB of whole texts), release, N=3 an arm,
  interleaved, every run shown. The svn records' deltas hold the history in **33.2 MiB** raw, 26.1 to 27.3 MiB under
  zstd 1. Stock FSFS 1.15.0 (format 8, `svnadmin load` of the same dump, SHA-256 matched) keeps **41.0 MiB** of
  revision files (49.8 MiB repository), loading in 105.9 s against slates' 2.6 s. Import RSS is 0.30x the whole
  texts, peak at most 0.57x.

## C1, goal 93 shared: c3: CLOSED
- Commits on origin/main: **2502607c4**, with **1df3d4700** (the merge that keeps w-svn's `read_step` and dispatches
  a stored text to the store alone), and **b51f908f2** (each reader makes a delta window once).
- What it is: `vcs_store::svn_texts::HeadTexts` reads a head text from the green, and any other from the one place the
  records keep it: the content store for a stored text (A-761), the records' delta for an imported or made one. No
  arm is tried after another (`text`, `text_at`, `text_in`, `read_step` each `match records.stored_len`).
- Tests: `a_stored_text_is_read_a_store_window_a_step` (server), `s4_c2_a_fetch_reads_a_texts_pieces_a_window_at_a_time_never_making_it_whole`
  and `s4_c2_a_reader_makes_each_window_of_a_text_once_whatever_the_pieces` (svn).
- Measurement: w-svn's replay_bench over git-2000 on b51f908f2: 328 s before, best 1.82 s after (0.91 ms a revision).

Next on my list: the deltify job (C1, and svn's P), then R2 (the hg import's peak, through w-store's payload).

## C1, goal 93 shared (and 93 svn's P): the deltify job: CLOSED
- Commit on origin/main: **1b168038c** (landed through merge **e1d896547**). Form (A), as you ruled at 16:10.
- What it is: a text a commit stored whole in the content store (A-761) is kept as a skip-delta svndiff by the
  records once a later revision moves its node past it (`vcs_store::svn_deltify`). Its delta is made a 100 KiB window
  a slice, landed as its own entry (`Record::Deltified`, the delta held by the records as c2 holds every other text),
  and only then is the store's hold given back. A recovery's replay holds still-stored texts after the whole log
  (`hold_svn_stored`), so a text a later entry deltified is never asked of the store; it then hands the job whatever a
  restart cut off.
- Test: `vcs_store::svn_deltify::tests::a_stored_text_its_node_moved_past_is_kept_as_a_delta_and_its_whole_bytes_released`.
  Red with the job not handed its texts: (deltified, windows, refused) = (0, 0, 0), the whole text held. Green: (1, 3,
  0), the records no longer keep it stored, the store's holders 0, read back byte for byte before and after a rebuild
  from the log, no replay refused. Codec: `a_deltified_texts_record_is_pinned_read_back_and_refused_by_kind`.
  slates-svn --lib 580/580; slates-server --lib `svn vcs_store::` 176/176 (macOS arm64, this MacBook).

C1 is now closed whole (svn storage A/B, c3, deltify job). Next: R2, the hg import's 5.73 GB, through w-store's
payload (the per-phase profile is rerunning as kiln r20).

## Main's 18:30 order: the derived per-path read form (A-645 on A-767): BUILT on the node's store, measured
- Commits on origin/main: **fa5fd0023** (the `Texts` codec and edit form, `T`/`H` rows at landing and replay,
  `Store::text_at`/`text_row`/`path_texts`, the owner's `text`/`text_row`/`path_texts` requests for shards),
  **c5d746f8b** (a `T` row is its first landing's: a reverted text made a chain loop on Mercurial's history; red
  `a_text_that_comes_back_keeps_its_first_row_and_no_chain_loops`), **33465d641** (BENCHMARKS).
- Tests (macOS arm64, this MacBook): `an_old_file_revision_is_read_from_its_own_chain_alone` (the changesets read equal
  the version's chain, again after a reopen), `a_shard_reads_an_old_file_revision_through_the_owner_across_a_restart`
  (red with no rows: no landed text), the codec's two; slates-store all green.
- Measurement (release, big Studio, kiln r32, Mercurial's history landed as 54,728 changesets): old read on a cold cache
  **p50 85 / 87 µs** (rounds 2 and 3; the bar 91 µs), p99 4.05 ms (arm A 3.7 ms), 0 of 6,000 reads outside their chain.
  Round 1, the log read from the device for the first time: p50 234 µs, p99 20 ms. Landing 17.9 ms a changeset (one
  full flush each: landings sharing a flush are w-store's).
- Interface agreed with w-hg; w-hg's intake writes `Texts` on agents/w-hg-c1. w-store never answered; the change to its
  `store.rs`/`owner.rs` is listed for its review (inbox/w-store-from-w-perf-per-path-landed-2026-11-20.md), with one gap
  for it: changesets landed through `begin`/`applied` get no `P` or `T`/`H` rows.

## R2 (the hg import's 5.73 GB): waits on w-hg's hg onto the store
Attributed (3d39e0f62: 91 % the content store's whole texts). The cut is hg's intake and replay on the changeset store
with the derived reads above; w-hg has the intake writing `Texts` and is blocked on this Mac by the store refusing every
landing ("a memtable past 4 GiB", reported to w-store). I'll measure R2's peak against the `.hg`'s bytes the moment
w-hg's C1 lands.
