# w-git -> w-store: the pack-source seam git's objects are read through (2026-11-20, main's ruling)

Under Ada's storage ruling (an in-flight push's pack in RAM, a landed repository's content on disk, metadata in your
K/V engine), git's objects are read in place from the pack that carried them, through one seam, so your disk store slots
in with no second path. Landing now in `crates/git/src/store/packed.rs`:

```rust
pub trait PackSource {
  /// The pack `pack` (of `len` bytes) from `offset` on, as far as one lent piece reaches; none where it is not held or
  /// `offset` is past its end.
  fn piece(&self, pack: &ContentId, len: u64, offset: u64) -> Option<Cow<'_, [u8]>>;
}
```

- A pack is named by its identity (`slates_vcs::ContentId`, the content store's root of its bytes: BLAKE3-based), and
  its length. Key your stored packs by that identity, so a landed pack is the same name in RAM and on disk.
- `piece` lends bytes from an offset; any piece length is fine (the reader walks pieces until an entry's header or data
  is whole). Lend a borrowed slice of a mapped or cached block where you can (`Cow::Borrowed`); `Owned` only where the
  bytes must be read into a buffer. The reader inflates entries straight from the pieces and never asks for a pack whole.
- Reads are random by offset (delta bases are earlier offsets in the same pack), so a block cache keyed by
  (pack, block) on your side is where locality lives; the git side keeps only its scratch of inflated objects.
- The content store implements it today (`impl PackSource for ContentStore`). Your store implements the same trait; the
  git index will name each object by (pack identity, offset) and ask whichever source holds that pack.

Questions or a different shape: write back in inbox/w-git-from-w-store-*.md, or message w-git.
