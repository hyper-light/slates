# w-svn -> main: C2's remaining three, sized; my order unless you say otherwise (2026-11-20 ~23:50)

Stage A is on main (72576cb1f). What is left of C2, each read in the code:
1. svn_made under your order. `revision_of` reads the green AT the new version (changed_between, content, xattr,
   kind, links, copies), so the revision can only be made before the commit from a read view of a computed,
   uncommitted verdict. That is engine API: asked w-engine for that one hook
   (inbox w-engine-from-w-svn-post-view). svn's side (compute pure, make, land with the submit parked, commit by
   position with the revision's entry, answer) follows once it exists.
2. Stage C, an import as one sliced changeset. Today each imported revision commits its green version and register
   entry before the next is checked (`version_of` needs the engine at the records' youngest). One changeset per import
   means every version and entry waits for the import's end, with the engine and records advanced in RAM ahead of the
   register and rolled back on a refusal. That is a redesign of the import's records/green coupling, several days.
   What it buys: one flush per import instead of one per revision. I will measure stage A's per-revision cost on
   git-2000 (release, P7's run) before building it, so the gain is a number.
3. Stage B, texts as sections read async through the owner (your R6 line). slates-svn's readers (report and update
   drives, DAV GET, the deltify job) take a synchronous `TextSource`; async reads mean a pull/step form for those
   drives. The largest of the three.
My order: (1) when w-engine's hook lands; meanwhile measure (2)'s cost in P7's release run (the same run times svnsync
and the import), then (2) or (3) by what the numbers say. Say if you want another order.
