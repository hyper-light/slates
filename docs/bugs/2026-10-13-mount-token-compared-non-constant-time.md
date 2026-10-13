# The mount capability token was compared in non-constant time — a timing side-channel on a bearer secret

- **Date:** 2026-10-13
- **Area:** `crates/server/src/nfs.rs` (`authorized_rights`) and `crates/server/src/landing.rs`
  (`constant_time_eq`). §4.13, AUD-01; CWE-208 (observable timing discrepancy), CWE-203 (observable
  behavioural discrepancy on a secret comparison).
- **Severity:** low — a hardening/defense-in-depth defect, not a practically-exploitable escape. The timing
  signal of one byte-comparison difference (sub-microsecond) is buried under the NFS/IPC/shard-hop/runtime
  jitter of the transport (micro-to-milliseconds), and the token space is 128-bit, so a byte-at-a-time timing
  attack over this transport is not practical. It is still a real deviation from the constant-time discipline
  slates applies to its other bearer secrets, and the quality bar ("where vorpal has a gap, slates does not
  inherit it") is to compare every bearer secret in constant time. Found by attacking the VFS (the `/goal`
  loop), auditing the cross-tenant mount-capability boundary.

## Description

The mount capability token (`<attachment_id>.<16-byte token>`, §4.13) is the bearer secret that authorizes an
NFS request to touch a volume: `authorized_rights` looks up the attachment record and serves the request only
if the presented token matches the record's. The match was `record.token != token` — a short-circuiting
`[u8; 16]` comparison that returns as soon as the first differing byte is seen. Its running time therefore
depends on how many leading bytes of a presented (forged) token matched the real one, which is a classic
timing oracle: an attacker who could measure the comparison time could, in principle, recover the token one
byte at a time (turning a 2^128 guess into ~16·256 oracle queries).

The token is agent-reachable: the NFS listener is a localhost TCP port (`daemon.rs`), and any local process can
send it RPCs (the per-uid socket gate protects the *control* plane rendezvous, not this data-plane port, though
the `bound_uid` "stranger" check and the token itself are what authorize here).

## Root cause

An inconsistency, not a deliberate choice. slates already compares its other two bearer secrets in constant
time:

- the 32-byte grant proof, through `landing::constant_time_eq` (`grant_verb`), and
- the MCP HTTP bearer, through an inline XOR-accumulate (`mcp::http::bearer_matches`, "compared in time
  independent of where they differ").

The mount token was the one bearer-secret comparison that kept a short-circuiting `!=`. There was no shared
rule forcing every secret comparison through the constant-time helper, so this site drifted.

## Fix

`landing::constant_time_eq` was generalised from `(&[u8; 32], &[u8; 32])` to a const-generic
`(&[u8; N], &[u8; N])` — the const length couples both operands to one size, so there is no length branch to
leak, and it now serves both the 32-byte proof and the 16-byte token. `authorized_rights` compares the token
with `!constant_time_eq(&record.token, &token)`; the `record.volume != volume` check stays an ordinary
comparison (the volume id is not secret — the caller names it).

The functional behaviour is unchanged (a wrong token was refused before and after); only the comparison's
timing is now independent of the secret. The grant proof's call site is unchanged (`&[u8; 32]` infers `N = 32`).

## Sibling sweep

Every `==`/`!=` comparison of a `token`/`proof`/`secret`/`verifier`/`key`/`digest`/`mac` across the workspace
was enumerated. Only bearer-secret comparisons need constant time; the sweep found:

- `crates/server/src/nfs.rs` `record.token != token` — **the bug, fixed here.**
- `crates/server/src/nfs.rs` `token == [0u8; 16]` — a comparison to a public constant (the "no capability"
  sentinel), leaks nothing secret; left as-is.
- `crates/server/src/landing.rs` grant proof — already `constant_time_eq`.
- `crates/mcp/src/http.rs` `bearer_matches` — already an inline constant-time compare.
- The remaining hits are **not** bearer secrets: NFSv4 session/exclusive-create verifiers (reboot/idempotency
  nonces, not authorization), consensus/receipt integrity digests (the content and its hash are both known to
  the caller — an integrity check, not a secret match), and data-structure keys (ART trie, replay cache,
  mirror). None authorize access from a secret, so a short-circuit there leaks nothing security-relevant.

A hardening note for Ada: a structural `cargo xtask check` rule — "a comparison of a field named `token`,
`proof`, `secret`, or `mac` must go through `constant_time_eq`" — would make this a lint rather than a habit,
matching how slates enforces its other rules. Flagged, not built, to keep this fix focused.

## Verification

`a_handle_with_a_valid_attachment_but_a_wrong_mount_token_is_refused` (`crates/server/tests/nfs_hostile.rs`),
against a live daemon over the raw NFS loopback socket: a file reads with its honest handle (non-vacuity); the
same handle with its 16 token bytes overwritten by `0xff` (the valid attachment id intact) is refused; the
all-zero token is refused. The fuzzed single-bit-flip-under-load test
(`a_mount_survives_connections_broken_and_tampered_mid_call_under_load`) still passes (token-bit flips refused).
The grant-proof path through the generalised helper is intact
(`a_grant_is_accepted_only_with_a_verified_proof_bound_to_the_presented_landing`,
`a_granted_landing_consumes_its_presentation`). `cargo clippy -p slates-server --all-targets -- -D warnings` is
clean. nfs_hostile: 13 passed.

A timing-measurement test is deliberately not added: it would be flaky under the runtime's scheduling jitter and
is the kind of test that should not gate CI. The constant-time property is guaranteed at the code level by the
shared helper (whose own forged-proof and golden-vector tests exercise it), and the functional refusal is proven
by the regression above.
