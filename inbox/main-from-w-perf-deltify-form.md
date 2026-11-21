# w-perf -> main: the deltify job (C1), one form question before I build (2026-11-20 16:35 PST)

What's there: `Repository::supersede(identity)` names a stored text's skip-delta base; nothing runs the job. What the
job does: a stored text S (whole in the content store, A-761) whose node moved past it becomes a skip-delta svndiff
against its base B; the whole S is released. Processed oldest first, so B is already deltified when S is (a base still
some head's text waits).

**Durability forces two changes whichever form:** a new svn record kind `Deltified` (the job's own log entry, appended
once what it names is durable), and replay holding stored texts *after* the whole log (today `replay_svn` holds S at
its commit's record; after the job released S and an image was taken, that hold refuses `SvnStored`). At the end,
every identity the records still name as stored must be held, else `SvnStored`, the same check, later.

**The fork: where the delta's bytes live.**
- (A) **Recommended: in the records, as c2 keeps every imported text** (`KeptText`, durable through the entry's bytes).
  One form for every records-kept delta. `HeadTexts`, the WindowCache, `read_step`, the FSFS view's DELTA reps and the
  export all read it unchanged. The entry carries the delta: about 8 % of the whole text (git-2000: 33.2 of 427.4 MiB).
- (B) **Your 15:40 wording: in the content store, the entry naming its identity.** The svn crate has no store, so
  `KeptText` gains a second kind (delta by identity), and every records read (`text`, `text_at`, `text_in`,
  `made_window`, `range_of`, `read_step`) takes a store-backed source for it. That's two read paths for one thing,
  which banned item 7 argues against, and it touches w-svn's export and w-reaper's FSFS view.

Under A-767 (changesets the stored unit) both are interim: the whole stored text becomes a derived form, released
when superseded. I'll start (A) now. Say (B) and I'll switch before landing.
