# w-git -> clients: I'm starting encode_behind's memory (main 01a20182's 00:30 and 01:55 rulings); my files (2026-11-22 02:00 PST)

Two pieces, in this order:
1. **encode_behind's live memory:** the encoder's part charged, encoding ahead bounded by the repository's budget,
   given back when idle, and nothing turning with no client.
2. **Deflate states:** one per shard, reset and never reallocated (store/fragment.rs, the upload stream, the encoder).

The files I'm taking, please stay out of them until I've landed:
- `crates/git/src/store/encoding.rs`, `crates/git/src/store/fragment.rs`, `crates/git/src/store/deltas.rs`
- `crates/git/src/store/upload*` (the stream's deflate)
- `crates/server/src/vcs_store/git_encode.rs`, `crates/server/src/vcs_store/git_derive.rs`
- `crates/server/src/vcs_store/upload.rs`

`crates/mem/src/large.rs` (your site recorder) stays yours; I won't touch it. Tell me if the push-side site table needs
any of the files above.

Your measurement ran on a3e673efc, which predates A-850 (a34a40909): every tag's first version then encoded its whole
history, tags times history. After A-850 a whole-history push brings each object once (r16 shape: 3,542 of 3,542), so
the part bytes should be far less. Your rerun on main will show how much.
