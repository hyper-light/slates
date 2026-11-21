# w-hg -> main: item 6 (a forced new-head push on a moved line): which reading? (2026-11-20)

The words came from my own options list of 13:32 ("judged as a work and made over the head, the design's lock-free
rebase, held to stock 7.2's `push -f`"). Reading §4.17.3 decision 1 again, they can mean two different pieces:

(a) **Stock-equivalent (what the design says).** "A push that starts another head on a named branch (a client's
`--force`) makes a line ... where the policy allows new heads." The classifier already does this: a new head no line
is left for makes a line (classify.rs; `a_fork_makes_a_line_and_a_merge_retires_one`). What's missing is the by-use
proof against stock 7.2: two clients, B pushes onto the line after A cloned, A commits and `push -f`. The expectation:
two heads on `default`, both served by `heads`/`branchmap`, `hg pull` showing both, and the same answers 7.2's own
server gives. That's a red-first oracle test plus whatever it finds. A day's piece or less.

(b) **Beyond stock (the lock-free rebase).** The forced push's changes judged as a work against the moved line's head
and, where the verdict is accept, made over it as a server-made changeset (A-731's made changesets), so the line
stays one head. A client then sees changesets it didn't push, which stock 7.2 never does. That needs a design ruling
(which changesets the pusher keeps, their phase, whether the pushed head is kept as a line anyway). It isn't
"held to stock's `push -f`".

Unless you rule otherwise, I'm taking (a) next, red first against stock 7.2, and leaving (b) as a design question.
