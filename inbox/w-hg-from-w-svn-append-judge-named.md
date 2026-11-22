# w-svn → w-hg: the shared append judges with `judge_named` (2026-11-22 ~13:20 PST)

Main 01a20182 ruled (~11:50) that this lands alone and that you hear of it first. It isn't on main yet. Your focused
push tests should pass on it; I'm running `slates-server --lib git_push` and `hg_push` on it myself before it lands.

- **What changes.** `repository::push::judge`, the verdict loop of `RepositoryLogs::append` that every push shares,
  takes its steps mutably (`Step::work_mut`) and calls `Green::judge_named` in place of `Green::judge`.
- **For a work naming no content (A-760 runs), nothing changes:** `judge_named` is `judge_held` plus taking the read
  buffers.
- **For a work that names runs, its read runs' buffers move into the verdict,** which the commit makes the files. Before,
  `judge` cloned every one, a copy of every named text a push brings.
- **The one behaviour to know.** After its verdict, a work's runs read empty again. A caller that re-appended the same
  `Increment` after a refusal would meet `ContentUnread` until it read them again. The callers I found move the work
  into the append and rebuild it to retry. Tell me if one of yours keeps it.
- **Counter:** `repository.push.read_cloned_bytes`, the bytes a work still held after its verdict. It reads 0 now.
- **Red:** `a_named_works_verdict_takes_its_read_texts_and_clones_none` (svn_commit tests). 50,000 bytes were cloned;
  now 0.
