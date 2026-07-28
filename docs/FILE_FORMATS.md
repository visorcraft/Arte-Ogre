# File Formats and Interchange

Arte Ogre's native `.ogre` format is the only format intended to preserve the
complete editable layer model. Other formats are import sources or interchange
deliverables with explicit limits.

## Safe working pattern

1. Open or import the source.
2. Immediately save an editable master as `.ogre`.
3. Continue editing the `.ogre` master.
4. Use **Export** for PNG, JPEG, TIFF, WebP, EXR, ORA, or SVG deliverables.

Saving an imported flat image back to its source path writes a flattened
composite. Use **Save As** to avoid accidentally replacing the original.

## Compatibility matrix

| Format | Open | Save/Export | Layer fidelity | Important notes |
| --- | --- | --- | --- | --- |
| `.ogre` | Yes | Yes | Native | Preserves the current editable document except history and UI state |
| `.ora` | Yes | Yes | Partial | Raster groups and common layer properties; adjustment/vector layers and masks are not exported |
| `.psd` | Yes | No | Partial | Imports visible pixel layers; hierarchy, blend modes, masks, and editable PSD features are not preserved |
| `.png` | Yes | Yes | Flat | RGBA, 8-bit or 16-bit export |
| `.jpg`, `.jpeg` | Yes | Yes | Flat | 8-bit lossy RGB; transparency is composited over white |
| `.tif`, `.tiff` | Yes | Yes | Flat | RGBA, 8-bit or 16-bit export |
| `.webp` | Yes | Yes | Flat | Lossless RGBA8 export |
| `.exr` | Yes | Yes | Flat | RGBA32F export |
| `.bmp` | Yes | No explicit export option | Flat | Raster import |
| `.svg`, `.svgz` | Yes | `.svg` export | Partial or flat | Raster, extracted vector, or hybrid import; export is one embedded PNG |

The Open dialog lists the formats supported by the desktop workflow. A typed
path may also reach decoders enabled in the `image` crate, but unlisted formats
are not a compatibility promise.

## Save and export behavior

### Save

**File → Save** writes to the document's current path:

- A current `.ogre` path writes the native layered document.
- A current raster or interchange path writes that flattened/interchange
  format.

### Save As

**File → Save As** chooses behavior from the extension:

- `.ogre`, or no extension: native layered save.
- A recognized image extension: export that format and adopt the chosen path.

### Export

**File → Export** offers PNG, JPEG, TIFF, WebP, EXR, ORA, and SVG. Raster
exports composite the visible document first. Hidden layers do not contribute.

The bit-depth control affects PNG and TIFF. JPEG and WebP are always 8-bit in
the current exporter; EXR is 32-bit float.

### Export Slices

**File → Export Slices** composites each non-empty slice rectangle and writes:

```text
{safe_slice_name}_{x}_{y}.{extension}
```

Characters outside letters, digits, `-`, and `_` are replaced with `_`. Slice
export supports the same flat raster destinations. Slice definitions are
persisted in `.ogre`.

## Native `.ogre`

### What it preserves

| Document data | Preserved |
| --- | --- |
| Canvas dimensions | Yes |
| Color-space identifier and embedded ICC bytes | Yes |
| Root layer order and active layer | Yes |
| Raster layers and sparse tiles | Yes |
| Raster masks | Yes |
| Groups and child order | Yes |
| Adjustment layers and parameters | Yes |
| Vector layers and editable geometry | Yes |
| Names, offsets, opacity, blend mode, visibility, and lock state | Yes |
| Current selection and its coverage | Yes |
| Undo/redo history | No |
| Slice definitions | Yes |
| Zoom, pan, panels, dialogs, and other UI state | No |

### Container

The current native version is 2. Files contain:

1. The four-byte magic `OGRE`.
2. A little-endian format version.
3. A little-endian manifest length.
4. A named MessagePack manifest.
5. Occupied pixel tiles compressed independently with zstd level 3.

Independent tile compression keeps empty canvas regions out of the file and
preserves the sparse copy-on-write document model. The loader accepts native
versions 1 and 2.

The loader validates dimensions, allocation sizes, tile payloads, layer
references, and vector geometry before constructing a document. It rejects
empty canvases, canvases above 8192 in either dimension, excessive canvas area,
oversized paths, malformed references, and decompression-size violations.

`.ogre` is the application's evolving native format, not a replacement for a
published interchange standard. Keep archival backups when a document matters.

## Flat raster formats

### Import

Raster import creates one layer named from the source filename and a canvas
matching the image dimensions.

- Integer sRGB data is converted to linear-light `Rgba32F`.
- 8-bit and 16-bit inputs are expanded without retaining their original
  integer storage type.
- Float inputs remain float through the internal pipeline.
- An embedded ICC profile, when detected, is used for conversion and retained
  in the document.
- Alpha remains straight, not premultiplied.

### Export

Raster export composites visible layers in linear light, converts the result
for the destination, then encodes:

- **PNG:** RGBA8 or RGBA16.
- **TIFF:** RGBA8 or RGBA16.
- **JPEG:** lossy 8-bit RGB at the selected quality. Alpha is composited over
  white.
- **WebP:** lossless RGBA8.
- **EXR:** RGBA32F.

Current raster encoders do not write the document's ICC profile bytes into the
output file. Pixel values are converted for export, but applications that
require explicit profile tagging should assign or embed the intended profile
after export.

## OpenRaster `.ora`

### Import

ORA import preserves:

- Nested raster groups
- Layer order
- Layer names
- Layer offsets
- Opacity
- Visibility
- Common blend modes when a mapping exists

### Export

ORA export writes a group tree and cropped PNG data for raster layers.

Current limitations:

- Adjustment layers are skipped.
- Vector layers are skipped.
- Raster masks are not written.
- Selection, slices, lock state, ICC metadata, and history are not written.
- Unsupported blend behavior may not reproduce identically in another editor.

Because skipped layers are not automatically flattened into replacement raster
layers, the exported image may look different from the Arte Ogre document.
Inspect the result before delivery. For reliable appearance, export a flat PNG
alongside ORA.

## Photoshop `.psd`

PSD support is import-only.

The importer creates raster layers for visible pixel layers and preserves:

- Layer name
- Opacity
- Visibility
- Rendered pixel placement

Current limitations:

- Group divider records are skipped; hierarchy is flattened.
- Layer offsets are baked into canvas-positioned pixels.
- Blend modes are imported as Normal.
- Masks, adjustment layers, smart objects, vector objects, text editability,
  and other Photoshop-specific structures are not preserved.
- PSD export is unavailable.

Use the imported result as a visual starting point, then save it as `.ogre`.

## SVG and SVGZ

The SVG import dialog asks for DPI and one of three modes.

### Rasterize

Renders the complete SVG through `resvg` into one raster layer. This is the
best visual-fidelity option for unsupported vector features, but the result is
not path-editable.

### Vector

Extracts supported visible root path nodes into editable Arte Ogre vector
geometry. It supports common:

- Solid fills and strokes
- Stroke width
- Dashes
- Line caps and joins
- Path geometry

This is intentionally a subset. Text, filters, gradients, nested structures,
and other SVG features may be absent or simplified.

### Both

Stores supported editable paths together with a pre-rendered cache and the
original SVG source. The initial appearance retains the rasterized visual. Once
the vector content is edited, the editable paths become authoritative and
unsupported source features may disappear.

### DPI and compressed input

DPI must be greater than zero and no more than 2400; the default is 96. `.svgz`
input is gzip-compressed SVG.

### SVG export

SVG export currently writes an SVG 1.1 wrapper containing one base64-encoded
PNG image of the flattened composite. It does not export the Arte Ogre layer
tree or editable vector paths.

## Color and alpha model

Internally all pixels are:

- `Rgba32F`
- Straight alpha
- Linear light
- Top-left origin

This matters when comparing exports with another editor. A compositing system
that blends gamma-encoded values or assumes premultiplied alpha can produce
different edges and blend results.

GPU composition is tested against the CPU reference within `1e-4` per channel.
Small device-dependent floating-point differences inside that tolerance are
expected.

## Format choice

| Need | Recommended format |
| --- | --- |
| Continue editing in Arte Ogre | `.ogre` |
| Lossless web or general delivery | PNG |
| Small photographic delivery | JPEG |
| 16-bit integer delivery | PNG or TIFF |
| Floating-point/HDR interchange | EXR |
| Limited raster layer interchange | ORA plus a reference PNG |
| Import an existing Photoshop document | PSD, then save as `.ogre` |
| Preserve complete SVG appearance on import | Rasterize or Both |
| Deliver SVG to a consumer that only needs appearance | SVG export |

When fidelity is critical, reopen every exported file and compare it with the
master before distributing it.
