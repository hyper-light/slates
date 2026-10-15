# A relay answered only the last member that asked it about a target

**Found:** 2026-10-07, while measuring `an_indirect_probe_through_a_relay_keeps_a_peer_the_direct_path_lost_and_losing_both_paths_retires_it`
under load. **Status: fixed.**

## Description

SWIM's indirect stage has a member whose direct probe went unanswered ask `k` relays to probe the target. Each relay
forwards the target's answer to the member that asked (Das, Gupta & Motivala, DSN 2002 §3).

slates' member plane (`crates/cluster/src/member_plane.rs`) kept one relayed probe per target (`relaying`, keyed by
target). A second request about the same target overwrote the first. That happened in two cases:
- another member asked about the same target;
- the same member asked again for its next probe before the first relayed probe was answered.

When the target answered the overwritten probe, its nonce no longer matched the entry. The relay handled it as an
answer to its own probe, and the first asker was never answered. That asker then condemned a live member.

## Root cause

The relay table was keyed by target alone. memberlist, HashiCorp's SWIM and Lifeguard implementation, registers one
ack handler per relayed probe's sequence number in `handleIndirectPing`, so concurrent indirect probes of one target
stay independent.

## Impact

- Any fleet where two members suspect the same peer at once, which is the usual case when a peer's path from part of
  the fleet is lost, or where one member's consecutive relay requests overlap. Overlap grows as round trips stretch
  under load.
- Live members were falsely condemned and retired. Retirement triggers a takeover of their objects.
- The simulation's evidence: four members, where member 2 answers neither 1 nor 4 directly and member 3 reaches
  everyone. Member 3 relayed 2,796 probes in 30 simulated seconds. The askers received 2,635 answers (1,164 + 1,471),
  so 161 went to nobody, and member 4 condemned member 2.

## Fix

- `relaying` is keyed by `(target, asker)`.
- Each entry keeps up to `RELAYS_PER_ASKER` relayed probes, oldest first (`RelayedProbes`). The bound is hyper-swim's
  per-peer outstanding records (three: the probe that suspected, the probe that told, one extension). A fourth request
  about one target means the asker reused its oldest record, so that probe's answer can no longer be credited, and it
  is the one dropped. The table is bounded by the view squared times three.
- An answer is matched by `(target, relayed nonce)` and goes to its own asker (`take_relayed`).
- A retired member's entries go, as target or as asker (`remove_peer`).

## Tests

- `two_members_asking_one_relay_about_the_same_target_are_each_answered` (`crates/cluster/tests/member_plane.rs`): the
  four-member shape above. It failed before the fix (member 4 condemned member 2) and passes after it. It is
  deterministic.
- The server-level indirect-probe test: see its record in `docs/wip/GAPS.md`.

## Owed

hyper-swim's `OUTSTANDING` is private at the vendored revision (`41761ff`), so `RELAYS_PER_ASKER` restates its
derivation. hyper-raft's owner made it public the same day (`hyper_swim::detector::OUTSTANDING`, branch
`swim-outstanding-pub`, `7ea1c6c`, value unchanged). Done: slates vendors hyper-swim at `1b3b020`, and
`RELAYS_PER_ASKER` reads `hyper_swim::detector::OUTSTANDING`.
