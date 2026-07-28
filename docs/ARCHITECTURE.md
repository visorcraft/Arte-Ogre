# Architecture

Arte Ogre is a Rust-native layered image editor. The headless core owns document
truth; UI, GPU, I/O, vector, and plugin crates surround it without creating a
second document model.

This document describes the current `1.8.x` design and the invariants required
for correct changes.

## Workspace map

```text
ogre-core ─► ogre-gpu ─► ogre-ui ─► ogre
   ▲            ▲           ▲
   └── ogre-io ─┴─ ogre-plugins ─┘

ogre-vector ─► ogre-core
```

The arrows show the main flow from reusable engine crates toward the desktop
binary. Some integration crates share lower-level dependencies.

| Crate | Responsibility |
| --- | --- |
| `ogre-core` | Headless document, layers, sparse tiles, selection, commands, history, brush engine, transforms, resampling, operations, and CPU compositor |
| `ogre-gpu` | `wgpu` context, interactive compositor, GPU filters, dirty-tile uploads, and CPU/GPU golden tests |
| `ogre-ui` | `eframe::App`, panels, tools, views, dispatch, preferences, clipboard, workers, and dialogs |
| `ogre` | Thin desktop entry point and shared eframe/wgpu device setup |
| `ogre-io` | Native, ORA, PSD, raster, SVG, ICC, export, and recovery serialization |
| `ogre-plugins` | Sandboxed Lua document scripts and WASM tile filters |
| `ogre-vector` | Path and text rasterization through `kurbo`, `lyon`, and `cosmic-text` |

The workspace uses Rust 2021 and tracks `Cargo.lock`. There is no JavaScript
package layer.

## Design rules

Five rules define the architecture:

1. **One document model.** `ogre_core::Document` is authoritative.
2. **One persistent mutation path.** Edits are `ogre-core` commands recorded in
   `History`; UI code submits them through `ogre_ui::dispatch::dispatch`.
3. **One compositing truth.** `ogre_core::composite_document` is the CPU
   reference; GPU output must match it within `1e-4` per channel.
4. **One pixel model.** `Rgba32F`, straight alpha, linear light, top-left
   origin.
5. **Layer-local sparse storage.** Tiles live in local coordinates; layer
   offsets place them in the document.

Breaking one of these can produce plausible-looking output that fails only
under negative offsets, undo/redo, interchange, or a particular GPU.

## Core data model

### Document

`Document` contains:

- Canvas dimensions
- Color-space information and optional ICC profile bytes
- A stable-ID layer store
- Bottom-to-top root layer order
- Active layer
- Current selection
- Soft-removed layer state used by commands
- Slice metadata

Layer IDs are document-local. Never share a `LayerId` between documents.
Cross-document copy remaps every ID in a copied subtree.

### Layers

All layer variants share:

- Stable ID
- Name
- Integer X/Y offset
- Opacity
- Blend mode
- Visibility
- Lock state

Layer content is one of:

- Raster tiles with an optional mask
- Group child IDs
- Adjustment parameters
- Vector geometry/text

Root and group child arrays define order from bottom to top.

### Selection

A selection can be:

- Empty
- Rectangular
- Inverted rectangular
- Sparse per-pixel coverage

Coverage supports antialiased boundaries and Replace, Add, Subtract, and
Intersect operations. Clipboard and extraction operations multiply pixel alpha
by selection coverage.

### History and commands

A command owns enough state to apply and undo one logical mutation. `History`
executes it, limits retained history, and supports redo.

The UI path is:

```text
menu/tool/panel input
        │
        ▼
ogre_ui::dispatch::dispatch
        │
        ▼
ogre_core::History
        │
        ▼
Command::apply(Document)
        │
        ├── changed document state
        └── dirty information for rendering
```

Do not mutate a document from UI or GPU code to implement an edit. Direct
mutation bypasses undo, renderer invalidation, and command invariants.

Commands that belong to one user gesture should coalesce or enter a batch when
appropriate. Move-arrow nudges coalesce. A plugin operation becomes one
undoable result.

## Tiles and coordinates

Raster storage is sparse and copy-on-write:

- Tile size is 256×256 pixels.
- Empty tiles do not exist.
- Shared tiles use `Arc`; mutation clones only the touched tile.
- Tiles are stored in layer-local coordinates.
- A layer offset maps local pixels into document space.

The mapping is:

```text
document_coordinate = local_coordinate + layer.offset
```

Negative local and document coordinates are valid during transforms and layer
movement. Tile calculations must use `div_euclid` and `rem_euclid`; Rust's
truncating `/` and `%` are wrong for negative coordinates.

### Why extraction keeps position

Copy to New Layer does not crop pixels into a new origin and later guess where
they belonged. It copies selected sparse content and assigns the new layer an
offset that preserves:

```text
new_local + new_offset = old_local + old_offset
```

The same invariant holds through cut, move, undo, and redo.

## Pixel and compositing model

`Rgba32F` is `#[repr(C)]` and `bytemuck::Pod`, allowing validated transfer
between CPU storage and GPU buffers.

Pixels use:

- Four `f32` channels
- Straight, non-premultiplied alpha
- Linear-light RGB
- Top-left origin
- Positive Y downward

The compositor evaluates visibility, group order, masks, opacity, adjustments,
and blend modes. Supported blend modes are Normal, Multiply, Screen, Overlay,
Darken, Lighten, Color Dodge, Color Burn, Hard Light, Soft Light, Difference,
Exclusion, and Add.

All new GPU composition behavior needs an equivalent CPU reference behavior.
Golden tests compare channels with a `1e-4` tolerance.

## CPU compositor

`ogre_core::composite_document` is the correctness oracle. It is also the
fallback for headless work and operations that need a complete flattened
image.

Responsibilities include:

- Traversing root layers and groups in order
- Mapping document coordinates into each layer
- Applying masks and opacity
- Applying adjustment layers
- Evaluating blend modes
- Producing linear `Rgba32F`

When a visual discrepancy appears, first reduce it to the CPU compositor. A GPU
change should follow the CPU semantics, not redefine them.

## GPU compositor

`ogre-gpu` uses the same `wgpu::Device` as eframe. The binary requests
`TEXTURE_ADAPTER_SPECIFIC_FORMAT_FEATURES`, uses float texture paths, and caps
`max_texture_dimension_2d` at 8192.

The interactive renderer:

- Uploads only changed sparse tiles needed by the visible output region
- Enforces bounded LRU source and result tile caches during compositing
- Tracks dirty document regions
- Recomposites affected regions instead of rebuilding the whole canvas
- Implements blend and adjustment behavior in WGSL
- Registers its texture directly with egui

`GpuContext::from_shared` integrates with eframe. Headless constructors support
tests. The `ogre-gpu` `egui` feature enables
`CanvasRenderer::register_with_egui`.

Shaders live under `crates/ogre-gpu/src/shaders/`.

## UI and dispatch

`ogre-ui` owns interaction state, not document truth. Major areas are:

- `state`: open documents, active view, preferences, transient UI state
- `dispatch`: the edit boundary into command history
- `panels`: tools, layers, menu, and status UI
- `tools`: pointer/key handling and tool-specific options
- `views`: Editor, Bird's Eye, Settings, Licenses, and Plugins
- keymap and preferences
- I/O, plugin, clipboard, autosave, update, and background-removal workers

The application has one active editor document but can keep up to 12 tabs.
Each document owns its history and renderer-facing state.

### Tool transaction pattern

Interactive tools generally follow:

```text
pointer down       create preview/gesture state
pointer move       update transient preview
pointer up/Enter   build and dispatch one command
Escape             discard transient preview
```

This keeps half-complete transforms and filter previews out of persistent
history.

## Background workers

Long work must not block the egui frame loop. I/O, plugins, autosave, update
checks, AI refinement, and related tasks run through workers.

For document-changing work, a worker operates on owned or cloned state and
returns a replacement `(Document, History)` for the UI to adopt. It does not
hold a shared mutable document behind the UI's back.

Worker requests carry IDs so stale results can be ignored. Some queues coalesce
pending work to the latest request. The UI prevents conflicting edits while a
document-changing task is busy.

Current user-facing limits include:

- Plugin operation timeout: 60 seconds
- Standard background-removal timeout: 30 seconds
- AI refinement timeout: 600 seconds

Sandbox fuel/instruction limits may stop work earlier.

## Bird's Eye cross-document moves

Bird's Eye only displays root layers, but a group thumbnail represents its
entire subtree.

Within one document, drag reorders roots through normal history.

Across documents:

1. Clone the source subtree.
2. Remap all layer IDs for the destination.
3. Dispatch `InsertLayerSubtreeCmd` to the destination.
4. For a move, dispatch `DeleteLayerCmd` to the source.

The two documents keep independent history. Therefore destination insertion
and source deletion are separately undoable.

## File I/O

`ogre-io` translates external formats into and out of the core model. It does
not define an alternate layer representation.

The native format stores a named MessagePack manifest plus independently
zstd-compressed occupied tiles. Loaders treat all dimensions and payload sizes
as untrusted.

Raster import converts pixels into the internal linear `Rgba32F` model. Flat
export composites through core behavior before encoding. ICC transforms use
`lcms2`.

SVG support is feature-gated by `ogre-io`'s default `svg` feature. Optional
matte refinement is feature-gated by `ml` through `ogre`, `ogre-ui`, and
`ogre-io`.

See [File Formats](FILE_FORMATS.md) for fidelity boundaries.

## Vector pipeline

Vector layers keep editable paths and text in the core document.
`ogre-vector` converts them to raster coverage with:

- `kurbo` for path geometry
- `lyon` for tessellation/raster work
- `cosmic-text` for shaping and text layout

Shape, Pen, and Type tools can either write editable vector content or
rasterize into the active raster layer. Vector commands must survive byte-level
undo/redo round trips.

## Plugin pipeline

`ogre-plugins` supports two constrained execution models:

- Lua calls a small document-operation API.
- WASM processes independent raster tiles through a raw linear-memory ABI.

Plugins run away from the UI thread. Lua has restricted libraries, a memory
limit, and an instruction limit. WASM has fuel and linear-memory limits. Plugin
results are adopted as one undoable document operation.

See [Plugin Author Guide](PLUGINS.md) for the supported ABI and sandbox.

## Network boundary

Normal editing is local and has no telemetry. Only two explicit paths use the
network:

- **Help → Check for Updates** queries GitHub.
- First AI edge refinement downloads a pinned model and verifies its checksum.

The model is cached locally; inference does not upload image data.

## Correctness tests

High-value invariants are pinned by named tests:

```sh
cargo test -p ogre-core copy_to_new_layer_retains_exact_position
cargo test -p ogre-core --test killer_feature cut_move_undo_redo_loop_is_byte_identical
cargo test -p ogre-core --test vector_parity
```

Run broader property tests with:

```sh
PROPTEST_CASES=1024 cargo test -p ogre-core
```

Run the release-only GPU performance/golden test with:

```sh
cargo test -p ogre-gpu --release --test composite_perf -- --ignored
```

Every changed crate must also pass test, clippy, format, and documentation
checks described in [Contributing](../CONTRIBUTING.md).

## Where to make a change

| Change | Start here |
| --- | --- |
| Document/layer/selection semantics | `crates/ogre-core/src/` |
| Undoable operation | `ogre-core` command, then `ogre-ui::dispatch` caller |
| Blend/composite behavior | CPU compositor first, then matching WGSL |
| Canvas upload/rendering | `crates/ogre-gpu/src/` |
| Menu, panel, dialog, or tool gesture | `crates/ogre-ui/src/` |
| Import/export or recovery encoding | `crates/ogre-io/src/` |
| Path/text rasterization | `crates/ogre-vector/src/` |
| Lua/WASM runtime | `crates/ogre-plugins/src/` |
| Device/window/bootstrap behavior | `crates/ogre/src/main.rs` |

Generate browsable Rust API documentation:

```sh
cargo doc --workspace --no-deps --open
```

## Review checklist

Before merging a significant architecture change, verify:

- The edit enters through a command and is undoable.
- Negative layer/tile coordinates use Euclidean division.
- Pixel math uses straight alpha and linear light.
- CPU and GPU implementations agree.
- Cross-document copied IDs are remapped.
- Untrusted sizes are bounded before allocation.
- Background work cannot apply a stale result.
- Native/interchange behavior and public docs remain accurate.
