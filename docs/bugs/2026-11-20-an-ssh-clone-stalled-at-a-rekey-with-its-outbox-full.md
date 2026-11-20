# An SSH clone stalled at a key re-exchange with its outbox full (A-745, 2026-11-20)

**Found by use** (w-interop, release 295d8723d, `ci/interop/hg-matrix.sh`'s `ssh-clone` row): the clone wrote 25 MB,
then the daemon and the client both sat at 0% CPU for 20 minutes. Locally, w-git's
`vcs_ssh::tests::receive::a745_a_long_historys_clone_over_ssh_is_answered_within_one_requests_room` failed the same
way on clean main 8281ea297 (the clone's session ended at the edge's bound after 811,241 bytes, no exit status).

**Root cause** (`crates/ssh/src/session.rs`, `Session::blocked`). The session stops reading the client's input while
its outbox is at its cap, so a client cannot make the server queue unbounded replies. During a key re-exchange the
transport refuses the upper layer's sends (`SendRefusal::Rekeying`, RFC 4253 §7.1), so a streamed answer fills the
outbox; the outbox then drains only once the exchange ends, and the exchange ends only once the client's half of it
(its KEXINIT, KEXDH_INIT, NEWKEYS) is read, which the full outbox forbids. Both sides wait for the other, with no CPU.

**What starts the exchange.** The server's key time (A-594's tunable, `vcs_ssh::EdgeState::key_time_ns`): the tail of
the lifetimes this edge's authenticated connections lived. One short session before a long clone (the test's own
push; in the matrix, earlier SSH sessions on the anchor) sets it to seconds, so the clone's key is replaced mid-stream:
the log line this fix adds read `SSH key exchange begun by KeyTime after 14307 ms of its key ... the key time 9523 ms`.

**Impact.** Any SSH answer longer than a key time whose sends outrun the client's reads: Mercurial's and git's clones,
Subversion's checkouts over `svn+ssh`. HTTP is unaffected.

**Edits.** `Session::blocked` holds input back on a full outbox only while no exchange holds the sends
(`ServerTransport::is_rekeying`); the client's data meanwhile is still bounded by the channel's window. Every exchange
after the first is recorded with its trigger (`RekeyTrigger::{KeyTime, KeyUsage, Client}`, `Session::take_rekey`) and
the SSH edge logs each. Red then green: `slates-ssh` `a_rekey_with_its_outbox_full_still_reads_the_clients_exchange`
(the rekey never finished, nothing delivered); w-git's long-history SSH clone passes with it.

**Open for main.** The key time's derivation (no floor: a long transfer rekeys every tail interval) is cheap now that a
rekey cannot stall, but RFC 4253 §9 asks once an hour or a gigabyte; A-594's owner to rule.
