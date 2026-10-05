# Two mounts of a file never met in the lock table (2026-10-04)

**Description.** Two NFSv4 clients reaching one file through two mounts of its volume could both hold a write lock on
the same byte range, and could open the file with conflicting share reservations. Two Docker containers, each with its
own `slates export` of one volume, are exactly this case.

**Root cause.** A slates file handle carries the mount capability (attachment id and token, §4.13 AUD-01) after the
file's identity (volume, inode, generation). The NFSv4 open table (`crates/bridge-nfs/src/v4/files.rs`, `by_file`)
and lock table (`crates/bridge-nfs/src/v4/lock.rs`, `by_owner`) keyed a file by the handle's raw bytes. Two mounts name
one file with two handles, so their opens and locks sat under two keys and never met. The conflict scans
(`FileState::open`, `refuse_denied`, `LockTable::conflict`) only saw state from the same mount.

**Impact.** Advisory POSIX locks and share reservations did not exclude clients on different mounts of one volume. A
database or tool relying on `fcntl` locks across two containers sharing a volume could corrupt data. No effect for
clients sharing one mount (one capability).

**Edits.** `handle::identity` (the handle re-encoded with no capability) keys and compares files in both tables: the
open and lock keys, the conflict scans, and the state-to-handle checks. Durable records keep the client's own handle;
the identity is derived on restore. Test first: `locks_and_shares_meet_across_two_mounts_of_one_file`. Its lock half
failed (B's overlapping lock granted, status 0); its share half fails with the old keying (status 0 where
`NFS4ERR_SHARE_DENIED` is due). Two drafts of that half were vacuous, because the denying client conflicted with an
open on its own mount; the test now gives it a third mount. Sibling sweep: no other state in `bridge-nfs` or the server
is keyed by handle bytes.
