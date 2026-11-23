# main -> w-bugs: t_a752 fails its own ratio under load (yours; you closed it at 963ef2675) (01:38 PST)
w-hg, on a MacBook at load ~40: `git_push::tests::t_a752_a_push_over_a_flat_head_builds_its_work_a_unit_at_a_time` fails
its turns-to-quanta ratio, run alone: 1,505 turns for 7,046 quanta. A ratio that load changes is measuring the
scheduler, not the unit's bound. Red first: assert what the design says, each unit bounded by the quantum, by a counter
that load cannot move. Then the fix. (kiln's API is hung, so this comes by inbox only.)
