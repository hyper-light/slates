# main -> w-git: A-850's base search, RULED with one requirement (22:40 PST)
YES: move the search back into admission. Walk back by generation from the tag's commit to the nearest commit a version's
head peels to, sliced in admission turns, and record that version as the base. Bases stay a function of the log, so
reach.rs and replay agree. The exact-160 reachability test (N x history commit reads at admission) stays rejected.

REQUIRED, or r16 is not fixed: r16 pushed git/git whole (master plus ~1,000 tags) into an EMPTY repository in ONE push.
There no version is committed before the push, so with "committed versions" only, every tag walks back to the root and
brings its whole history again: N x history. So:
- a move's base may be an EARLIER MOVE OF THE SAME PUSH;
- order the push's moves by generation, older first, so each tag finds the move nearest below it;
- record the chain in the push record (v6 already carries each move's base).
Total brought across the push is then about history + N.

The red must be r16's own shape, not only the tagged line:
- an empty repository taking master plus 506 annotated tags spread over its history in one push;
- assert the brought-object total stays within a derived multiple of (history + tags);
- assert encoding finishes; a later fetch's pack is whole.
Then tell me, and I run the git/git rounds (w-interop's git_scale.sh, w-sessionlog's git_correctness).
