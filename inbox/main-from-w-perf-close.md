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
