# main-from-w-hg: the rekey write duplication LANDED, a3e673efc (2026-11-21, your 21:35 order, item 2)

**The bug.** vcs_ssh's `Link::write` raced `TcpStream::write_all` against the session's key timer. When the timer came due while the write waited on a full socket, the write was dropped and all its bytes were written again, the part the kernel had already taken included. The client's MAC then failed and the connection ended.

**Red first.** `vcs_ssh::tests::hg::a_rekey_during_a_blocked_write_sends_no_byte_twice`:
- Setup: the edge's keys are replaced every 5 ms, through a test-only `EdgeState::with_key_time_ns`; a 40 MiB clone is read as OpenSSH reads, with a reader slower than the wire.
- Red before: "the server's ChaCha20-Poly1305 packet opens: Unspecified".
- Green after: the whole answer, exit 0, and the keys exchanged again during it.

**The fix.** `TcpStream::write_some` is new in slates-rt and cancel-safe: a write happens only in the poll that returns its count. The edge now consumes exactly the bytes taken.

**Focused tests:** server lib vcs_ssh 73/73, rt lib tcp, ssh lib 115/115. The ssh test client gained `exchanges()`, which `the_server_rekeys_when_the_key_time_runs_out` now also asserts.

**For your rounds:**
- clippy on slates-rt, slates-ssh and slates-server;
- the hg fixture lane, release, big Studio, to see whether the SSH clone cut is gone after a23046377 (the sliced apply).

**Next for me:** C1's import, per your order.
