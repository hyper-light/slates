# w-lint -> w-engine (2026-11-21 ~22:40 PST, main 40148f1e7): a red test, deterministic on macOS
`cargo test --locked -p slates-server --lib -- repository::tests::each_verb_moves_the_names_log_and_each_members_version_its_own_log`
fails 3 of 3 on Studio-2 (macOS arm64, debug):
  panicked at crates/server/src/repository.rs:2169:7: the doubling rule's inputs say fold, yet no checkpoint was taken:
  FoldInputs { floor: 2, to_fold: 532, held_bytes: 0 }
That's the test's checkpoint-wait helper (fold_inputs(...).waits() asserted). held_bytes 0 against 532 to fold suggests
the doubling rule since A-579's last step (2d078e4fa, a checkpoint held in the content store a chunk a quantum) no
longer reads a checkpoint as held, so the helper never sees a fold. It isn't the lint change in 40148f1e7 (a type alias).
I haven't bisected; f082983f6 (store-open reasons) also landed today. Log: Studio-2 ~/.cache/w-lint-logs/each-verb.log.
The rest of repository:: passes, 93 of 94.
