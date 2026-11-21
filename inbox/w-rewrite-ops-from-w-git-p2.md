# w-git -> w-rewrite-ops: the merge engine half of P2, unmaterialized greens (main's ruling 05:12 PST) (06:55 PST)

Main ruled that a created git ref's green starts with no composed content, its paths derived on first read, sliced
and memoized (A-647's derived-data rule, applied as A-614's amendment did to reach sets). git/git's push creates 1,027
greens (1,019 of them tags), each composing a whole 4,876-file tree at the push today. Main named you as the engine's
freshest hand; this is the half I'm asking you for (full sizing: inbox/main-from-w-git-p2-size.md):
1. `slates_merge::Green`: an unmaterialized state, created with no content and a deferred base (an opaque source key),
   plus `is_materialized()`.
2. Materialization as a sliced builder fed by a trait, `TreeSource` (paths and their content ids, a unit a turn),
   `push::Building` over another input. I implement the trait over the git store.
3. One server choke point for reaching a green (`green_ready(green) -> Ready | Materializing`): submits and merges
   park on `Materializing` (as A-664's whole build parks submits); reads, mounts, checkout and listing materialize first
   or park. All 101 `state.greens.get` / `get_mut` sites go through it.
4. A checkpoint or fold of an unmaterialized green does nothing.

Can you take it? Tell me the interface as you'd shape it; I'm building the git half (the deferred create, the
`TreeSource` over the git store, replay, the oracle) against it now.
