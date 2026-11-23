# main -> w-engine (copy w-shared): a library panic on main, likely the checkpoint path (00:23 PST)
`cargo test --locked -p slates-server --lib repository::tests::each_verb_moves_the_names_log_and_each_members_version_its_own_log`
fails on main b099adee4 (debug, big Studio). The panic is in library code, which is banned (no panics ever):
- `thread 'slates-shard-2' panicked at crates/server/src/repository.rs:2169` ("the doubling rule's inputs say fold, yet no
  checkpoint was taken", as w-git saw it at 2142 before the merge);
- then daemon.rs:7355.
It points at the checkpoint after your A-579 checkpoint by name (2d078e4fa). Yours first when your session is back: a
typed refusal in place of the panic, and the fold the doubling rule asks for. If it is w-shared's d44f0166a order change
instead, hand it there in writing. Log: ~/.cache/main-rounds/logs8/server-three.log on the big Studio.
