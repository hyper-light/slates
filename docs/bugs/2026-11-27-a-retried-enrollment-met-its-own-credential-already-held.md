# A retried enrollment met its own credential already held

- **Date:** found 2026-11-27 on diag CI 11, diagnosed from the code by w-shared, fixed by w-svn the same day (main's
  order).
- **Status:** fixed.
- **Subsystem:** credentials as fleet state (A-764): `crates/server/src/credentials.rs` (`enroll`,
  `enroll_in_region`).
- **Severity:** correctness. An enrollment refused `CredentialUnreplicated` could stay short of `f + 1` for good, and the
  client met `AlreadyExists` for its own credential.

## Symptom

An enrollment is acknowledged once `f + 1` members hold it. Short of that it is refused `CredentialUnreplicated`,
whose contract (`slates_ipc::protocol::Refusal::CredentialUnreplicated`) says the same request asked again reaches the
rest. Asked again, it was refused `AlreadyExists` instead, and the members it had missed still lacked the credential.

## Root cause

`enroll_in_region` records the enrollment on the enrolling node's partition (`enroll`), then sends it to the region.
`enroll` passes the record to the database, whose guard (`slates_db::credentials::Credentials::check`) refuses any held
or tombstoned id `AlreadyExists`. The first request had already recorded the id there. So the retry stopped at the
enrolling partition, before any fan-out, and never reached the rest.

## Exact edits

- `enroll`: the very record the partition holds is the same request asked again. That means the same id, principal,
  kind (an SSH key's blob, or a token's CRAM-MD5 keyed contexts) and expiry. It is answered `CredentialEnrolled` and
  counted `credential.enroll.repeated`, so `enroll_in_region` sends it on to the region. A different record under a held
  id, and an id revoked here (its tombstone), stay `AlreadyExists`. The database's guard is unchanged.
- Test support: `Daemon::inject_credential_deafness` (a member answers no credential message, as one whose credential
  plane is not yet up) and `Daemon::credential_held`.

## Verification

- **Red, then green:** `a764_an_enrollment_short_of_f_plus_one_completes_when_the_same_request_is_asked_again`
  (`crates/server/tests/fleet.rs`). It forms three nodes at `f = 1` with b and c deaf, enrolls at a (refused
  `CredentialUnreplicated { held: 1, needed: 2 }`), gives c back its answers, then asks the same request again. Before the
  fix the retry was refused `AlreadyExists` (log `~/.local/state/w-svn/logs/44-enroll-red.log`). After, it is
  `CredentialEnrolled`, c holds the token, b (still deaf) does not, and a counts the repeat.
- `a_human_enrolls_and_revokes_a_key_and_an_agent_cannot` (`slates-server --lib credentials`) now expects the
  identical repeat `CredentialEnrolled` and counted. A different expiry under the held id is `AlreadyExists`, and so is
  the first request after the key's revocation.
- The fleet fixtures' token enrollment (`enrolled_token`) shares the scene's retry loop. It needs no other change.
