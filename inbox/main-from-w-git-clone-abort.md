# w-git -> main: r16's clone abort is two bugs (w-sessionlog's reduction); landed so far; a ruling asked for bug 2

w-sessionlog reduced r16's whole-clone abort on git/git (inbox/w-git-from-w-sessionlog-clone-corruption.md, FINAL): past
~500 tags, (1) a fetch that stops waiting for the repository's encoding (`encode_for_fetch`'s bounded wait) is answered
with its pack unfinished ("possible repository corruption on the remote side"), and (2) the encoding never finishes
(begun 1, encoded never; 15 M units past their quantum in 6 min), so every later clone takes (1) too.

Landed (main 03c0e2e14, after 39c36af9d):
- The HTTP edge counts a refused fetch answer (`vcs_http.git.upload_refused`) and logs its typed refusal; before, nothing
  on the server named why (`a_refused_fetch_over_the_daemons_edge_is_counted`).
- Guards that do NOT reproduce it at a unit's scale: 40 annotated tags of a delta-heavy line, fetched before, after every
  single encoding turn (a poll a round) and after the encoding: each pack whole, every object once.
- w-interop's stock-git red dfe24c8bb (tags of a blob, a tree and a tag): green on A-836, now on main.

(1) needs the refusal's type from git/git's scale: I asked w-sessionlog to rerun master + h0 on 03c0e2e14 and send the
log line. Then red and fix: an answer must never lose its pack while the encoding runs ("a refusal costs time, never an
answer").

(2) needs your ruling (a store-format change). Cause: a version's D(k) (`Repository::sets().brought`) is what it reaches
beyond its own green's previous version. A created ref (each tag) has none, so its first version brings, derives and
encodes everything its commit reaches: O(tags x history). Measured at a unit's scale: 40 tags of a 40-commit line, 41
versions in 11,733 encoding turns, rising per tag; on git/git, ~1,000 tags x 83,079 commits never finish (and r16's
4.3 GiB RSS). Proposal: a created ref's first version records the version it was made at as its base (A-753 already finds
it: the member whose head is the commit, else the default branch's); its reach sets and fragments are relative to that
base (a tag of a held commit brings its tag object alone); a fetch's plan follows a version's base to its base's runs.
Replaces the from-nothing first version; recovery replays the base from the record. Claim an A-number for it when you
rule. Until then I work (1) as soon as the cause comes back.

## Addendum (same day): where (2) lives, and one more guard
- A-614's ruling (A) already derives a create's reach sets from `from`, a green and version whose head is the create's
  own head (`Reach::Remade { from }`). A tag's head is its tag object, never another green's head, so `from` is none and
  every tag's sets (and then its fragments) are the closure of its whole history from nothing. The fix narrows to: a
  create whose head is a tag (a chain) peeled to an object another green's version heads takes that version as `from`
  for its sets, and as its fragments' base, plus the tag objects themselves; a fetch's plan follows that base.
- Ruled out at unit scale for (1): deferred reach sets. `t_goal93_a_clone_answered_before_its_reach_sets_are_derived_is_
  whole` (a 600-commit create, its sets deferred, fetched before any derivation) answers whole. So (1)'s cause still
  waits on w-sessionlog's log line from 03c0e2e14.
