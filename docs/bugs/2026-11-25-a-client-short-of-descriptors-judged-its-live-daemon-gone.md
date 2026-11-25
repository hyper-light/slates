# A client short of descriptors judged its live daemon gone (2026-11-25)

**Found by** main 01a20182's whole runs: `crates/client/tests/reap_map.rs`
`a_hand_off_that_fails_in_the_client_gives_its_copy_back` hung alone on macOS (debug) at f954135af and a95b34dfa.
Main's bisect on this Mac: 57f50dcae passes in 2 s; 7113415eb (11-18 15:05, "ipc: a reply wait across a restart inside
the daemon's process ends") fails in 4 s with "the starved mapper's map: DaemonGone { after_ns: 2000000000 }", where
the test wants `Ipc(...)`; later commits hang.

**Description.** The test's starved role caps its `RLIMIT_NOFILE` at its lowest free descriptor, so its map's hand-off
fails in the client (a claim's socket refused, `EMFILE`), and the copy must be given back while the client lives. The
client instead judged its live daemon gone at its 2 s reply deadline.

**Root cause** (confirmed by a log in the starved role: `shm_open` refused, errno 24, `EMFILE`). macOS's
`Liveness::daemon_gone` (`crates/ipc/src/rendezvous.rs`) was `watch.exited() || generation_of(instance)
.is_none_or(|now| now != generation)`, and `generation_of` opened the bootstrap object by name with
`SharedObject::open(...).ok()?`. So any refused open, the process short of a descriptor among them, read as the
daemon's departure.

**Impact.** A client that ran short of descriptors (its own cap, or a busy embedder) judged a live daemon gone at its
reply deadline: its call refused `DaemonGone`, and, where its retry reconnected, the hang the later commits show. The
claim's `open_bootstrap` had the same shape: any refused open read "the rendezvous object is not there (no daemon has
created it)", which a caller might answer by starting a second daemon.

**Exact edits.**
- `rendezvous.rs` (macOS and Windows): a look at the stamp is three-way, `Stamp::Found(generation)`, `Stamp::Gone`
  (no object under the name, a header not yet published, one not ours) or `Stamp::Unknown` (the open refused for any
  other cause), by `refused_stamp` (`ENOENT` on macOS, `ERROR_FILE_NOT_FOUND` on Windows, is gone). `daemon_gone`
  decides on the first two alone; on `Unknown` it waits on, and the exit watch still guards the process.
- `open_bootstrap`: only no object under the name is "no daemon"; any other refusal is returned as itself.

**Red.** The end-to-end test (it now answers `Ipc(OsRefused { call: "socket", code: Some(24) })` in 3 s), and at the
seam `a_stamp_look_refused_for_want_of_a_descriptor_is_no_departure` (`EMFILE` and `ENFILE` read `Unknown`, `ENOENT`
reads `Gone`; before, every refused open read gone).

**Siblings swept** (every by-name open whose refusal decides liveness): `adopt` and `doorbell_waiter` propagate the
refusal typed; Windows' `holder_lives` already tells not-found from other causes; Windows' claim's bell open returns
its refusal typed; `made_again` reads a refused open as "not made again" and waits on, the right shape. Not changed,
reported: Windows' `owned_object` (`Err(_)` makes the object anew for any refused open, under the owner's token, so no
live owner holds it; a refusal other than not-found should be its own refusal). `route.rs` opens nothing by name.
Linux's liveness is a peek on a socket it holds, which no descriptor shortage reaches.
