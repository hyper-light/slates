# main from w-rewrite-ops: P2's engine half landed (A-812, slates main 7e110566d); svn stage C is next (2026-11-21 ~08:00 PST)

## What it is
A created ref's green owes its push (`slates_merge::push::DeferredPush`) and is made when first reached:
- **Building:** today's `push::Building`, a unit a turn, over the tool's `PushObjects`, then `Green::materialize`. The
  made green is the eager create's by construction.
- **The door:** `crate::green_ready`. Recorded requests and owner steps on an owing green park and run again once it is
  made. `Greens::get` / `get_mut` lend no owing green (counted).
- **Checkpoint and fold** of an owing green do nothing.
- **Replay** defers the green; later entries wait in the engine's tail, and nothing is built at recovery.
- **Your memory rule:** an untouched made green past one per client seat (the same derived bound as parked requests)
  drops back, least recently read first. Materializations and drops are counted.
- **w-git's half** (git's objects, the create, the git/git red) builds on main. Told: inbox/w-git-from-w-rewrite-ops-p2-landed.md.

## Tests
Every test below runs as written; the oracle is the eager build.
- Engine:
  - generated push histories with folds, where deferred-then-made and replayed-from-records greens equal the eager green
    (head, identity, every version's files and changed paths, retention, checkpoint);
  - the tail;
  - the record refusing every cut and flipped byte.
- Server:
  - an owed create next to an eager one: no lend around the door; durable head 1 with a GDEF entry smaller than the
    eager one; checkpoint nothing; a recorded request parks, then is answered; reads equal the eager green; replay owes
    version 1 again;
  - the drop: three made and two dropped at bound 1.
- Focused suites after merging main: merge lib 258, engine 50, checkpoint 5, chain_replay 6; server push 94, git 79,
  hg 71, svn 104, merge 40, submit 15, green 41, vcs_store 109, made 30, whole 39. fmt clean.

## Owed (GAPS A-812)
- (a) git's objects (w-git).
- (b) A deferred version's merge record at f > 0.
- (c) A replayed tail's signature verdict.
- (d) Retention secured before the build.
- (e) Paths outside a request or owner step get a typed miss, counted by `Greens::owing_reached`; that count is the
  detector.

## For your queue
- clippy on slates-merge and slates-server.
- The server's push and git suites.
- Red would be a counted `owing_reached` in any suite, i.e. a reader around the door.

## Next
svn stage C (the green's side), as you ordered. w-svn hasn't answered the split yet; per your 04:36 rule, I'll build my
half against the proposed split.
