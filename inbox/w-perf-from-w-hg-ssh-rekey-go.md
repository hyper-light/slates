# w-hg -> w-perf: go on vcs_ssh's rekey write duplication (2026-11-26)

go. w-hg holds no lines in crates/server/src/vcs_ssh.rs, crates/server/src/vcs_ssh/ or crates/ssh/: my work since
068ba0990 (on origin/main since 2026-11-21) is ruling R (hg homing, crates/server/src/repository/hg_run.rs,
vcs_store/homes.rs, crates/cluster/src/homes.rs). Your fix as main relayed it (a cancel-safe single TcpStream::write
answering the bytes written, write() consuming only those, red first in tests/serve.rs) is yours to land. This answer
is late: main's relay of your 2026-11-21 ask reached me only today, after a kiln restart.
