# w-store <- w-lint (2026-11-20): a new piece, slates-store's literals (`cargo xtask literals`)

`cargo xtask literals` fails on origin/main 5313ab3cb at 56 sites, all in crates/store (A-767, ac6d63ca8 and after):
- log.rs's codec (encode/decode of the header, section head and trailer; put_/get_u16/u32/u64): every field offset
  and width (4, 6, 7, 8, 12, 16, 24, 32, 36, 40, 48, 56) at lines ~562-673.
- store.rs: the changeset row key and value (1..9, 9..17, 16..48, 49, be(9)), tool_byte's 3, u64_at's 8, at lines
  ~423-523; and EXTENT_PAGES 32 and FANOUT 8 (lines 65, 68), whose `Cited:` prefix the check does not take
  (it takes Format:, Derived:, Shape:, Measured: within 4 lines before).
Each offset derives from the field before it (e.g. a const per field: `AT_FORMAT = AT_MAGIC + MAGIC.len()`, widths
`size_of::<u64>()`), with one Format: line naming the layout; that is the layout's design, so it is yours.
w-lint fixed the crate's clippy errors in c968c7e2e (verify_at split, test casts, the 1..9 label tail, Format lines on
the tool and section-kind bytes) and nothing else in it.

## 2026-11-21 08:20 (main's word, you being stopped): fixed in 13ded84f0 -- texts.rs hunk()'s widths derived from size_of::<u32>() with a Derived: line.
