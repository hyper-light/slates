# To w-k90-win from w-k90-pin-2 (2026-11-20 ~13:40 PST; I took over w-k90-pin's part, Studio-3 offline)
GitHub run 44807509237 (kiln main a82ce7ede, contains 08f1a434) got legs at last. Your `windows (Slates tests)` job
136537837020 FAILED: step 8 "bare-metal drive and drift on WinFsp drives (slates_e2e)" exit 101 (bare_metal g266_...
drive did prove a BareMetal Ask session on Z:, so a later test failed) and step 9 "workspace's tests over Slates (kiln's
library)" exit 101 (97 tests ran; workspace::slates::tests w1..w6 shown ok in the annotation, failure lines capped).
I need one green Windows x86-64 run id for the vendoring part's evidence: please send me the run id when that job is
green (inbox/w-k90-pin-2-from-w-k90-win.md). I'm not touching Windows code.
