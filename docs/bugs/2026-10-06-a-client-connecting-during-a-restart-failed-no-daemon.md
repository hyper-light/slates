# A client that connected while its daemon restarted waited a second, then failed "no daemon"

Date: 2026-10-06.
Area: `crates/ipc/src/rendezvous.rs` (the rendezvous), `crates/cli/src/anchor.rs` (what the anchor holds).
Conditions: 11 (failures handled quickly); design D-10, §4.7, A-114.

## Description

Under `slates anchor` on this Mac (release build, `--quick --shards 2`), `slates status` started at the moment the
daemon was killed exited 3 ("no daemon") after 1,013.8 and 1,016.3 ms. Started 300 ms after the kill it succeeded in
12–14 ms. The restart itself takes about 7 ms: a trace put the restarted daemon's first accept at +6.7 ms after the
kill. Every CLI command or SDK connect that landed in a restart failed, and a reconnecting client that claimed on the
dead daemon's object waited a claim wait first.

## Root cause

A restarted daemon made a new rendezvous: on macOS and Windows `SharedObject::create` unlinks the bootstrap object and
creates another under the same name, and on Linux the dead daemon's abstract socket vanished with it until the next
one bound. A client that had already opened the dead daemon's object published its claim there, where no daemon would
ever look, and waited out the claim wait (`CLAIM_WAIT_NS`, one second); on Linux the connect was refused outright. The
anchor held the NFS listener across restarts (§4.6) but not the endpoint every client connects through.

## Edits (A-114)

- `slates_ipc::rendezvous::HeldRendezvous`: the anchor creates the rendezvous once and keeps it — an inheritable
  listening socket on Linux, the bootstrap object with nothing published on macOS and Windows — and hands it to each
  daemon (`ENV_RENDEZVOUS`, `Supervisor::hold_environment`).
- A daemon told it is held adopts it: on Linux it duplicates the inherited socket and checks it listens at the
  instance's own address; on macOS and Windows it opens the existing object, checks its magic and slot count, returns a
  claim the dead daemon took and never answered (`ANSWERING`) to `CLAIMED`, frees `DONE` slots, and stamps the header.
  A client's claim made during the restart is then answered by the next daemon; on Linux its connect waits in the
  backlog.
- A daemon with no anchor makes its own rendezvous, as before.

## Tests

- `crates/cli/tests/held_rendezvous.rs` (T-2.16): six times, kill the daemon and at once run `slates status --json`;
  every one exits 0, answered by the next daemon, inside the claim wait. macOS 15.6–17.0 ms longest (three runs), Linux
  (Docker, non-root) 14.5 ms. Before: exit 3 after the claim wait.
- The IPC, client and anchor suites pass on macOS and Linux; Windows cross-lints.

Gotcha: ten kills in two seconds is, by design, a crash loop (`RestartPolicy`: at most recovery budget ÷ start p99
restarts a window); the anchor stops restarting and its held endpoints go with it. The test spaces its kills.
