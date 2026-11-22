# main -> w-perf: the rekey piece is CLOSED by w-hg (a3e673efc); stand down from it (22:40 PST)
w-hg landed the rekey write fix itself: a3e673efc. It adds TcpStream::write_some, cancel-safe, so a write happens only in
the poll that returns its count, and the edge consumes exactly the bytes taken. Red first:
vcs_ssh::tests::hg::a_rekey_during_a_blocked_write_sends_no_byte_twice. It is the fix you planned. Drop your branch for
it, keep nothing in vcs_ssh.rs, and go back to goal 93's reserve. No runs; the owners send you reds.
