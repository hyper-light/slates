# w-svn → main 01a20182: SlicedLanding::part's piece copies are cut (2026-11-22 ~01:25 PST)

2a8dbcd99 is on origin/main.

- **No split needed.** I checked first, as you asked: neither `agents/w-hg-c1` nor `agents/w-hg-file-deltas-held`
  touches `crates/store/`, and owner.rs's recent history on origin/main is landed work only.
- **The change.** The store owner answers a part with the same buffer, emptied once written, and
  `SlicedLanding::part` returns it. The svn landing now fills one buffer for the whole landing, each content-store run
  copied into it once from the store's lent window.
- **Tests.**
  - Red first: `a_sliced_landings_part_gives_its_buffer_back_to_be_filled_again` didn't compile against
    `part(Vec) -> ()`. Green: every part returns the same allocation, emptied, and the payload reads back whole.
  - slates-store --test owner 5/5.
  - slates-server --lib svn 113/114. The one failure was
    `a_dav_posts_start_commit_hook_that_refuses_refuses_the_transaction_in_svnserves_words`, with "the answer ended
    inside a line (WouldBlock)" at load 74. It passes 3/3 alone; it looks like a test-helper read flake under load.
- **The local attribution** no longer shows `landing::content_piece` or `chain_entry::encode_carrying`.
  - Left: `put_window` 300 calls, 60 MB; the engine's `judge_held`, two `push_slice` and `Increment::encode`;
    `release_one`.
  - Plus two copies that remain by design: the importer's one text buffer, and the increment's post-state, until the
    engine names stored content.

r18 can run at 2a8dbcd99 or later to include this.
