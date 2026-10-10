# Theme artwork

Three character themes for the window, each a standing character and a faint ornament for the
right edge, both 1024 × 1536 RGBA PNGs with a transparent surround:

- `ember/` Ember, a dragon maid
- `selene/` Selene, a moon-moth archivist
- `nova/` Nova, an arcade pilot

The colours that go with each (background, panel, border, accent, text, muted text, status,
text on the accent, secondary button) are the `Palette`s in `src/gui/theme.rs`. The window's
background is a flat colour, so the packs' separate background images are not used.

The pictures come from the `hindsight-theme-assets` pack, which says its character and
decorative images were produced with built-in image generation (its `generation-prompts.json`
holds the prompts), and that the style references it used are not redistributed.
