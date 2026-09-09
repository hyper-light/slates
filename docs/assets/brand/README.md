# slates logo

Created 2026-09-09 with the built-in image generation tool. The angular planes represent
independent copy-on-write workspaces over a shared base (the unified design, Part 1 and R1).
The open gaps suggest separation in memory; the stepped cut gives the top plane its signature.
The monochrome silhouette follows the sibling vorpal mark, with geometric ornament for slates.

- `slates-planes-transparent.png` is the original generated artwork, including its alpha channel.
- `slates-planes-light.svg` and `slates-planes-dark.svg` embed that PNG unchanged. Their filters
  use vorpal's theme colors (`#1f2328` and `#f0f6fc`); their shared viewBox frames the silhouette.
  These are presentation wrappers around raster artwork, not traced vector paths.
- `slates-planes-preview.png` shows both SVGs at 256 pixels and the README's 90-pixel size.
  It was rendered with the installed `rsvg-convert` on 2026-09-09 and visually checked.

The root README uses relative paths and a `<picture>` element to select the matching theme.
Clicking its logo opens the preview. No external image host or font is required.

## Generation prompt

```text
Use case: logo-brand.
Create one finished futuristic logo for "slates", a Rust copy-on-write virtual filesystem for coding agents. Its core ideas are independent in-memory workspaces, shared immutable base, instant copies, and controlled landing onto disk.
Visual concept: a compact architectural emblem of three precise angular slate planes hovering above one another in axonometric/isometric perspective. The lowest plane is the stable base. A middle plane and top plane share its footprint but are detached by generous negative-space gaps. Slight forward offset gives energy and shows independence. The top plane has a single bold inset stepped cut, a precise geometric signature suggesting an S or split workspace, without rendering a literal letter. All contours carefully aligned, sharp beveled corners, graceful proportions through geometry, like a future-machined insignia. Strong black silhouette, striking engineered negative space. Ownable and sophisticated, not a generic database stack icon. The sibling vorpal brand has a black ornamental blade silhouette: preserve its graphic restraint, striking silhouette and beautifully cut negative spaces, but use straight architectural cuts for slates, with no flowing ornament.
Color: pure flat black ink. Genuinely transparent background and transparent cutouts, antialiased edges. No white fill, no shadows, no gradients, no texture.
No plants, branches, sprigs, leaves, floral ornament, ribbons, calligraphy, swashes. No text or wordmark. No computer chip, circuits, server rack, cloud, folder, cylinder or knife.
One centered emblem only. Square canvas with modest transparent margins; the emblem fills around eighty percent of the canvas. Recognizable and legible as a small 90-pixel-high README mark.
```
