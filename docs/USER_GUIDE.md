# Arte Ogre User Guide

Arte Ogre is a Linux desktop image editor for layered raster, vector, and
compositing work. It uses 32-bit floating-point, linear-light pixels internally
and renders through the GPU.

This guide describes the current `1.8.x` interface. For exact import and export
fidelity, read [File Formats](FILE_FORMATS.md).

## Requirements and limits

- Linux with a Vulkan-capable GPU and working Vulkan drivers.
- Maximum production canvas size: 8192×8192 pixels.
- Maximum open document tabs: 12.
- Undo history: the latest 100 edits per document.
- Wayland is the default in a Wayland session. File drag-and-drop requires X11
  or XWayland.

New, resized, cropped, and imported documents are limited to 8192 pixels in
either dimension.

## Install and launch

### AppImage

Download the AppImage from the
[latest release](https://github.com/visorcraft/Arte-Ogre/releases/latest), make
it executable, and run it:

```sh
chmod +x Arte_Ogre-x86_64.AppImage
./Arte_Ogre-x86_64.AppImage
```

Install it for the current user:

```sh
install -Dm755 Arte_Ogre-x86_64.AppImage ~/.local/bin/arte-ogre
arte-ogre
```

### Source checkout

```sh
git clone https://github.com/visorcraft/Arte-Ogre.git
cd Arte-Ogre
cargo run -p ogre
```

Arte Ogre currently has no command-line file or option interface. Use
**File → Open** after launch.

### Wayland and X11

Arte Ogre uses native Wayland when launched from a Wayland session. Force X11
when file drag-and-drop is needed:

```sh
ARTE_OGRE_X11=1 arte-ogre
```

`File → Open` works on both backends. Native Wayland generally gives smoother
window resizing. See [Troubleshooting](TROUBLESHOOTING.md) for desktop icons,
GPU startup failures, and backend diagnostics.

## First five minutes

1. Choose **File → New** and enter a canvas size, or choose **File → Open**.
2. Paint with **Brush**, press `[` or `]` to change its size, and use `1`
   through `9` for opacity.
3. Add a raster layer with **Layer → New Raster Layer** or `Ctrl+Shift+N`.
4. Use **Rectangle Select**, right-click inside the canvas, and choose
   **Copy to New Layer**. The new layer remains at the exact source position.
5. Save editable work as `.ogre` with `Ctrl+Shift+S`.
6. Choose **File → Export** for a flattened PNG, JPEG, TIFF, WebP, EXR, ORA, or
   SVG deliverable.

## Workspace

The default editor has four regions:

- **Menu bar:** File, Edit, Select, Image, Layer, Filters, Plugins, View, and
  Help.
- **Tools sidebar:** selectable tools and the current tool's options. Collapse
  it to an icon strip or hide it with `Tab`.
- **Canvas tabs:** one tab per open document.
- **Layers panel:** layer creation, selection, ordering, visibility, locking,
  blend mode, and opacity.
- **Status bar:** zoom, cursor coordinates, selection dimensions, canvas size,
  and application version.

The tools sidebar sections can be reordered. Arte Ogre stores that order in
preferences. **Settings → Reset tools sidebar order** restores the default.

Press `Tab` to hide or restore the menu, tools, layers, and status chrome.
Choose **View → Reset Layout** if the workspace is difficult to recover.

### Command palette

Press `Ctrl+Shift+P` to search common actions:

- New canvas, Open, Save, Save As, and Export
- Undo and Redo
- Editor and Bird's Eye views
- Settings, Licenses, Credits, About, and Check for Updates
- Show or hide the tools sidebar

The palette runs the same commands as the menus.

## Documents and files

### New, open, duplicate, and close

- **File → New** creates a blank document using the requested dimensions.
- **File → Open** opens a supported image or document.
- **File → Duplicate Document** makes another editable document from the
  current state.
- **File → Close** or `Ctrl+W` closes the active tab.
- `Ctrl+Tab` and `Ctrl+Shift+Tab` move between tabs.

Opening a flat raster creates a document with one raster layer. Layered formats
preserve only the structures listed in [File Formats](FILE_FORMATS.md).

### Save, Save As, and Export

- **Save** writes to the document's current path.
- **Save As** with `.ogre`, or with no extension, writes the native layered
  format.
- **Save As** with an image extension exports that format and makes it the
  current path.
- **Export** writes a deliverable without changing the editable document model.

Use `.ogre` as the working master. Most export formats are flattened. Saving an
imported flat image back to its original path overwrites it with a flattened
composite.

### Autosave and crash recovery

Autosave writes recovery snapshots in the background. The default interval is
60 seconds; set it from 0 to 3600 seconds in **Settings**. `0` disables
autosave.

Recovery files are written atomically and capped at 10. On the next launch
after an unclean exit, Arte Ogre offers to recover or discard them. A clean exit
removes obsolete recovery snapshots.

Recovery is a crash safety net, not version history. Keep important work in a
saved `.ogre` document.

## Canvas navigation

- Mouse wheel: zoom toward the pointer.
- Middle-button drag: pan.
- Hold `Space` and left-drag: pan without changing tools.
- **Hand** tool: drag to pan.
- **Zoom** tool: click to zoom in, `Alt`-click to zoom out.
- `Ctrl+=`, `Ctrl+-`: zoom in or out.
- `Ctrl+0`: return to 100%.
- Status-bar zoom: set any value from 10% to 800%.
- Right-click the status-bar canvas size: fit the canvas in the window.
- Click the status-bar canvas size: open Canvas Size.

Choose **View → Show Grid** to display the grid and **View → Snap to Grid** to
snap supported interactions. Current grid spacing is 64 pixels.

Coordinates use a top-left origin. X increases right and Y increases down.

## Colors and swatches

The Color section holds foreground and background colors:

- Click either swatch to edit straight RGBA.
- Click the swap button or press `X` to exchange them.
- Click reset or press `D` for black foreground and white background.
- Recently chosen foreground colors appear under **Recent**; click one to load
  it.

The Swatches section stores up to 64 colors. Click Add to save the current
foreground color, click a saved swatch to load it, and right-click one to
remove it. Preferences retain up to 16 recent colors.

Foreground color drives Brush, Pencil, shapes, Pen, Type, Stroke Selection, and
foreground fill. Background color drives the background fill shortcut and a
gradient endpoint.

## Layers

### Layer types

Arte Ogre supports:

- **Raster layers:** sparse pixel tiles with an optional raster mask.
- **Group layers:** ordered layer subtrees.
- **Adjustment layers:** non-destructive tonal transformations.
- **Vector layers:** editable paths and text.

Each layer has a name, document offset, visibility, lock state, opacity, and
blend mode.

### Layers panel

Use the top buttons to add a raster layer, duplicate the active raster layer,
or delete the active layer.

For each row:

- Click the row to activate it.
- Click the eye to show or hide it.
- Click the lock to prevent destructive editing.
- Drag the row to reorder it.
- Edit opacity from 0 to 100%.
- Choose a blend mode.
- Double-click or press `F2` to rename.

Available blend modes are Normal, Multiply, Screen, Overlay, Darken, Lighten,
Color Dodge, Color Burn, Hard Light, Soft Light, Difference, Exclusion, and
Add.

Right-click a layer row for Delete or Flatten. Flatten and **Layer → Merge
Down** require compatible adjacent raster layers. The lower layer must be
unlocked and use Normal blend mode.

### Layer menu

The Layer menu provides:

- New Raster Layer, Duplicate, Delete, Rename, and Layer Size
- Merge Down
- Bring Forward and Send Backward
- Lock or Unlock
- Flip Horizontal and Flip Vertical
- New Adjustment Layer
- Layer Mask: Reveal All, Hide All, Reveal Selection, Apply, and Delete

All document edits enter undo history.

### Exact-position Copy/Cut to New Layer

This is Arte Ogre's core workflow:

1. Make a selection.
2. Right-click the canvas.
3. Choose **Copy to New Layer** or **Cut to New Layer**.

The extracted pixels keep their exact document coordinates. Sparse
layer-local tiles and the new layer's offset carry that position, so the result
does not depend on a paste anchor or later UI bookkeeping.

Copy works from a locked raster layer because it does not modify the source.
Cut and Delete require an unlocked raster layer.

## Selections

### Selection tools

| Tool | Main behavior |
| --- | --- |
| Rectangle Select | Drag an axis-aligned rectangle |
| Ellipse Select | Drag an ellipse |
| Polygon Lasso | Click vertices; double-click or press `Enter` to close |
| Freehand Lasso | Draw a freehand boundary |
| Magnetic Lasso | Place a boundary that follows nearby contrast |
| Magic Wand | Select similar pixels with tolerance and optional contiguity |
| Quick Selection | Paint a similarity-based selection brush |

For marquee and lasso tools, hold `Shift` to add and `Alt` to subtract. Tool
options also expose Replace, Add, Subtract, and Intersect where applicable.

Magic Wand options include tolerance and contiguous mode. Quick Selection
options include radius, tolerance, and **Sample all layers**.

### Selection commands

The Select menu provides:

- **All** (`Ctrl+A`)
- **None** (`Ctrl+D`)
- **Invert** (`Ctrl+Shift+I`)
- **Color Range**, with a seed color, fuzziness, and active-layer or merged
  sampling
- **Feather**
- **Grow**
- **Shrink**

The Edit menu provides Copy, Cut, Paste, Fill with Foreground, Fill with
Background, and Stroke Selection. Stroke Selection currently requires a
rectangular selection.

Selection coverage is preserved as alpha when copying to the system clipboard.

## Clipboard and drag-and-drop

`Ctrl+C` copies the selected composite region to the system clipboard as sRGB
RGBA8. The copied image is cropped to the selection bounds and its alpha is
multiplied by selection coverage.

`Ctrl+X` also deletes the selected source pixels. `Ctrl+V` creates a new raster
layer, centers it under the canvas pointer when possible, deselects, and
activates Move. If no document exists, or the pasted image exceeds the current
canvas, Arte Ogre asks whether to resize and paste, paste without resizing, or
cancel.

On the welcome screen, dropping a file opens it. With a document open, dropping
a file imports it as a new layer. Only the first file in a drop is handled.
Drag-and-drop requires X11; use `ARTE_OGRE_X11=1` or **File → Open**.

## Paint and retouch tools

### Paint tools

| Tool | Purpose |
| --- | --- |
| Eyedropper | Sample the foreground color; point, 3×3, or 5×5 average |
| Brush | Pressure-aware soft or hard painting |
| Pencil | Hard-edged painting |
| Eraser | Paint transparency |
| Paint Bucket | Fill a connected area using tolerance |
| Gradient | Linear, radial, conical, or diamond gradient |
| Color Replacement | Replace sampled color while painting |

Brush, Pencil, and Eraser options include size, hardness, opacity, flow, and
spacing where applicable. Use:

- `[` and `]` for brush size.
- `Shift+[` and `Shift+]` for hardness.
- `1` through `9` for 10% through 90% opacity.

When a tablet reports pressure, Brush and Eraser modulate size and opacity.

Five brush preset slots are available. Left-click a slot to load it and
right-click to save the current settings into it.

Gradient options include direction, reverse, opacity, and Clamp, Repeat, or
Reflect wrapping.

### Retouch tools

| Tool | Purpose and notable options |
| --- | --- |
| Blur | Soften detail with a sized brush |
| Sharpen | Increase local contrast; adjustable strength |
| Smudge | Drag existing pixels; adjustable strength and finger painting |
| Dodge | Lighten Shadows, Midtones, or Highlights |
| Burn | Darken Shadows, Midtones, or Highlights |
| Sponge | Saturate or desaturate |
| Clone Stamp | `Alt`-click a source, then paint copied pixels |
| Healing | `Alt`-click a source, then paint texture blended into the target |
| Spot Healing | Paint defects using an automatically chosen source |

Retouching is destructive on the active raster layer and requires it to be
unlocked.

## Move, transform, and crop

- **Move:** drag the active layer. Arrow keys nudge it by one pixel.
- **Free Transform:** `Ctrl+T`; use handles or numeric controls, then press
  `Enter` to commit or `Escape` to cancel.
- **Crop:** drag a crop rectangle and press `Enter`.

Free Transform modes are Scale/Rotate, Skew, Distort, Perspective, and Warp.
Scale/Rotate exposes numeric X, Y, scale, and rotation controls once a preview
exists.

## Vector, shape, text, and slice tools

### Shapes

Rectangle, Ellipse, Line, and Polygon tools support fill color, stroke color,
stroke width, and a **Vector/Pixels** mode.

- **Vector** creates or updates an editable vector layer.
- **Pixels** rasterizes destructively into the active raster layer.
- Hold `Shift` to constrain proportions.
- Hold `Alt` to draw from the center.

### Pen and path selection

The Pen tool creates anchors. Press `Enter` to commit. It supports fill, stroke,
width, closed fills, and Vector/Pixels mode.

**Path Select** selects an entire path. **Direct Select** edits individual
anchors. Selecting an existing vector layer loads supported content back into
the matching tool for in-place editing.

### Text

The Type tool supports:

- System font selection or a font family name
- 4 to 512 pixel size, with common size presets
- Bold and italic
- Text color
- Left, center, or right alignment
- Line-height multiplier
- Optional wrap width; `0` means automatic width
- Vector or Pixels output

Click to place text or drag a text box. `Shift+Enter` inserts a line break,
`Enter` commits, and `Escape` cancels. Vector text remains editable; Pixels
output is destructive.

### Slices

The Slice tool drags named export regions. **File → Export Slices** writes one
flattened image per non-empty slice using names based on slice name and
coordinates. Slice metadata is stored in the native `.ogre` file.

## Adjustments, filters, and masks

### Canvas and image geometry

The Image menu also provides:

- **Canvas Size:** resize the document canvas.
- **Crop to Selection:** crop to the current selection bounds.
- **Trim Transparent Borders:** crop to the composite's non-transparent bounds.

**Layer → Layer Size** resamples the active raster layer without changing the
document canvas. These commands are destructive but undoable.

### Adjustment layers

**Layer → New Adjustment Layer** creates a non-destructive adjustment. Available
adjustments are:

- Invert
- Desaturate
- Brightness/Contrast
- Levels
- Hue/Saturation
- Curves
- Posterize
- Threshold
- Gradient Map

Adjustment layers affect content below them according to normal layer ordering.

| Adjustment | Parameters |
| --- | --- |
| Brightness/Contrast | Brightness offset and contrast multiplier |
| Levels | Input black/white, output black/white, and gamma |
| Hue/Saturation | Hue degrees, saturation multiplier, and lightness |
| Curves | Four editable control points |
| Posterize | Number of levels, minimum 2 |
| Threshold | Luminance threshold from 0 to 1 |
| Gradient Map | Foreground and background endpoint colors |

### Destructive adjustments and spatial filters

The Image menu applies the same tonal adjustments destructively to the active
raster layer.

The Filters menu provides Gaussian Blur, Sharpen, Emboss, and Edge Detect.
Filter dialogs show a live preview before commit. Cancel leaves the document
unchanged.

Gaussian Blur has a radius. Sharpen has amount and radius. Emboss and Edge
Detect have amount controls.

### Masks

A raster layer can have one grayscale coverage mask:

- **Reveal All:** white mask, initially showing the layer.
- **Hide All:** black mask, initially hiding the layer.
- **Reveal Selection:** derives coverage from the current selection.
- **Apply:** bakes mask coverage into raster alpha.
- **Delete:** removes the mask without applying it.

## Remove Background

Choose **Image → Remove Background** on a raster layer. The operation detects a
solid or checkerboard matte connected to the image border, converts it to alpha,
and defringes antialiased edges.

Options:

- **Tolerance:** how far colors may differ from the detected background.
- **Edge cleanup:** strength of fringe cleanup.
- **Remove enclosed:** also remove matching regions not connected to the border.
- **Refine edges with AI:** optionally refine difficult silhouettes.

The shipped binary enables AI refinement by default. First use downloads an
approximately 170 MiB IS-Net model into the user cache, verifies its pinned
SHA-256 checksum, and installs it atomically. Inference runs locally afterward.
If the model is unavailable or inference fails, Arte Ogre falls back to the
standard color-key result.

Build without model support:

```sh
cargo run -p ogre --no-default-features
```

## Bird's Eye View

Bird's Eye shows every open document as a card and each root layer as a
thumbnail.

- Click a thumbnail to activate its document and layer.
- Double-click to return to the editor with that layer active.
- Drag within one card to reorder root layers.
- Drag to another card to move a layer or group subtree.
- Hold `Ctrl` while dropping across documents to copy instead of move.
- Use the global thumbnail zoom from 40% to 200%.
- Open or add an image from the Bird's Eye toolbar.

Child layer IDs are remapped when a group is copied. A cross-document move is
represented by two independent history commands: insert in the destination,
then delete from the source. Undo each document separately.

Press `Ctrl+Shift+B` to toggle Bird's Eye. Press `Escape` or choose **Editor
View** to return.

## Keyboard shortcuts

Configurable shortcuts live in **Settings → Keyboard Shortcuts**.

### Documents and history

| Action | Default |
| --- | --- |
| New document | `Ctrl+N` |
| Open | `Ctrl+O` |
| Save | `Ctrl+S` |
| Save As | `Ctrl+Shift+S` |
| Close tab | `Ctrl+W` |
| Quit | `Ctrl+Q` |
| Next tab | `Ctrl+Tab` |
| Previous tab | `Ctrl+Shift+Tab` |
| Undo | `Ctrl+Z` |
| Redo | `Ctrl+Shift+Z` or `Ctrl+Y` |
| Command palette | `Ctrl+Shift+P` |
| Bird's Eye | `Ctrl+Shift+B` |

### Selection, layers, and canvas

| Action | Default |
| --- | --- |
| Select All | `Ctrl+A` |
| Deselect | `Ctrl+D` |
| Invert Selection | `Ctrl+Shift+I` |
| New raster layer | `Ctrl+Shift+N` |
| Duplicate layer | `Ctrl+J` |
| Rename layer | `F2` |
| Delete selected pixels | `Delete` |
| Bring Forward | `Ctrl+]` |
| Send Backward | `Ctrl+[` |
| Fill foreground | `Alt+Backspace` |
| Fill background | `Ctrl+Backspace` |
| Free Transform | `Ctrl+T` |
| Zoom in | `Ctrl+=` |
| Zoom out | `Ctrl+-` |
| 100% zoom | `Ctrl+0` |
| Reset foreground/background to black/white | `D` |
| Swap foreground/background | `X` |

### Tool keys

Pressing a family key repeatedly cycles its tools. Hold `Shift` to cycle
backward.

| Key | Tool family |
| --- | --- |
| `M` | Rectangle Select, Ellipse Select |
| `L` | Polygon, Freehand, Magnetic Lasso |
| `W` | Magic Wand, Quick Selection |
| `I` | Eyedropper |
| `B` | Brush, Pencil, Color Replacement |
| `E` | Eraser |
| `R` | Blur, Sharpen, Smudge |
| `O` | Dodge, Burn, Sponge |
| `S` | Clone Stamp |
| `J` | Healing, Spot Healing |
| `G` | Paint Bucket, Gradient |
| `V` | Move |
| `C` | Crop |
| `U` | Rectangle, Ellipse, Line, Polygon shapes |
| `P` | Pen |
| `T` | Type |
| `A` | Path Select, Direct Select |
| `K` | Slice |
| `H` | Hand |
| `Z` | Zoom |

Shortcuts are suppressed while typing in a field or while a modal dialog owns
keyboard input.

## Settings and local data

Settings include:

- Theme
- Default canvas width and height; 1920×1080 initially
- Autosave interval
- Tools sidebar order reset
- Keyboard shortcut editor

Themes include System, Light, Dark, OLED Black, Gentle Gecko, Black Knight,
Diamond, Dreams, Paranoid, Red Velvet, Subspace, Tiefling, and Vibes.

Preferences also store recent files, recent colors, swatches, five brush
presets, active tool choices, sidebar expansion, and Layers panel visibility.
The Settings window displays the exact preferences path. On a typical Linux
desktop it is under the platform configuration directory, usually
`~/.config/ogre/prefs.toml`.

Plugin bundles and recovery files use the platform local-data directory. The
Plugin Manager and recovery UI resolve those paths for the current system.

The AI model cache is:

```text
$XDG_CACHE_HOME/arte-ogre/isnet-general-use.onnx
```

or, when `XDG_CACHE_HOME` is unset:

```text
$HOME/.cache/arte-ogre/isnet-general-use.onnx
```

## Privacy and network behavior

Arte Ogre has no telemetry. Image editing, save/export, plugins, and AI
inference run locally.

Network access occurs only after an explicit action:

- **Help → Check for Updates** contacts the GitHub API.
- First use of **Refine edges with AI** downloads the pinned model.

No document or image content is uploaded by either action.

## Current boundaries

- Linux is the supported desktop platform.
- Canvas dimensions are limited to 8192×8192.
- GPU output intentionally matches the CPU reference within `1e-4` per channel,
  not bit-for-bit across every device.
- `.ogre` saves layers and slices, but not undo history or UI layout.
- Export fidelity varies by format. Read [File Formats](FILE_FORMATS.md).
- Plugins are local code with constrained capabilities, not a package registry.
- File drag-and-drop is unavailable on native Wayland.

For failures and diagnostics, continue with
[Troubleshooting](TROUBLESHOOTING.md).
