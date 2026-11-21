## hg_interop on main b9fed44e9 (your C1 step 1b): 16 of 16 pass, no skips (2026-11-21 ~08:35 PST)
Run r12, unpinned kiln run (needs macos,disk=30g), landed on Adas-Mac-Studio-2 (macOS arm64, 14 cpus, load 5.6-11.7),
debug, CARGO_INCREMENTAL=0, stock Mercurial 7.2 via SLATES_TEST_HG="uvx --quiet --python 3.12 --from mercurial==7.2 hg".
Command: cargo test --locked -p slates-cli --test hg_interop -- --nocapture. Exit 0; build+run 94 s, tests 15.84 s.
Log on that machine: ~/.cache/w-interop-hg/hg_interop-1121-0834.log. Skip lines: 0 (every test ran against stock hg).
  ok  a_pull_of_one_changeset_sends_its_large_files_stored_delta_as_stock_does
  ok  process_wait::a_mount_table_wait_reads_the_table_at_the_kernels_word_not_on_a_poll
  ok  process_wait::a_tree_is_waited_on_while_it_gains_cpu_and_given_up_stalled_gone_or_spent
  ok  stock_hg_batches_a_pushkey_over_the_ssh_edge
  ok  stock_hg_batches_a_pushkey_over_the_vcs_http_edge
  ok  stock_hg_clone_stream_streams_a_pushed_repository_and_an_imported_one
  ok  stock_hg_clones_over_the_ssh_edge
  ok  stock_hg_clones_pulls_and_pushes_over_the_ssh_edge
  ok  stock_hg_clones_pulls_and_pushes_over_the_vcs_http_edge
  ok  stock_hg_forces_a_new_head_on_a_moved_line_and_every_client_sees_both
  ok  stock_hg_lists_and_moves_keys_over_the_ssh_edge
  ok  stock_hg_lists_and_moves_keys_over_the_vcs_http_edge
  ok  stock_hg_pushes_a_bookmark_and_a_clone_sees_it
  ok  stock_hg_pushes_land_on_the_changeset_store_and_a_restarted_daemon_replays_them
  ok  stock_hg_sees_the_bookmarks_the_cli_makes_retires_and_binds
  ok  stock_hg_without_credentials_is_refused_in_its_own_words
No failures, so no failure output. Target dir there: ~/.cache/w-interop-hg/target, 2.8 GB (kept for a rerun; trim by
that exact path when done).
