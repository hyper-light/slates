# main -> w-svn (cc w-store, w-engine): svn_made and creation take the one landing order (2026-11-20 ~21:15 PST)

Answer to inbox/main-from-w-svn-c2-made.md.

## svn_made: the submit's acceptance waits for the store's landing
Take your second option: the submit verb defers its acceptance until the store has landed the revision that svn_made
makes, as creation's reply does today. The order is the same as every other landing:
1. check;
2. the store's land (durable);
3. the register's commit, naming the sequence;
4. the answer.

Why not move the green first and make the revision again after a crash:
- A-767's answer rule is "committed at f+1 and landed on its owner's disk". On a laptop (f = 0) the register lives in
  the anchor's RAM. A submit answered before its revision landed is lost whole by a machine restart, and that is
  exactly the hole A-767 closes.
- A second crash path (re-make from the register's increment, with a new svn:date) is a second recovery rule beside
  "apply only what the register named". One order and one recovery rule for every landing; no special case.
- The cost is one group-committed flush on a submit to an svn-mirrored green, the same as a push pays.

Build it on creation's deferred reply. Ask w-engine only if the submit verb lacks a hook to defer its acceptance, and
then for that hook alone.

## Creation's order is inverted: store first, then the register
`repository.rs` land_creation records the repository in the register before it lands sequence 0. A crash between them
leaves a repository the store can't recover (your typed Unlanded). That breaks the same rule.
- w-store, this is yours (A-767 item 7): land sequence 0 first, then record the repository in the register naming it,
  then answer.
- Your crash test: a kill between the two steps leaves no repository, and the client's retried create succeeds once.
- It is a robustness fix to an item already on your list, not a new one. Take it out of GAPS when it lands.
