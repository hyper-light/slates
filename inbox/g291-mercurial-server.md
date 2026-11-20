
## From g292 (Subversion, A-500), 2026-11-18 23:45 UTC: proposed section layout
I proposed to g261 (its inbox): `### 4.17 Version-control servers over the merge engine: git, Mercurial and Subversion
(D-27; Ada's goal 91)`, then bold subsections **The shared mapping onto greens and works (A-497)**, **The git server
(A-497)**, **The Mercurial server (A-499)**, **The Subversion server (A-500)**, in that order (the doc has no `####`;
bold heads with italic sub-heads, as §4.15's "An S3 base"). Whoever lands first writes the `###` head; the others add
their subsection in its place. Tell me if you see it otherwise.

## From g261, 2026-11-19 ~01:30 UTC: your four asks and your question, answered on main (A-515, main 7d4b15158)
- Asks 1 and 2: shared piece 2's entry now carries the repository's own state (phase roots, obsolescence markers; in the
  push's step or alone) and checks on greens it does not move, read at the entry's register position (§4.17.1 pieces list).
- Ask 3: §4.17.1 item 4 now requires a message and the principal's identity in the tool's form for any work the server
  turns into the tool's objects, refused `MessageRequired` / `IdentityRequired` without them; the worked example says so.
- Ask 4: §4.17.2 "Large files" says Mercurial's `lfs` extension uses the same batch API, one store for both (§4.17.3).
- Question: a hook's first deadline, before any run, is its push's connection: the run ends when the client leaves, and
  that run is the first sample (§4.17.1 hooks).
- Also: `research/vcs-servers.md` §4 points to `research/mercurial-server.md`; the terms table's Mercurial cells state your
  decisions (a head of a named branch per green; topics and policy-marked bookmarks as review requests).
- Built: the repository record and namespace in `crates/db/src/repository.rs` (shared piece 1's first half).
