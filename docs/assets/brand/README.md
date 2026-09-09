# slates logo

Redrawn as native SVG on 2026-09-09. The mark keeps the floating tablets and
stepped cut of the original concept. One working tablet lifts diagonally from
its source, with a slightly different orientation. The pairing refers to the
copy-on-write model in the unified design's glossary and Part 1: a workspace
can diverge while its source stays intact.

## Files and construction

- `slates-tablets.svg` is the editable monochrome vector master.
- `slates-tablets-light.svg` and `slates-tablets-dark.svg` use the same geometry
  in `#1f2328` and `#f0f6fc`. The README displays these SVGs directly.
- `slates-tablets-transparent.png` is a 1080 × 1080 export of the master.
- `slates-tablets-preview.png` shows both themes at 256, 90, and 28 pixels.
  These sizes were visually checked on 2026-09-09.

Each tablet is one filled outline that includes its thin front edge. The upper
one is offset and turned relative to the source. A vector mask clears three
design units around it, separating the tablets wherever they overlap. A second
vector mask cuts the 5.5-unit stepped opening through the upper tablet. Both
masks use black and white regardless of the artwork's theme color.

The background and cutouts are transparent. The assets contain no embedded
bitmaps, filters, fonts, scripts, or external resources. The 90 × 90 README
presentation uses the vectors; the PNG is an optional export.

Reproduce the transparent export from the repository root with:

```sh
rsvg-convert --width 1080 --height 1080 \
  --output docs/assets/brand/slates-tablets-transparent.png \
  docs/assets/brand/slates-tablets.svg
```

The root README selects the theme with a `<picture>` element and links to the
preview. All paths are relative to the repository.

## Earlier concepts

The generated three-plane stack remains in git history at `112b5ed`. Ada liked
its tablets but flagged its resemblance to Redis's old logo and its softness
at README size. The upright split-slate revision at `63356ec` removed too much
of that character. This version restores the tablets as a source-and-copy pair
and retains native vector rendering.
