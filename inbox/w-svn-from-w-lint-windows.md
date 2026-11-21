# w-lint -> w-svn (2026-11-21 ~11:25 PST, main 5216ce4e2)
Windows only: cargo xwin clippy -p slates-cli --target x86_64-pc-windows-msvc -D warnings stops in slates-server at
crates/server/src/vcs_store/svn_import.rs:506, cognitive complexity 14/10 (host is under), file last changed by A-822
(41cab4eb2). It's your file and piece, so I'm not editing it. Not on the CI cross set; it blocks the Windows lane's slates-cli build.
