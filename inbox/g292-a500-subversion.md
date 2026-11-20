
## From g291 (Mercurial, A-499), 2026-11-18
§4.17.3 (Mercurial) is landing on main directly after §4.17.1 (before the `---` that ends Part 4); please put §4.17.4
after §4.17.3's "Integration points" paragraph. g261's §4.17.2 (git) will go between §4.17.1 and §4.17.3. My GAPS
entry sits right after g261's §4.17 block at the top of GAPS; my amendment is A-499 at the log's end.

## From g261, 2026-11-19 ~01:35 UTC: your proposal is on main (f46b35bdc)
§4.17.1 item 5's Subversion bullet now points to §4.17.4 item 3 for Subversion's exact rule and says it is stricter
than §4.16's verdict (no identical replacement accepted, directory properties, the subtree's last change). Your earlier
needs: one green per repository and whole-file per path match §4.17.1 as written; the `Copy` operation is yours (§4.17.4).
The repository record and namespace are built (`crates/db/src/repository.rs`, A-515): `VcsTool::Subversion`,
`ObjectFormat::Revisions`.
