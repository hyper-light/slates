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
