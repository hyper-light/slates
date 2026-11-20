## 2026-11-19 ~00:05 UTC from main: bug 1 evidence (a_move_whose_target_dies_at_the_switch_is_served_by_its_successor)
Linux run r327 (adacachy, main 92c5a7c lint branch over 899aac162), log ~/.cache/slates-gate/l11-fleet.log there:
- line 62: "slates daemon: a volume 947c4bf74c5032a80000000000000006 this node moved away is released to member 10699510340234785448"
  (a released its frozen copy to that member id; which node that id is, a's hosts list in the test names: compare with c's member id).
- line 84: "fleet wait gave up: poll end fleet.rs:5246 verdict=period budget spent wall=485.143s advances=[4000, 4245]" (read_back's poll).
- line 92: final read "Refused { NotFound }". No line in the log mentions takeover/materialize/adopt for the moved volume.
- a's counters (fleet3-8282...): fleet.move.begun 1, handoff_begun 1, released 1, reported 1, frozen_refused 2374 (= overloaded 2374 =
  verb.overloaded_unrecorded 2374), not_found 13,068,430 (!), fleet.owner_location: round 61, no_session 60, refused_by_peer 61,
  unavailable 61, retried 57, budget_spent 2, gave_up 2, session_out 1; fleet.takeover: restarted 4, page_unbound 7, page_unmatched 2,
  pages_unsent 2; fleet.council.death_report_taken 3; fleet.link.session_judged_dead 2, stale_return 1.
- c's counters (fleet3-1268...): NO fleet.takeover.adopted, NO fleet.move.adopted, NO fleet.materialize.* at all;
  fleet.owner_location.root_view_differs 61; fleet.takeover: page_unbound 2, page_unmatched 7, restarted 2, round_behind 1,
  stale_configuration 2; fleet.link.invalid.member_changed 1; merge.content_put_refused 2 (the test's injected refusal before the move).
=> Neither survivor ever adopted or materialized the moved volume: c's takeover rounds were refused as stale configuration / round
   behind, and c's root view differed from a's at every owner-location round (61); a's own round found no session to the owner and
   gave up. The not_found count on a (13 M) is a's read_back poll meeting no owner.
macOS (quiet Studio 2768d0d5, r242/r277, main 3022c6894 and 2a5bf7264): same failure, last read HomedElsewhere {region 0};
  fleet.forward.batch_done 1,605,259 on a there (the reads forwarded to a home that was not the owner).
