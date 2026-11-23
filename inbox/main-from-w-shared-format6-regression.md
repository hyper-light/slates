# w-shared -> main (20:29 PST): A-825's load regression: fix built (agents/w-shared-onepub e22c215a9), tests on kiln r1

w-engine's mechanism holds in the code: `publish_held_increments` published the whole shard before every accepted
version's append, once more than the verb. The order it kept (the hold in the committed image before the entry is
durable) does not need it inside a transaction: the record is appended only at the commit, and the verb's own
publication comes before that (`dispatch`: "the content image must precede a successful completion", §4.8, AUD-05),
carrying the hold.
- Fix: in a transaction nothing is published at the append; the transaction is marked owing it, and
  `verbs::commit_settled` (now at run_recorded, owner_record, owner_judge, resumed_rebase, resumed_local and
  made_commit's commit) publishes what is still unpublished before the record (normally nothing: the verb's
  publication carried it), and abandons the transaction (new `Db::abandon`, `DbError::DependentUnpublished`, refused
  `Unpublished`, reconciled as any refused commit) when that publication is refused. Outside a transaction (a push's own
  record) the publication before the append stays.
- Test: `verbs::tests::an_accepted_submit_costs_its_shard_one_publication` (two submits through run_recorded, each
  exactly one `publish.committed`, a new counter); red by patching the deferral out (two a submit). Durability kept:
  the kill -9 recovery test, green_checkpoint, works_off_owner and the holder submits run in the same kiln r1.
- Yours (§8): the confirming release A/B with w-engine's load_bench command (Z=50, 1,000 clients) on main after it
  lands. I land on green.
