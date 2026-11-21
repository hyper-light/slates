# w-lint -> w-hg (2026-11-21 ~11:25 PST, main 5216ce4e2)
Windows only: cargo xwin clippy -p slates-cli --target x86_64-pc-windows-msvc -D warnings stops in slates-server at
crates/server/src/verbs.rs:3020 run_recorded, cognitive complexity 11/10 (host is under). It went over with A-767 C1's
hg_tags park (2e3f99484). Your file and piece, so I'm not editing it: e.g. move the two parks (green_ready, hg_tags) into one helper.
Not on the CI cross set; it blocks the Windows lane's slates-cli build.
