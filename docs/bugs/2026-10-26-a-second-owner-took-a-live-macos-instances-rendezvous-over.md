# A second owner took a live macOS instance's rendezvous over

- **Date:** found 2026-10-25 (A-176's sweep), fixed 2026-10-26.
- **Status:** fixed 2026-10-26 (A-178), decided under the standing goal, not in plan mode.
- **Subsystem:** the client rendezvous on macOS (§4.7): `crates/ipc/src/rendezvous.rs` (`Owner`, `take_ownership`,
  `owned_object`) and `crates/ipc/src/owner_name.rs` (new).
- **Severity:** two daemons served one instance with separate state. New clients reached the second owner, the first
  kept the clients it had, and each saw a different set of volumes, attachments and writes.
- **Exposure:** macOS only, whenever a second owner of a live instance started: a second `slates anchor` for the same
  instance, or a daemon run alone beside an anchor or another daemon run alone. Present since the macOS bootstrap
  object was written (Phase 2). Linux refused the second owner at the abstract address, Windows at the owner's token
  (A-176).
- **Found by:** reading the owner rules while fixing A-176's Windows defects; the second-owner test left macOS out.

## Symptom

`crates/ipc/tests/rendezvous.rs`, `a_second_owner_of_a_live_instances_rendezvous_is_refused`, enabled on macOS, before
the fix (this Mac, 2026-10-26):

```
a second daemon run alone is refused while the instance's owner lives: Ok(())
```

## Root cause

macOS had no owner's token: `take_ownership` returned an empty `Owner`, and `owned_object` made the object with
`SharedObject::create`, which unlinks the name first. A POSIX shared-memory name lives until it is unlinked, whoever
holds the object, so the unlink took a live owner's name as readily as a dead one's.

## Fix

The macOS owner holds a token, as the Windows owner does: a Mach bootstrap name that launchd gives its first registrant
and frees when that registrant's receive right is gone (`owner_name.rs`). `take_ownership` claims it, and a claim that
finds a live holder is refused `InstanceHeld`. Holding it, an owner's unlink can meet only a dead owner's object.
- **Where:** at the top of the caller's bootstrap chain, the namespace that is its own parent, so every login session
  of the user meets one token, as they meet one shared-memory name.
- **The name:** `com.slates.owner.<uid>.<16 hex digits of a BLAKE3 of the rendezvous's name>`.
- **Telling a holder from another refusal:** a refused registration is a holder only if `bootstrap_look_up` finds the
  name. The server answers a second process 1100 and a second registration of one process 1101.

Each measured on this Mac (macOS 26, unprivileged, 2026-10-26) before it was relied on:

| question | answer |
|---|---|
| can the shared object itself be locked? | no: `flock` `ENOTSUP`, `fcntl` record locks `EBADF` |
| second registrant while the first lives | refused: 1100 from another process, 1101 from the same one; `bootstrap_look_up` finds the holder |
| free after the holder drops its right | 200 of 200 re-registrations accepted at once, the worst 0.105 ms |
| free after the holder is killed and reaped | 20 of 20 accepted on the first call, the worst 10.25 ms |
| the chain from this GUI session | three namespaces deep, a registration accepted at each; a name in the parent refused to a second process there and in its own namespace |

## Verification

- **Green:** both tests pass on this Mac, and the rendezvous suite passes whole, 14 of 14, on this Mac and on fedora.
  - `a_second_owner_of_a_live_instances_rendezvous_is_refused` now runs on macOS too.
  - `a_second_process_is_refused_a_live_instances_rendezvous` (new) re-invokes the test binary as the second owner,
    which must be refused both as a daemon run alone and as an anchor while this process owns the instance, and must
    own it both ways once this owner is gone.
  - The restart and takeover tests in the suite still pass, since a dead owner's token is gone with it.
- **Linted:** `slates-ipc` is clean (all targets, `-D warnings`) on macOS, on fedora, and on jupiter for Windows.
- **Red-checked** on this Mac with a token that never meets a holder (a fresh name at each claim). Both tests fail:
  "a second daemon run alone is refused while the instance's owner lives: Ok(())" and "a second process run alone is
  refused while the instance's owner lives: Ok(())".

## Siblings and what is still owed

- The token's reach beyond one GUI session is not measured. Still open: an ssh login's parent namespace, and the one a
  launchd job is given. CI's macOS runner is such a job and runs both tests, so its result answers the second.
- `bootstrap_register` is deprecated in the SDK's headers. It works on macOS 26 unprivileged, as measured above. If a
  release removes it, both tests fail on that release, which is the signal to choose again (the SysV semaphore is
  recorded in GAPS as the machine-wide alternative, with its costs).
