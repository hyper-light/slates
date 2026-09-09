# slates logo

Redrawn as native SVG on 2026-09-09. A single upright slate is divided into two
complementary pieces by a stepped open seam. The pieces suggest workspaces that
can diverge independently while retaining a common shape: the copy-on-write
model described in the unified design's glossary and Part 1. The angular cuts
and monochrome silhouette continue the sibling vorpal brand's visual style.

## Files and construction

- `slates-split.svg` is the editable vector master: two filled paths, 633 bytes.
- `slates-split-light.svg` and `slates-split-dark.svg` use identical paths in
  `#1f2328` and `#f0f6fc`. The README displays these SVGs directly.
- `slates-split-transparent.png` is a 1080 × 1080 export of the master.
- `slates-split-preview.png` shows both themes at 256, 90, and 28 pixels.
  All three sizes were visually checked on 2026-09-09.

The SVG viewBox is 90 × 90, matching its README dimensions. Horizontal edges
sit on whole-pixel coordinates at that size. The long edges move one unit left
for every four units down, and the stepped seam is six units wide. The two
outer corners have matching bevels. The background and seam are transparent.
There are no embedded bitmaps, filters, fonts, scripts, or external resources.

Reproduce the transparent export from the repository root with:

```sh
rsvg-convert --width 1080 --height 1080 \
  --output docs/assets/brand/slates-split-transparent.png \
  docs/assets/brand/slates-split.svg
```

The root README uses relative paths and a `<picture>` element for the two
color themes. Clicking the logo opens the preview.

## Previous concept

The generated stack of planes remains in git history at `112b5ed`. Ada flagged
its resemblance to Redis's old logo and its softness at README size. This
revision replaces the stack with one upright form and replaces the raster
wrappers with native vector paths.
