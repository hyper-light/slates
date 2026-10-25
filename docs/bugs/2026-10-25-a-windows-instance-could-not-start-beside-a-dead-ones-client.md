# A Windows instance could not start beside a client of its dead owner

- **Date:** 2026-10-25
- **Status:** fixed 2026-10-25, in the same change as this record (A-176).
- **Subsystem:** the client rendezvous (§4.7; A-114): the macOS and Windows bootstrap object in
  `crates/ipc/src/rendezvous.rs` (`fresh`, `Held::hold`, the listener's `open`) over `crates/mem/src/shared.rs`.
- **Severity:** no daemon of the instance could start on Windows while any client of its dead owner was still running,
  however long that took: a daemon run alone (`slates daemon`, development), or an anchor started again after its
  predecessor died (a crash, or a `slates stop` then `slates start` while some client process lingered). The start
  failed with `CreateFileMappingW (the name is already a live section)`, code 183. A client of the dead owner was no
  better off: its reconnect found the dead object, and no daemon answered its claim.
- **Exposure:** Windows only. On Linux the abstract socket's name dies with the listener; on macOS an owner unlinks the
  name it finds and makes its own object. Present since the Windows rendezvous was written (Phase 2); A-114's text even
  said Windows "unlinked and recreated" the object, which a Windows name cannot be.
- **Found by:** the client binary's restart tests on jupiter (Windows 10.0.26200) before they held the rendezvous: the
  second daemon of a test was refused. Then a failing test of the case itself.

## Symptom

`crates/ipc/tests/rendezvous.rs`, `a_fresh_rendezvous_opens_while_a_client_of_a_dead_daemon_lives`, on jupiter before
the fix: a listener run alone, a client connected to it, the listener and its ends dropped as a daemon that dies with no
anchor, then a fresh listener for the instance:

```
thread 'a_fresh_rendezvous_opens_while_a_client_of_a_dead_daemon_lives' panicked at crates\ipc\tests\rendezvous.rs:545:6:
```

The panic is at the fresh listener's `expect`: its open was refused.

## Root cause

Every connected client's doorbell maps the bootstrap object, and a Windows section's name lives while any process
maps it. The owner that died left its object behind in its clients. The next owner tried to make the object new, and
slates refuses a live name rather than share it, since two live owners writing one object would answer each other's
claims.

## Fix

The owner of a rendezvous (the anchor that holds it, or a daemon run alone that made its own) holds a token: a named
Event, `slates-rv-<instance>-owner`, created only if no live process holds one (`Event::create_new`). The owner keeps it
for life and no one else opens it, so its name lives exactly as long as its owner. An owner holding the token knows no
live owner remains, and takes over an object it finds (`take_over`):

- its layout is checked, and a foreign object under the name is refused;
- its claims are readied as a restarted daemon readies them, so a claim a client of the dead owner made is answered
  now;
- nothing is published and no anchor is named until the new owner says so, so the dead daemon's clients judge it gone.

A second owner of a live instance is refused `InstanceHeld` at the token. Before, it was refused at the object's name.
macOS keeps its unlinking create, with an empty token.

## Verification

On jupiter, before and after the fix: the test above fails, then passes, and the rendezvous suite passes whole, 12 of
12. A second test, `a_second_owner_of_a_live_instances_rendezvous_is_refused`, holds the refusal on Windows
(`InstanceHeld`) and on Linux (the address in use), for a daemon run alone and for an anchor, in each order. Red-checked
on jupiter: with the token taken by a plain create-or-open, the second owner was let in (`Ok(())`). This Mac and fedora
pass the same suites.

## Siblings

- macOS takes a live owner's name over: an owner unlinks the name it finds. So a second daemon of a live instance runs
  beside the first, new clients reach it, and the first keeps the clients it has. Recorded in GAPS; the second-owner
  test leaves macOS out for that reason.
- A stopped daemon's start stamp stayed published while an anchor held the object, so a client in the daemon's own
  process judged a stopped daemon alive. It is now taken back at the listener's drop (A-176).
