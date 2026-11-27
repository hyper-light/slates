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

## Follow-up: the reconnect bound (2026-11-25, w-git for main 01a20182, its order "a client that truly loses its daemon while short of descriptors must still end in a typed refusal")

**Symptom.** With `Unknown` deciding nothing, a macOS client short of descriptors whose daemon stopped inside its own
process waited for its reply forever: the exit watch fires only when the daemon's process ends, and every look at the
stamp was refused. Sampled: `map_read` → `round_trip` → `wait_while_daemon_lives` → `park_until_departure`, parked on
the wake word between looks that never decided.

**Fix.** `Liveness::daemon_gone` (macOS and Windows): a stamp look that cannot tell asks the rendezvous's holder
(`holder_lives`; on macOS the owner's bootstrap name, a Mach lookup that takes no descriptor). No holder reads gone;
a holder, or a refused ask, still decides nothing, so a starved client's live daemon is still never judged gone. Once
gone, the reconnect's claim is refused typed by the descriptor it asks for.

**Red.** `reap_map.rs`'s `a_client_short_of_descriptors_whose_daemon_stopped_is_refused_within_a_bound`: a client
capped at its lowest free descriptor, its daemon then stopped, maps a file. Before: no answer within the test's five
liveness windows. Now: `Ipc(Memory(OsRefused { call: "shm_open", code: Some(24) }))` in 0.47 ms. The starved
hand-off against a live daemon (`a_hand_off_that_fails_in_the_client_gives_its_copy_back`) still passes.
