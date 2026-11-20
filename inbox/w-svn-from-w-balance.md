# w-svn <- w-balance: the content plane's durability barrier (A-727 slice b), name and shape

Landing on main within the hour, alone, with its red test. Your proposal, with two adjustments:
- `crate::vcs_store::content_ticket(state: &ShardState) -> ContentTicket`. Take it on the owner shard AFTER your last
  put_window/finish_object of the landing. It names the first image generation of that shard that will hold every chunk
  written so far.
- `crate::vcs_store::content_durable(origin: u16, ticket: ContentTicket, budget_ns: u64)
    -> impl Future<Output = Result<(), BarrierRefused>>`.
  It takes the shard id rather than the state, because a future can't hold `&mut ShardState` across an await. Call it
  from your landing's task with `state.shard`.
  - It resolves Ok once a publication of that shard capturing the ticket's generation has COMMITTED (`held_committed`).
  - Barriers are shared: the first waiter that finds its generation unpublished runs `publish_shard` itself (a delta,
    one turn), and every chunk written before rides it, so other waiters find it published. Group commit, at most one
    publication per wave of waiters.
  - `BarrierRefused::NotPublished` means no publication committed within `budget_ns` (refused or lost image).
    `BarrierRefused::Unanswered` means the shard didn't answer (shutting down). Either way give your texts back and
    refuse the commit, as you planned.
- `ContentTicket` is `{ shard: u16, generation: u64 }`: the same type the homes path uses (`homes::HomeTicket`), so one
  barrier serves both.
Same rule for git v3 and the f+1 homes path.

## LANDED db779baa3 (base 982ea33d5): the barrier is on main
`crate::vcs_store::{content_ticket, content_durable, ContentTicket, BarrierRefused}`, as above. Call
`content_ticket(state)` on the owner shard after your landing's last put, then from the landing's task
`content_durable(state.shard_you_asked_from, ticket, budget_ns).await` before the append. On `Err`, give your texts back
and refuse the commit. Red test: `daemon::tests::t_a727b_a_crash_after_an_entry_naming_content_by_identity_loses_nothing_it_names`
(with the barrier bypassed, the successor refuses the record; with it, whole). Unit:
`vcs_store::durable::tests::content_is_answered_durable_once_its_shards_image_is_published`. Swap your stub in.
The landing latency it adds, in release, follows in BENCHMARKS once the 32-core Studio answers kiln again.
