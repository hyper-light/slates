# w-sessionlog -> main: A-782 landed (2026-11-20 ~16:30 PST)

- LANDED ea730edb8 (merge c9be876f1 on origin/main, merge-base confirmed).
- Identical vs accepted, one typed outcome: engine Outcome::Identical { version } = an accepted increment whose verdict
  left no effect, committed exactly as an accept (chain/replay/retry unchanged; holders' replay checks in
  merge_origin.rs and merge_service.rs take either). Reply field `identical` (appended to Submitted, SubmittedSigned)
  read from the green's change index (Green::changed_nothing_at, above folded_below) so first/deferred/resent answers
  agree. Client Submitted::Identical, api MergeOutcome::Identical, MCP/CLI/Node/Python `identical`, log WorkMark::Identical.
- Red first: engine check disabled -> new test got Accepted(3) for Identical(3); green with it.
- Compiler found every site; workspace `cargo check --tests --examples` clean on macOS. Tests whose case was identical by
  construction now expect Identical (merge tests engine.rs x7, node_exclusive x2, push 1; server mounted_work, merge_load);
  oracles/drivers take Accepted|Identical as committed.
- Finding (one line, not chased): writing the head's own bytes with nothing intervening is still Accepted (the engine
  applies a no-op effect) -- client/tests/merge_identity.rs calls that "identical". Say if you want it Identical too.
- Run requests: Linux/Windows cfg code wasn't compiled here (sdk/server cfg arms touching Submitted); clippy on the
  touched crates. Also: `cargo fmt -p slates-server --check` fails on main in crates/server/src/repository.rs:1554-1566
  (not mine).
- Next: item 3 (ask-policy held merges in the log).
