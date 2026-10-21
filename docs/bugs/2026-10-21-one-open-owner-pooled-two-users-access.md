# One open-owner pooled two users' access, so either could write the other's file

**Status:** fixed (the acting uid is part of an open's identity; this change).
**Severity:** a privilege bypass between users of one NFSv4 client. A user the mode denies writing could
**write and truncate** a file by sharing an open-owner with a user the mode allows.
**Found:** 2026-10-21, working the sibling left open by
`docs/bugs/2026-10-05-a-delegation-let-any-user-of-its-client-truncate.md`.

## What happens

An NFSv4 open is named by a `state_owner4`: the client id and an **opaque open-owner the client
chooses**. slates keyed its open table by `(file, clientid, owner)` — no principal. A second OPEN under
an existing key did not create a second open; it joined the first:

```rust
open.share.access |= share.access;   // v4/files.rs
```

That is right for one user reopening a file, and it is what RFC 8881 §9.1.1 expects a client to do. It
is wrong across users, because the pooled share is then read as authority:

- `check_io` answers `IoAuthority::Open` when the open's share carries the access asked for.
- `access::setattr_denial` treats `IoAuthority::Open` as "already checked, no bits test" — the
  descriptor-keeps-its-access rule that `ftruncate` needs.
- `write_result` checks the bits only under `IoAuthority::Mode`.

So a user whose own open was read-only inherited the other user's write access and skipped every
permission check.

## The evidence

`crates/bridge-nfs/tests/v4.rs::two_users_sharing_an_open_owner_do_not_pool_their_access`. A file owned
by uid 1000 with mode **0477** — `r--` for its owner, `rwx` for everyone else — so uid 1001 may write it
and its own owner may not. uid 1001 opens it for writing under the open-owner `pooled`; uid 1000 then
opens it for reading under that same open-owner and truncates.

Traced through the server before the fix:

```
check_io other=[..1..] access=2 want=Write     uid 1001's write-capable open
merge    other=[..1..] now_access=3            uid 1000's READ open joined it
check_io other=[..1..] access=3 want=Write     uid 1000 now reads as write-capable
SETATTR  -> 0 (NFS4ERR_OK)                     the truncate was SERVED
```

After the fix the same call is refused **10038, `NFS4ERR_OPENMODE`** — the answer a read-only open's
state id already gets (`a_state_ids_access_mode_and_other_opens_denials_govern_io`), which the Linux
client turns into `EACCES`. Red-checked: with the uid dropped back out of the key the test fails with
`left: 0`, the truncate served again.

The test asserts the WRITE too, not only the truncate, because `write_result`'s bits check is also
skipped under `IoAuthority::Open` — the escape was never truncate-only.

**It took a second attempt to reproduce at all.** The first version of the test passed, and the reason
matters: `Export::new` gives an export its *own* `FileState`, so two exports meant two independent open
tables and the two users' opens never met — the merge never happened and the file state showed
`access=1`. The daemon does not work that way: it lends the owner shard's state to the export it builds
per request (`crates/server/src/nfs.rs:380`). The test lends one `FileState` to both exports, which is
the daemon's actual shape. A test that builds the state per caller cannot see this class of bug.

## The fix

The acting uid joins the open's identity:

```rust
type OpenKey = (Vec<u8>, u64, Vec<u8>, u32);   // file, clientid, open-owner, uid
```

The same user's reopen still merges, which is what the RFC asks for; a principal change starts its own
open with its own share, so `check_io` answers that user from access it was itself granted. Nothing in
the authorization path had to learn about principals, because the pooling never happens.

This closes the write path as well as the truncate path, which a change confined to
`setattr_denial`'s size arm would not have.

The uid is carried on the durable open record (`OpenRecord`, `NfsOpenRecord`) so a restored open still
knows whose it was. Failing that, a daemon restart would have to fail closed and the
descriptor-keeps-its-access rule would quietly change meaning across a restart — a second path for the
same job, which this project does not keep.

## The second door: a lock state id carried the same access

Putting the uid in the open key closed the pooled open and nothing else. Hunting the same class again
found a second way in, and this one the key fix did not touch.

`check_io` resolves a **lock** state id to its open and reads that open's share access:

```rust
let lock = self.locks.get(stateid, fh, Some(clientid))?;
let open = self.opens.get(&lock.open).ok_or(Nfsstat4::BadStateid)?;
```

and a LOCK names an open state id plus a lock-owner, validated by `check_open(&open, fh, clientid)` —
which proves the open belongs to the same **client**, never to the acting **user**. So a second user
could build a lock over the first's open and read its access through the lock state id. Presenting
another user's lock state id directly does the same thing.

Measured, `a_lock_does_not_carry_another_users_open_access`: uid 1001 opens the 0477 file for writing,
uid 1000 locks naming 1001's open state id with a lock-owner of its own, and truncates under the lock
state id. Served. uid 1000 owns the file and its own bits are `r--`.

The fix is the general rule the first one only approximated: **a state id confers authority on the
principal that made it and on nobody else.** `check_io` now refuses `NFS4ERR_ACCESS` when the open it
resolved to was not made by the acting caller. That one check covers both lock doors — building a lock
over another user's open, and presenting another user's lock state id — and it is why the acting uid had
to be on the open in the first place. Red-checked: disabling it fails exactly the lock test and nothing
else, which is the clean split — the key fix owns the pooled open, this rule owns the lock paths.

Delegations and special state ids are unaffected: both return before the open is resolved, and both
already decide strictly on the bits.

The chaos property was extended to generate locks over *other users'* opens, and with the rule disabled
it shrinks to and reports the escape itself:

```
uid 1002 truncating (locked over Some((1001, 0))): served=true, its own opens asked for 1
```

Writing that property taught one thing worth keeping: the first version failed in the *opposite*
direction, served=false where the model expected true, and the model was wrong, not the server. A
repeated lock-owner returns the lock state it was **first** created over, not the open the current LOCK
names, so the lock-owner is now unique per step. A model that states the implementation's accident
rather than the design's rule certifies drift either way.

## Why not the alternatives

- **Refuse the cross-principal merge at OPEN.** Needs the principal on the open anyway, and invents a
  status the RFC does not define for it.
- **Check the bits for every size change.** Breaks the rule `ftruncate` depends on (pjdfstest
  `ftruncate/00.t`): a write-capable descriptor may size its file whatever the mode became.
- **Track access per principal inside one open.** Keeps RFC share-reservation semantics exactly, but
  adds a per-open map keyed by uid — growth that would need its own bound and refusal, for a case that
  is one user in practice.

The cost of the chosen fix is that two users sharing one open-owner now hold two opens, so a DENY set by
one is a share conflict for the other where before it was not. They are genuinely separate opens, and a
client that shares an open-owner across users while setting deny bits is not a case the RFC contemplates.

## Checked and deliberately left alone: CLOSE, OPEN_DOWNGRADE, LOCKU

These take a state id too and also prove only the client (`check_open(stateid, fh, clientid)`), so one
user of a client can close or unlock another's state. That is **not** this escape and is not worth the
same rule:

- Nothing crosses a permission boundary. The damage is confined to the users of the one client, whose
  requests that client already composes — a client that wants to disrupt its own users can simply not
  send theirs. The I/O paths are different in kind: there the server is the only thing standing between
  a user and a file it may not write, so only the server can enforce it.
- Tightening them has a real cost. A client may legitimately tear down state under a credential other
  than the one that opened it — unmount and lease-expiry cleanup are the obvious cases — and refusing
  those would break clients to buy nothing.

Recorded rather than silently skipped, so the next person does not have to re-derive why the rule stops
where it does.

## Still open, and not mine to decide

`write_result` and `read_into` use `permits_io` — which gives the file's owner an override — for a plain
WRITE or READ under a **special** state id (`IoAuthority::Mode`). A read-only-mode owner can therefore
write its own file that way. That is deliberate NFS owner-override policy, not this escape, and whether
POSIX conformance should make it strict the way truncate now is belongs to Ada.

## Exact edits

- `crates/bridge-nfs/src/v4/files.rs` — `OpenKey`, `Open.uid`, `OpenRecord.uid`, `open()`'s acting uid,
  the `by_file` scans, `journal_open`, `restore`, `remove_open`.
- `crates/bridge-nfs/src/procedures.rs` — the acting caller read before the file state is borrowed and
  threaded through `open_state`.
- `crates/db/src/catalog.rs` — `NfsOpenRecord.uid`.
- `crates/server/src/nfs_state.rs` — the record mapped both ways.
- `crates/bridge-nfs/tests/v4.rs` — the escape test and `cx_as`.
- `crates/bridge-nfs/tests/v4_delegation.rs` — `OPENER` for the 27 direct `FileState::open` calls.
- `crates/db/tests/model.rs` — a distinct uid per client through the recovery history.
