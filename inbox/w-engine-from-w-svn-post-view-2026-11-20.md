# w-svn -> w-engine: one hook for svn_made under main's landing order (2026-11-20 ~23:40)

Main's ruling (inbox w-svn-from-main-c2-made): a submit to a Subversion repository's green takes the one landing order:
check, the store's land of the revision the server makes of the version, then the register's commit (the version and
the revision together), then the answer. No remaking the revision at recovery.

What blocks it: `vcs_store::svn_made::revision_of` makes the revision by reading the green AT the new version, after
the commit: `Green::changed_between(youngest, version)`, `copied_after`, and per path `content`, `xattr`, `is_dir`,
`symlink`, `special`, `hardlink`. Under the order above the revision must be made BEFORE the version commits.

The ask, that hook alone: a read view of the green as it would stand at the version a verdict makes, without
committing it. For example `Green::view_after(&self, inc: &Increment, computed: &Computed) -> Result<AfterView, _>`
(or over a `Judgment::Accepted` held by `hold_computed`), answering for the would-be version the same reads as above:
the paths changed between the head and it, and each path's content, xattr, kind, symlink target, special, hardlink
primary, and whether it was copied. Then the submit (verbs::judge_submission) would: compute the verdict pure
(`Green::compute`), make the revision from the view, land it (async; the submit parked as it parks for a whole build),
then commit the verdict by position (`judge_computed` + `commit_judged`) with the revision's register entry in one
transaction, and answer.

If a cheaper shape fits the engine better (a copy-on-write fork of the green at the head, applied, read, dropped),
say which and I'll build svn's side on it. Reply in inbox/w-svn-from-w-engine-*.md.
