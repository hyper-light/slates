# A grant did not bind its target, volume or consumer (2026-09-29, AUD-29-01, P0)

## Description

The 2026-09-29 audit (AUD-29-01) found that landing execution did not enforce what the grant's issue had
verified. §4.13 "Grants" requires the daemon to verify "the exact manifest hash, target identity, intended
consumer, scope and validity", and requires that "retargeted or modified-plan grants refuse before
writing". Issuing a grant did verify a keyed proof over the landing id, manifest, scope and term. The
runtime grant it produced, though, carried only the manifest hash, and the check at use (`Grants::check`)
compared only state, expiry and, for a single-use grant, that hash.

A plan's manifest names its entries relative to the target. So a create-only plan has one manifest in every
empty directory, and from every volume with the same content. Using an approved grant id, a caller could:

- land the same plan into another directory;
- land it into a directory that replaced the approved one at the same path;
- land it from another volume;
- land it as another consumer;
- with a session grant, land any plan on the owner shard.

Each of these writes to the host.

## Root cause

- `slates_land::grant::GrantRecord` had no binding fields, and `Grants::check` took only a manifest hash.
- The engine's `LandingRequest` carried no consumer, volume or snapshot, and nothing identified the target
  directory beyond its path string.
- `issue_grant` recorded the issuer's principal, and a session grant the issuer's session, where the
  durable record's documented field is "the principal it was made for".

## Impact

The P0 of the audit: the human disk-write permission boundary (R10) was weaker than the grant a human
approved. An approval for one landing authorized others within the grant's term.

## Exact edits

- `crates/land/src/grant.rs`: `TargetIdentity` (key, device, inode), `GrantBinding` (consumer principal
  bytes, volume, snapshot, target) and `BindingField`. A grant is issued with its binding. `check` takes
  the landing's binding and refuses `Unbound { field }` for another consumer, volume or target, and for
  another snapshot when the grant is single-use; the manifest comparison follows.
- `crates/land/src/engine.rs`: `LandingRequest` gains `consumer`, `volume` and `snapshot`. `land` builds
  the binding before the grant check, identifying the target by the host's `fingerprint_dir` of the
  directory it opened (device and inode). `Presented` returns the binding, so the grant a human approves
  binds exactly what was presented.
- `crates/server/src/landing.rs`: the request carries the caller's `Principal::key()`, the volume and the
  snapshot. The presentation keeps the binding in its awaiting record, and `issue_grant` binds the grant
  to it. The durable record names the consumer and the consumer's session. An `Unbound` refusal answers
  `GrantMismatch` and is counted `grant_unbound.<field>`.
- Callers: the test harness, the oracle, the bench and the unit test pass the presented binding.

## Evidence

- **Failing tests first.** `crates/land/tests/grant_binding.rs` failed with each wrongly bound landing
  answering `Ok` and writing: the retargeted plan, the directory replaced at the approved path, and the
  other consumer of a session grant. All three pass now. Each refusal names its field and leaves the disk
  unchanged byte for byte, and the approved landings, including a session grant's second landing with a
  new manifest, land.
- **The daemon.** `crates/server/tests/daemon.rs` `grant_scenario` refuses the same plan into a second real
  directory (`GrantMismatch`, the directory left empty) before landing the approved one. This check was
  added after the engine fix, to prove the wiring; it was not run red.
- **Suites.** slates-land 32; the server daemon tests 16.

## Siblings found, open

- **AUD-29-03 in part.** `session_of` maps every `Sid` and `Certificate` principal to session 0, and the
  landing lease's holder still uses that number, so such principals share a lease holder. The grant no
  longer does.
- **Lint.** The Windows lint of `slates-land --all-targets` fails on `examples/land_bench.rs` (unused
  imports and constants on Windows). This predates this change; CI does not lint that crate for Windows.
