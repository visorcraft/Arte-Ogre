# `ogre-ui`

`ogre-ui` is the `eframe`/`egui` application layer. It owns interactive state,
panels, tools, preferences, background workers, and command dispatch, while
`ogre-core` remains the document ground truth and `ogre-gpu` renders the canvas.

## Running

```sh
cargo run -p ogre
```

This opens the editor shell with a GPU canvas, tool sidebar, dockable panels,
menus, dialogs, and the layers panel.

## Current Surface

- Docked app shell, command palette, welcome/recovery flows, settings, credits,
  licenses, and plugin manager views.
- GPU canvas with pan, zoom, DPI-aware coordinate mapping, overlays, context
  menus, paste prompts, and file drag/drop where the platform backend supports
  it.
- Layers panel for add, delete, duplicate, reorder, rename, visibility, opacity,
  blend mode, locking, masks, adjustment layers, and vector layer re-editing.
- Selection tools: rectangle, ellipse, polygon lasso, freehand lasso, magnetic
  lasso, magic wand, quick select, Select All, Deselect, and Select Inverse.
- Paint and retouch tools: brush, pencil, eraser, paint bucket, eyedropper,
  gradient, clone stamp, healing, spot healing, blur, sharpen, smudge, dodge,
  burn, sponge, and color replacement.
- Transform and layout tools: move, crop, free transform, distort, perspective,
  warp, hand, zoom, and slice.
- Vector/text tools: shapes, pen, type, path select, and direct select, with
  Vector/Pixels commit modes where applicable.
- File and background work: open, save, export, slice export, clipboard image
  copy/paste, plugin execution, autosave/recovery, and remove-background worker.

All document mutations must route through `crate::dispatch` and ultimately an
`ogre-core::Command`; direct document mutation in UI code is reserved for
adopting replacement documents from I/O or plugin workers.

## Testing

```sh
cargo test -p ogre-ui
cargo clippy -p ogre-ui --all-targets -- -D warnings
cargo fmt --check
cargo doc -p ogre-ui
```
