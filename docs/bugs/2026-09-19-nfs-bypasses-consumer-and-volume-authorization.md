# NFS bypassed consumer and volume authorization (AUD-01)

Date: 2026-09-19. Contracts: §4.13 ("Principals", "Grants", "Access lists"); §4.6 (the loopback
mount); AUD-01 in `docs/bugs/2026-09-14_AUDIT.md`; GAP-A9-9 (the NFS-authority leg). Companion record:
`2026-09-19-mount-capability-attachment-dies-with-its-client-and-the-daemon.md` (found by this fix's
own recovery regression).

## Symptom

The NFS edge served every volume with unconditional read/write. The listener accepted any loopback
TCP connection with no enrolled consumer; the requester trusted the `AUTH_SYS` uid (and fell back to
root with no credential); every export was built with `mount_rights()` — read and write — and never
consulted the volume's access list or an authorized attachment. So a local process that could reach
the loopback port could enumerate volume names, obtain handles, and read or mutate volumes that the
control-channel access rules made private to a consumer — the capability the SDK and CLI channels
require was not enforced at the mount. Root enumeration listed every volume with no filter.
Source-confirmed by the September 14 audit.

## Root cause

Authorization at the loopback edge cannot come from the `AUTH_SYS` uid: a uid is client-supplied and
forgeable, and loopback reachability identifies no one (the design: "a supplied uid and loopback
reachability are not consumer authority"). The edge had no other identity, so it fabricated full
rights. Nothing carried a consumer's capability to the edge, and nothing bound a request to an
attachment.

## Fix

Every volume is served over NFS **only through a mount capability**, and every request carries it.

- **The capability.** `AttachmentRecord` carries the rights the attachment was granted and a random
  16-byte **mount token** (`crates/db/src/catalog.rs`). The access-list-checked `verbs::attach` mints
  the token from the platform's secure random (refusing, never issuing a weak one), stores it with the
  rights bounded by the intent (`granted_rights`: an attach-for-read records no write right), and
  returns it in `ReplyBody::Attached.token`; a green's `attach_green` does the same with read-only
  rights. The token is unguessable and cannot be derived from the attachment id (a routable counter).
- **The mount.** A mount presents the capability in the `MNT` path
  `/<name>@<attachment_hex>.<token_hex>`, or `/@<attachment_hex>.<token_hex>` for the host root scoped
  to that capability. The daemon's serve loop (`crates/server/src/nfs.rs`, `presented_capability`)
  parses it (`split_mount_capability`, a parser of external bytes with hostile-input tests), rewrites
  the path to the bare name for the routing, and the volume's owner shard serves the mount under it.
- **The handle.** The root handle a `MNT` returns and every handle derived from it carry the
  capability: file handle v2 (`crates/bridge-nfs/src/handle.rs`: version, volume, inode, generation,
  attachment, token — 57 bytes). Every later request presents its capability through the handle it
  names (`slates_bridge_nfs::request_capability`), so the edge keeps **no per-connection state** and a
  handle self-authorizes on any connection — the kernel's re-dials and parallel connections need no
  re-mount.
- **The gate.** On the volume's owner shard, `authorized_rights` validates every request: the record
  exists, the token matches, the volume matches, the token is non-zero; the request then runs with the
  record's granted rights (read/write) — never the uid's. A handle whose capability does not authorize
  its volume is `NFS3ERR_ACCES` before any effect; a `MNT` of a path the capability does not make
  visible is `MNT3ERR_NOENT`. So an unbound TCP client, a forged `AUTH_SYS` uid, and a wrong token are
  all refused alike, before enumeration or data access. The uid sets only the POSIX subject.
- **Enumeration.** A root `ls /` lists only the volume the presenting capability authorizes
  (`listable`); a bare `/` lists nothing and enters nothing.
- **The attachment's lifetime.** A host mount's attachment is recorded under the bridge consumer
  (`AttachRequest::HostMount` → `Consumer::Bridge`): it outlives the process that attached and a
  daemon restart, and ends with the kernel's `UMNT` of the mount path (`unmount_capability`, the same
  core as `detach`), a `detach`, or the volume's destroy — see the companion record.
- **The CLI.** `slates mount ID PATH [--read-only]` attaches as a host mount (`Client::attach_mount`)
  and runs `mount_nfs` with the export `localhost:/<name>@<attachment_hex>.<token_hex>` (and `rdonly`
  for a read-only mount); a write mount takes the volume's write lease (D-16), refused `LeaseHeld`
  while another principal holds it unexpired, the read-only mount being the remedy. `slates unmount
  PATH` unmounts; the kernel's `UMNT` ends the attachment. The failure message names the volume, never
  the token.
- **Test support.** `Daemon::mount_capability(name)` mints a durable owner attachment (one
  `begin`…`commit` transaction, as the verb path writes it) and returns the mount path, so every
  in-process NFS test mounts the way a consumer that called `attach` does.

## Failing test first, and regressions

`crates/server/tests/nfs_mount.rs::a_consumer_private_volume_is_served_over_nfs_only_through_its_attachment_capability`
— over the daemon's real NFS loopback socket (no kernel mount, runs in CI): a consumer is enrolled and
a workload channel bound to it; the consumer creates a volume it owns (private to it — the account's
own uid channel gets `Forbidden` on `Status`). An unbound TCP client cannot `MNT` it (`MNT3ERR_NOENT`)
and an unbound `ls /` is empty; a caller forging the account's uid in `AUTH_SYS` and a caller with the
right attachment id but a wrong token are refused the same way; the consumer attaches as a host mount,
receives the token, and the mount of `/<name>@<attachment>.<token>` serves it — a file written through
it reads back **on a second connection that presented no token** (the handle alone authorizes), and
the host root scoped to the capability lists exactly that volume. Then the mount's lifetime: one
attachment holding the write lease while mounted; a `UMNT` of the scoped root ends nothing; the `UMNT`
of the mount path ends the attachment — the handle answers `NFS3ERR_ACCES`, the volume reports zero
attachments and no lease. Before the fix the edge served the private volume with read/write to any
loopback caller and listed it to everyone.

Also: the capability-parser unit tests (`the_mount_capability_parser_reads_a_capability`,
`…_refuses_malformed_paths`: short/long/non-hex/empty token, non-hex id, missing separator, truncated
XDR); the handle tests (`crates/bridge-nfs/tests/handle.rs`: the v2 golden vector, the same object
under another capability is a different handle, a 33-byte v1 handle refused, an unknown version
refused); the recovery crash sweep (`crates/server/tests/recovery.rs`: a handle minted before every
crash point resolves after it — the proof the capability survives a restart); and the live
kernel-mount CLI flow (`crates/cli/tests/cli.rs`, `SLATES_TEST_CLI=1`).

## Validation (this box, 18 cores, 2026-09-19)

- `cargo test -p slates-server --test nfs_mount`: **5 passed, 1.19 s**.
- `cargo test -p slates-server --test recovery`: **4 passed, 5.30 s** (the crash sweep 15/15).
- `SLATES_TEST_CLI=1 cargo test -p slates-cli --test cli slates_mount_establishes…`: **1 passed,
  2.75 s** — a real `mount_nfs` kernel mount: `status` reports one attachment while mounted, the
  daemon's `clients_reaped` moves and the mount still serves, `umount` leaves zero attachments.
- `cargo test -p slates-server --lib`: 99 passed; `--test attach_forms`: 4; `--test daemon`: 9
  (14.39 s); `--test virtiofs`: 2; `cargo test -p slates-db --test model`: 10;
  `cargo test -p slates-bridge-nfs`: all suites green (handle 8, loopback, multi, procedures …);
  `cargo test -p slates-cli`: 33 unit + 9 integration.
- `cargo test -p slates-server --test fleet` (alone, on the final source): **48 passed, 336.44 s**.
- `cargo clippy` (bridge-nfs, server, cli, db, ipc, client, mcp; all targets, `-D warnings`),
  `cargo fmt --check`, `cargo xtask check`: clean.

## Scope and siblings

- The token is a **bearer capability** presented in the mount path, so it is visible in the mounting
  user's own `mount` table — the user's own view on a per-user daemon. The FSKit app-group path carries
  the capability out of band; it is the secondary backend, not a gap here.
- The finer per-uid rule inside a mounted volume (which uid may read which file) is the POSIX
  permission layer (`crates/bridge-nfs/src/access.rs`, enforced per request with the `AUTH_SYS`
  subject); authority to reach the volume at all is the capability's. Both are in force.
- `Consumer::Launcher` (the OCI launcher) still has no writer: an OCI bind's record is the requesting
  ring client's. The bind is the runtime's and carries no capability, so nothing is affected; noted.
