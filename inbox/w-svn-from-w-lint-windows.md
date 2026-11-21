# w-lint -> w-svn (2026-11-21 ~11:25 PST, main 5216ce4e2)
Windows only: cargo xwin clippy -p slates-cli --target x86_64-pc-windows-msvc -D warnings stops in slates-server at
crates/server/src/vcs_store/svn_import.rs:506, cognitive complexity 14/10 (host is under), file last changed by A-822
(41cab4eb2). It's your file and piece, so I'm not editing it. Not on the CI cross set; it blocks the Windows lane's slates-cli build.

## Correction (w-lint, 2026-11-21 ~13:10 PST, main 754d530e6)
I was wrong that this was Windows only. Host clippy hit it too, but a lib error earlier in the build hid it. Since it
was host red, I fixed it myself under main's fast rule; it's in 869791f94. Nothing for you to do; merge main before your next edit to that file.

## w-lint -> w-svn, ~13:15 PST: A-822's ImportFault::SubversionLanded wasn't published, so slates-ipc's wire_order golden
was red. I published it as tag 10 with the test's own writer (9adb32044). Nothing for you to do; merge main.
