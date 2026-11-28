# main to w-interop: the hg incremental push's counters (2026-11-28 03:43 PST)

Your run request of 01:54. hg_scale.sh ran with stock `hg serve` (Mercurial 7.2.4) and slates over hg-src (57,171
changesets, 307 tags), one run, KEEP_LOGS, release slates ee81fc621b2e at main 86a8f9885, which holds 1360fd527. It ran on
this Mac, 03:05:14 to 03:38:54 PST, at load 23 to 42. Every check passed: changesets and tags identical, verified, and
the same digest after the incremental pull.

| step | stock s | slates s |
|---|---|---|
| push | 49.067 | 74.352 |
| clone | 82.166 | 36.944 |
| verify | 66.028 | 54.269 |
| incremental-push | 0.841 | 9.527 |
| incremental-pull | 0.862 | 1.798 |
| concurrent-clone, 18 clients | 246.828 | 173.842 |
| concurrent-pull, 18 clients | 5.244 | 2.064 |
| stream | 4.198 | 5.393 |

The incremental push is 11.3 times stock's (12.2 at 47e37df66).

The moved line, whole, from results.txt:

```
slates moved incremental-push 1: merge.green.materialize_wait_ns_most=6459919542 vcs_store.hg_stream_store_wait_ns=5964555504 vcs_store.hg_owner_total_ns=5574028000 vcs_store.hg_owner_serve_ns=4158090000 vcs_store.hg_owner_spin_ns=1702457000 vcs_store.hg_owner_wake_ns=1226911000 mem.large.returned_bytes=913162240 vcs_store.hg_store_ask_step_bytes=310876852 vcs_store.hg_owner_disk_ns=152544000 vcs_store.hg_owner_most_ns=106982000 vcs_store.hg_owner_back_ns=34193000 content.trimmed_bytes=9281536 vcs.hg.push_rows_bytes=7980073 rt.polls=7284425 rt.steps=7276722 publish.delta_held_bytes=7256984 vcs_store.image.copied_bytes=7050034 rt.poll_log2.8=6992963 vcs_store.hg_owner_behind_ns=2290000 vcs_http.reply_yield_gap_ns=421333 vcs_store.image.encoded_bytes=292620 rt.poll_log2.9=274776 vcs_http.reply_yield_gap_most_ns=262625 vcs_store.image.encoded_bytes.windows=260950 vcs_store.homes.homing_local_ns=239916 vcs_store.image.read_allocated_bytes=214160 vcs_store.content.reads=101914 vcs_store.image.split_discarded_bytes=22575 vcs.hg.segment_compress_ns=13125 vcs_store.image.encoded_bytes.region_holds=8130 vcs.hg.push_index_runs_visited=7876 rt.poll_log2.10=7184 rt.long_steps=5231 vcs_share.fast=5121 vcs_store.hg_owner_requests=5023 vcs_store.hg_stream_store_asks=4780 rt.poll_log2.18=4708 vcs_store.hg_owner_spin_hits=4683 rt.poll_log2.14=2668 vcs_store.hg_text_requests=2633 vcs_store.content.brought_back=2483 vcs_store.hg_pull_whole_texts=2388 vcs.hg.push_work_texts_read=2385 merge.green.materialize_texts_asked=2384 content.allocations_seen=1824 vcs_store.homes.landed_let_go_bytes=1314 vcs_store.landed.let_go_bytes=1170 vcs.hg.push_index_resolves=1018 rt.poll_log2.7=595 rt.poll_log2.11=549 vcs_store.image.encoded_bytes.aways=539 vcs.hg.segment_landed_record_bytes=503 mem.large.mapped=482 mem.large.unmapped=481 vcs.hg.segment_landed_texts_bytes=449 vcs_store.image.encoded_bytes.landed=436 vcs_store.index_upkeep.steps=381 vcs.hg.push_index_lookups=378 vcs_store.hg_owner_parks=335 rt.poll_log2.12=261 vcs.hg.push_text_bytes_let_go=253 vcs.hg.segment_compress_plain_bytes=227 vcs.hg.segment_landed_payload_bytes=218 vcs.hg.segment_compress_stored_bytes=218 rt.poll_log2.19=171 rt.poll_log2.17=165 rt.poll_log2.13=163 vcs.hg.push_list_bytes=144 vcs_store.homes.homing_region_ns=125 vcs_store.content.stored=122 vcs_store.content.freed=117 rt.poll_log2.20=117 vcs_store.image.encode_turns=98 rt.poll_log2.15=55 rt.poll_log2.16=27 rt.unattributed_steps=19 vcs_store.content.let_go=11 vcs.hg.push_work_turns=11 sweep.passes_started=11 rt.waiting_asked=11 content.ticks.busy=9 xshard.reply_home_ran=8 xshard.reply_home_handed=8 vcs_http.reply_yields=8 rt.poll_log2.6=8 rt.poll_log2.22=6 rt.poll_log2.21=6 publish.committed=5 vcs_store.image.cuts=4 vcs_store.homes.homed_in_region=4 vcs_store.homes.homed=4 vcs_store.hg_intake_base_asks=4 vcs.hg.push_objects_homed=4 push.held_ahead.units=4 vcs_store.image.cut_preimages_old=3 vcs_store.image.cut_preimages_absent=3 vcs_store.hg_sized.revisions=3 vcs_store.homes.published_for_ticket=2 vcs.hg.push_work_log_texts_read=2 vcs.hg.owed_log_texts_read=2 hg_push.intake_slices=2 content.ticks.idle=2 vcs_store.index_upkeep.merged=1 vcs_store.image.publications_skipped=1 vcs_store.image.pages_split=1 vcs_store.homes.settles_queued=1 vcs_store.homes.settled=1 vcs.hg.segment_record_turns=1 vcs.hg.segment_chunk_turns=1 vcs.hg.push_works_built=1 vcs.hg.push_segments_landed=1 vcs.hg.push_need_admitted=1 vcs.hg.push_log_texts_asked_most=1 vcs.hg.push_log_texts_asked=1 vcs.hg.landed_let_go_turns=1 rt.poll_log2.26=1 rt.poll_log2.25=1 rt.poll_log2.23=1 repository.push.update_slices=1 repository.push.files_named=1 repository.push.create_slices=1 push.held_ahead.named=1 push.held_ahead.held=1 merge.green.materialized=1 hg_push.landed=1 client.reaped_by_hangup=1 mem.large.mapped_bytes=-7847936
```

The two status files, on this Mac: /Users/adalundhe/.local/state/main-01a20182/rounds/hg-interop/logs/status.before-incremental-push-1.json and
/Users/adalundhe/.local/state/main-01a20182/rounds/hg-interop/logs/status.after-incremental-push-1.json. The anchor's log is /Users/adalundhe/.local/state/main-01a20182/rounds/hg-interop/logs/anchor-1.log.

What main reads in it, for your next piece:
- The two largest are waits, not work. merge.green.materialize_wait_ns_most is 6.46 s, so one wait on a green's
  materialization took most of the push. vcs_store.hg_stream_store_wait_ns is 5.96 s over 4,780 stream-store asks.
- hg_owner_total_ns is 5.57 s: serve 4.16 s, spin 1.70 s, wake 1.23 s. That is over 5,023 owner requests and 2,633 text
  requests, with content.brought_back at 2,483.
- mem.large.returned_bytes is 913 MB and hg_store_ask_step_bytes is 311 MB, for a one-changeset push.
