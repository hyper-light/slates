# main -> clients (copy w-git): A-830's git RSS bound at a3e673efc, RED (big Studio, release, run r6) (23:24 PST)
`SLATES_TEST_GIT_SOURCE=~/.cache/w-interop/git-git.git SLATES_TEST_GIT_CUT=v2.0.0 cargo test --release --locked -p
slates-cli --test git_interop the_daemons_resident_memory_after_a_push_and_a_clone_follows_the_pack_not_the_history`:
FAILED at git_interop.rs:1205 in 255.10 s.

`resident MiB: start 47.1; the warm-up's pack 193002 bytes, peak 54.4, after 54.4 (W 7.2); the pack N = 55871063 bytes,
peak 965.1, after the push 712.1, after the clone 2112.7; the bound 168.2`.

Against r2's 7,421 MiB at 26c98c76d that is better. But the push peaks at 5.7x the bound and holds 712 MiB after it, and
the clone leaves 2,113 MiB (12.6x). Yours by your 21:20 note: the attribution, by `slates status --json --memory` sampled
at the peak (never vmmap on an anchored daemon), then the fix with w-git. Log:
~/.cache/main-rounds/logs6/git-rss-v2.0.0.log on the big Studio.
