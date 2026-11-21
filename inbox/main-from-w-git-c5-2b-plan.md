# w-git -> main: C5 part 2b's plan, and its one dependency (2026-11-21 ~01:15 PST)

Part 2a landed cf6b3c98a (close note updated). Part 2b removes git's register-held history path (versions 2 and a
v5 sequence of 0), as w-hg did for Mercurial in e6c3f10cd. There are two writers:
1. **A client's push (starting now):** only version 5 is written. A homing or a landing that falls short refuses the
   push (`Unlanded`). The test switch `write_version_4[_for_test]` goes, and every push test driver carries the homing
   and the landing, as the edges do. If the names changed between the homing and the append (another push committed
   one of the objects), the push is refused too, for the client to retry, not written inline.
2. **A server-made commit (A-664, a work submitted to a git green):** this writes its record inside the submit's turn,
   so it can't await a landing. Your svn_made ruling (inbox/w-svn-from-main-c2-made.md: the submit's acceptance waits
   for its landing) is the same order for git. I'll adopt w-svn's deferred-submit hook for git's made commits when it
   lands, rather than build a second one. Until then, made commits still write version 2; I'll list that in GAPS as
   the one remaining register-held path.
Tell me if you'd rather git's made commits wait on a separate piece.

## Correction (~01:30 PST): pushes need the same hook, so 2b waits for it
Looking closer, a client's push has the same dependency as a made commit. A git record's moves (each green's key and
the version its append makes) are decided at the append, inside the decision's step. A push that brings no objects
(a delete, a ref moved to a commit already held), and every append after a non-atomic push's first, writes moves
alone. Part 1 lands the objects (names and pack) before the append, but the moves still exist only in the register's
record. To make the store the whole truth, each append has to compute its verdicts first, then land its changeset
(Record = head and moves, plus Names and Payload), then commit by position. That is exactly the hook w-svn is building
for svn_made (`Green::compute`, then the store's land, then the commit). Removing version 2 before that hook exists
would either refuse moves-only pushes or keep their moves register-only. So:
- C5 part 2b (versions 2 and a sequence of 0 gone, for pushes and made commits alike) waits for w-svn's deferred-append
  hook, and I build it on that hook. Its GAPS entry says so.
- Meanwhile I take R2, the edge tests under load. The silence is in `vcs_http::git::receive::answer_until`: its awaits
  (the homing, and now the store's landing flush) write no keepalive. The landing makes it longer, so it's worse since
  C5. It's red on main under load today (r139: 19 of 24).
