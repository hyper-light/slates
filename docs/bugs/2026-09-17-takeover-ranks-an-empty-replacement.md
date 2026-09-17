# Takeover ranks an empty replacement using a new copyset

Date: 2026-09-17. Design: §4.8 Promotion and takeover, D-14, AC-8.1/AC-8.18.

## Evidence and cause

Ubuntu job [105312670699](https://github.com/hyper-light/slates/actions/runs/35253899553/job/105312670699)
spent its fleet period budget waiting for the first-ranked survivor to place the old owner's head in
`a_restarted_peer_is_learned_on_contact_under_its_fresh_identity`. That fixture waits until both
survivors hold the sealed head before killing the owner; the replacement has no copy.
Three initial local Linux samples passed. Those passes did not exclude an ordering-dependent fault.

`FleetNode::install_configuration` called `Routing::take_over` with the **new holder's neighborhood**.
Routing recomputed the dead owner's copyset from that input. A membership update admitting an empty
replacement could therefore assign the old record to that replacement. It had no routing entry or
held acceptor, so no node drove the assigned takeover. The same computation was wrong above the
candidate floor, where a holder and the record's owner have different neighborhoods.

The bounded public-core history is deterministic:

```
cargo test -p slates-cluster --lib a_fresh_member_cannot_displace_a_surviving_holder_during_takeover -- --nocapture
```

Before: fails in 0.00 s, choosing HostId(4), the empty replacement, instead of surviving HostId(1).
It covers admission before retirement and both changes observed in one configuration update.
This establishes the mechanism; the original CI log did not record its chosen routing owner.

## Fix

- An accepted record retains its owner's candidate set, placement generation and quorum in the
  existing local routing entry. The set is bounded by 2f+1; it is allocated only on the first
  acceptance or a changed placement generation, and removed with the held object's route.
- Takeover ranks only remembered candidates still in the committed region membership. Simultaneous
  retirements are filtered together. Later joins cannot turn an unheld copy into a recovery candidate.
- Phase one gathers promises from that original candidate set using its original quorum. Only after
  recovery does the successor commit the adopted record under its current placement. A completed
  adoption stores the exact candidate set captured for that round, not a configuration read afterward.
- Acceptance refuses a nonmember owner, and a record whose generation differs from the council's
  placement, before effects. Stale-holder and stale-sender refusals remain distinct. This prevents
  attaching a newer neighborhood to a record accepted during the council-to-placement install gap.
- The authority regression's successor is now an actual configured survivor. Its old fixture directly
  installed an authority for a node absent from the council; the strengthened admission correctly
  rejected that fixture. Forged transport principals remain refused before raising the promise.

Sibling sweep: the holder's own-neighborhood error, simultaneous retirements, and phase one's use
of the new quorum are fixed here. The broader generation-transfer contract in §4.8/AC-8.18 remains
subject to its existing independent protocol evidence; these histories do not close that whole criterion.
Cross-region forwarding still guesses a successor from all alive home-region members in
`verbs::owner_in_region`; it needs authoritative owner discovery and is a separate routing defect.

## Validation

2026-09-17, macOS arm64, Rust 1.98.0: deterministic regression passes in 0.00 s; 146 cluster unit
histories and three transport membership/promotion histories pass in 0.02 s (unit) and 0.01 s per transport binary.
The server suite passes 89/89 in 18.78 s. Strict cluster/server clippy passes in 5.92 s.
Linux aarch64, cached Rust 1.98 compiler image, uid 1000, four CPU quota, no network/capabilities:
the original restart history passes twice (full bounded invocations 7.40 s and 6.49 s), and the
five-node f=2 takeover passes in 10.59 s. Logs remain in the session scratch directory.

Final Linux source: `a_takeover_successor_serves_the_dead_owners_content_over_nfs` passes
in 11.37 s; `a_whole_ram_replacement_joins_as_a_fresh_voter_and_commits_after_another_loss`
passes in 9.35 s. Both use `cargo test -p slates-server --test fleet <name> -- --exact --nocapture
--test-threads=1` (prebuilt binary in the bounded container). No timeout, period budget or assertion
was weakened. The wide-history fixture first selected a host outside the owner's copysets and its
non-vacuity assertion failed; it now selects an actual candidate before accepting records.
