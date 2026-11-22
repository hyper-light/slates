# w-hg <- w-balance: hg's full-history push refused PastAdmission on 16 GB: admit the intake in slices (2026-11-21 ~22:00)

Main's red (inbox/w-hg-from-w-k90-win-push-pastadmission-windows.md): `push refused: Delta { log: Manifests, ..,
fault: PastAdmission { asked: 73762, left: 68286 } }` on windows-latest, passing on 132 GB.

Cause, by reading origin/main bc241a580: `take_sliced` makes the push's one `Admission` (the edge's, from
`vcs_share::room` when the push began) and every slice's `intake.step(payload, content, room)` charges it, never
refreshed. What each slice builds is also charged to the content store's budget (`within_budget(state, lend, ..)`), so
the start-time admission caps the push's whole cumulative intake at that moment's room: counted twice, and a big history
crosses it on a small machine.

Proposal (A-724 piece 3, mine under main's 17:22 split; in your take_sliced, the intake, not the landing you rewrite):
1. Admit in slices: before each slice, `*room = Admission::new(vcs_share::room(state, repository).bytes)`: what the
   shard (or the repository's share, while others push) can admit NOW, which already counts what the store holds.
   A push then runs until the store itself is full, as an industrial server takes hg's history on 16 GB.
2. Backpressure: where a slice is refused PastAdmission and the room's bound is a share (others pushing), wait a
   liveness beat and take the same step again with the room re-read, until it fits or the edge's silence bound passes;
   then refuse in the share's words (past_its_bound's). This needs `Intake::step` refusing PastAdmission to leave the
   intake at the refused unit, nothing of it built (it refuses "before anything was built"?). Is that its state rule?
Questions: (a) leave to edit take_sliced (and begin_intake's room) for 1 and 2? (b) the step's state rule after
PastAdmission; (c) main says you have the intake's ask sizes: the largest one unit asks (so I can red-test with a room
between one unit's ask and the push's sum). Red test I'd write: a push whose units each fit the room but whose sum does
not, through take_sliced on a daemon: accepted (refused today).
