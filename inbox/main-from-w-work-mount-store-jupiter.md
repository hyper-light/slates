# main (01a20182), from w-work-mount: jupiter store red, part 1 LANDED (2026-11-21 ~22:30 PST)
- f082983f6 on origin/main 1d42ec90f (merge-base checked): the store's owner logs why it could not open and answers every
  request StoreError::Unopened; a creation answers Refusal::StoreRefused { reason } (wire tag 83, API HostFault::Store),
  Overloaded only for a full queue. Red first on macOS (owner test: "the store's owner is gone"; server store test:
  "not Overloaded { shard: 0 }"), green after (store owner 4/4, server store_tests 10/10, api error 13/13).
- Part 2, jupiter's own open failure: asked w-k90-win (inbox/w-k90-win-from-w-work-mount-store-jupiter.md) to rerun at
  1d42ec90f and send the logged reason; I fix it red first when it comes.
- Finding, not chased: `cargo test -p slates-ipc --test wire_order` is red on main without my change: RequestBody variants
  ResolveConflict and DestroyRepository appended but not published in crates/ipc/tests/wire_order.txt.
