# Fresh client ids collide with retained completions after daemon restart

Date: 2026-09-20. Design: §4.7, §4.8, §4.9 RIFL, AC-2.3.

The OCI lifecycle crash regression retained its mount and both bindings, then a new CLI
`status` failed with `unexpected reply to status`. A diagnostic that prints only request
identity and reply discriminant confirms client **13**, sequence **1**, received
`ReplyBody::Created` (discriminant 0). Red run: 4.69 s;
`/private/tmp/slates-oci-identity-diagnostic.log`. Command is the strict CLI lifecycle
regression in the companion OCI binding report.

`Listener::open` starts `next_client` at 1 on every daemon start. A reconnect raises it only
past that reconnecting client's id. Fresh callers then reuse identities whose completion
records survived on another partition. The completion cache correctly answers the old
request key, but admission assigned that key to the wrong caller.

Record a monotonic client-id high-water mark in the control partition before handing out a
new identity. Include it in snapshots and replay. Before admission, restore the floor across
all partitions' retained local completion identities and that durable high-water mark.
The listener must never wrap into zero or a previously issued id; exhaustion refuses fresh
admission while allowing an existing session to resume. This is connection admission work,
not a per-verb coordination call. Failed publication refuses the handoff.

Keep the crash regression's fresh CLI commands; replacing them with the already-connected
observer would hide this defect. Remove diagnostic instrumentation after verification.

## Validation (2026-09-20)

The diagnostic was removed. The full CLI lifecycle history passes in 7.90 s
(`/private/tmp/slates-oci-lifetime-recovery-green.log`), keeping fresh CLI calls after the crash.
The portable restart test also creates a client that runs no verb, then admits a fresh caller
before any old client reconnects. It requires the correct status reply and an identity above
all pre-crash admissions, while retaining every original retry and live-session refusal check.
`cargo test --offline -p slates-db -p slates-bridge-oci -p slates-client` passes 95 tests,
with two existing ignored tests (`/private/tmp/slates-lifecycle-portable.log`). These are macOS
results. The Linux io_uring workspace rerun passes the portable restart case too
(`/private/tmp/slates-linux-lifecycle.log`; 1,554 tests, zero failures, 14 ignored).

The additional `exhausted_client_id_space_refuses_fresh_callers_but_preserves_a_resuming_session`
history seeds the durable reservation at `u32::MAX - 1`, admits the last id, requires a typed
fresh-admission refusal, and then restarts and resumes the old session. Fresh callers must
still be refused and the old session must still execute a status afterward. It passes in
0.98 s on macOS (`/private/tmp/slates-macos-identity-and-snapshot.log`). Its first fixture
omitted the anchor's content object and correctly refused create as `ContentUnavailable`;
the fixture now hands off both metadata and content, as the real anchor does. No refusal
assertion was loosened. Its Linux run and unpublished-admission coverage remain owed.
