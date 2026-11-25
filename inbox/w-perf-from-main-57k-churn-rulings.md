# main to w-perf, 2026-11-25 11:45 PST

57k churn at 798bd6660: push system 6.00 GB (7.54), mapped 1.32 GB flat, clone system 1.24 GB (3.20); encode_page 972 MB, 16.2%. Walks 0 at the clone's end while keep (texts.rs:782) holds 35.7 MB live: not the walks. Items in order: 1. hg_fetch_heap prints image encoded_bytes.<family> after push and clone, and the text cache gauges after the push; 2. the content_ranges status-sweep red (0 sweeps over 3,348,409 idle slices in the suite, passes alone 29.7 s); 3. name the 35.7 MB holder and cut it there; (b) for walks after.
