# A credential was its issuing node's alone, so a takeover's successor refused the owner's token (2026-11-20)

**Description.** In a fleet at `f = 1` (w-balance's `crates/cli/tests/v3_takeover.rs`: three `slates anchor --fleet`
nodes, stock git over each node's HTTPS edge), a client pushed with a token minted on the repository's owner. After the
owner's anchor and daemon were killed, the successor adopted the repository but answered the owner's token 401 (git:
"could not read Username ... terminal prompts disabled"), and a token minted on the successor proved the successor's own
account, which the repository's access list does not name (403).

**Root cause (logs).** `credentials::enroll` recorded the credential on the partition its id routes to
(`credentials::owner_of`) on the enrolling daemon's own shards, and no path sent it to another member: the successor's
status counted no credential, and its edge's `credentials::find` found none.

**Impact.** Every credential was lost with its issuing node: after any takeover, every client of the lost node was
refused by the survivors until a human enrolled new credentials there, under another host's account.

**Edits (A-764).** `crates/cluster/src/credentials.rs` (the exchange, stream 31); `crates/server/src/credentials.rs`
(`apply_here`, `serve_remote`, `enroll_in_region`, `revoke_in_region`, `heal_region`); `crates/server/src/verbs.rs`
(`credential_on_channel`); `crates/server/src/fleet.rs` (the stream's arm, the healer); `crates/ipc/src/protocol.rs`
(`Refusal::CredentialUnreplicated`, tag 80); `crates/api/src/error.rs` (`RetryCause::CredentialUnreplicated`).
