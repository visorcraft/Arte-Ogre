# Plugin Author Guide

Arte Ogre discovers local plugin bundles and runs them on a background thread.
There is no package registry or automatic installer.

Two plugin kinds exist:

- **Lua:** a document macro using four constrained editing functions.
- **WASM:** a CPU-only, point-wise filter over occupied raster tiles.

Both run with resource limits. A plugin still changes the active document, so
test it on disposable work and save first.

## Install a plugin

1. Open **Plugins → Plugin Manager**.
2. Read the plugin directory shown beside **Rescan**.
3. Create one immediate child directory for the bundle.
4. Put `plugin.toml` and its entry file in that child directory.
5. Click **Rescan**.
6. Confirm the plugin is valid and Enabled.
7. Open a document, select the required layer, and click **Run**.

A typical Linux plugin directory comes from the platform local-data directory.
Do not hard-code a guessed path; the Plugin Manager displays the resolved path.
If the platform directory cannot be resolved, Arte Ogre falls back to:

```text
.arte-ogre/plugins
```

Only immediate child directories are scanned:

```text
plugins/
├── cut-selection/
│   ├── plugin.toml
│   └── cut_selection.lua
└── invert/
    ├── plugin.toml
    └── invert.wat
```

Valid plugins are enabled after discovery. The Enabled checkbox is
session-local; rescanning rediscovers valid bundles as enabled.

## Manifest

Every bundle needs a UTF-8 `plugin.toml`:

```toml
name = "Cut Selection"
version = "1.0.0"
kind = "lua"
entry = "cut_selection.lua"
```

| Field | Required | Meaning |
| --- | --- | --- |
| `name` | No | Display name; defaults to the bundle directory name |
| `version` | No | Display version; defaults to empty |
| `kind` | Yes | Exactly `lua` or `wasm` |
| `entry` | Yes | Entry file relative to the bundle directory |

The manifest is limited to 64 KiB. `entry` must be a contained relative path:
absolute paths, `..` traversal, and missing files are rejected. Invalid bundles
remain visible in the manager with their error.

## Lua document plugins

Lua plugins are useful for repeatable command sequences. They do not receive
arbitrary document or pixel access.

### Complete host API

```lua
set_selection_rect(x, y, width, height)
cut_to_new_layer()
copy_to_new_layer()
add_raster_layer(name)
```

There are no other Arte Ogre globals.

#### `set_selection_rect(x, y, width, height)`

Replaces the current selection with an integer rectangle. Width and height
must be non-negative. The rectangle is clipped to the canvas; a rectangle
outside it becomes empty.

#### `cut_to_new_layer()`

Cuts the current selection from the active raster layer into a new layer while
preserving document position. The core command performs the normal active-layer
and lock validation.

#### `copy_to_new_layer()`

Copies the current selection from the active raster layer into a new layer
while preserving document position.

#### `add_raster_layer(name)`

Adds and activates an empty raster layer with the supplied string name.

### Example

`plugin.toml`:

```toml
name = "Cut Top Left"
version = "1.0.0"
kind = "lua"
entry = "cut_top_left.lua"
```

`cut_top_left.lua`:

```lua
set_selection_rect(0, 0, 256, 256)
cut_to_new_layer()
```

A working bundle is included at
[`examples/plugins/cut_selection_lua/`](../examples/plugins/cut_selection_lua/).

### Lua sandbox

The VM loads capability-free string, table, math, and UTF-8 libraries. It does
not expose:

```text
os
io
package
require
dofile
loadfile
load
loadstring
collectgarbage
debug
```

Limits:

- 64 MiB Lua memory
- 10,000,000-instruction hook limit
- 60-second UI timeout

Every host function creates a normal `ogre-core` command. If any statement
fails, previously applied script commands are undone in reverse order and the
document is restored.

On success, all script commands form one undo group. Current UI integration
replaces the document's earlier undo/redo history with that script result,
because a script can allocate layer IDs on its worker-owned document snapshot.
Save before running a script if earlier undo history matters.

## WASM tile filters

WASM plugins are destructive filters for the active raster layer. They execute
on the CPU, one occupied tile at a time.

### Current host ABI

The production host currently loads a core WebAssembly module, with no imports,
and requires two exports:

```text
memory
process
```

`process` has this core-WASM signature:

```text
process(
    in_ptr: i32,
    in_len: i32,
    width: i32,
    height: i32,
    params_ptr: i32,
    params_len: i32
) -> i32
```

Arguments:

| Argument | Unit | Meaning |
| --- | --- | --- |
| `in_ptr` | bytes | Start of input pixels in exported memory |
| `in_len` | bytes | Input byte length |
| `width` | pixels | Current tile width |
| `height` | pixels | Current tile height |
| `params_ptr` | bytes | Start of packed `f32` parameters |
| `params_len` | bytes | Parameter byte length |

The return value is a byte pointer to exactly
`width * height * 4 * sizeof(f32)` bytes of output in exported memory.

The module may modify input in place and return `in_ptr`, which is the simplest
implementation. It may instead return another valid memory region.

Pixels are row-major:

```text
R, G, B, A, R, G, B, A, ...
```

Channels are `f32`, straight alpha, and linear light. Edge tiles may be smaller
than 256×256, so always use the supplied width, height, and length.

The current Plugin Manager supplies an empty parameter list. Authors must
handle `params_len == 0`.

### WIT status

[`crates/ogre-plugins/wit/ogre-plugin.wit`](../crates/ogre-plugins/wit/ogre-plugin.wit)
documents the intended semantic interface using `list<f32>`. The current
desktop runtime does not instantiate that WIT component interface. A component
built only against the WIT contract will not load today; implement the raw
core-WASM ABI above.

### Point-wise filters only

The host invokes the plugin independently for each occupied sparse tile. It
does not provide neighboring tiles or a halo. Good filters include:

- Invert
- Per-channel scale or offset
- Threshold
- Point-wise color transforms

Do not implement blur, convolution, edge detection, or any effect that samples
neighboring pixels. It will produce seams at tile boundaries. Empty sparse
tiles are not materialized merely to run a plugin.

### WAT invert example

Wasmtime accepts `.wat` text directly, so a separate compile step is optional:

```wat
(module
  (memory (export "memory") 1)
  (func (export "process")
    (param $in i32) (param $len i32)
    (param $w i32) (param $h i32)
    (param $params i32) (param $params_len i32)
    (result i32)
    ;; Transform width * height RGBA32F values here.
    local.get $in))
```

The complete working invert filter is
[`examples/plugins/invert_wasm/invert.wat`](../examples/plugins/invert_wasm/invert.wat).
Its manifest is:

```toml
name = "Invert (WASM)"
version = "1.0.0"
kind = "wasm"
entry = "invert.wat"
```

Binary `.wasm` entries using the same ABI also work.

### WASM sandbox

- No host imports or WASI capabilities are provided.
- Fuel budget is 10,000,000 for one filter application, divided across
  occupied tiles.
- Linear memory is capped at 512 MiB per instance.
- At most 32 compiled modules are kept in the process cache.
- The UI stops waiting after 60 seconds and ignores late results.
- A compile failure, missing export, trap, fuel exhaustion, invalid pointer, or
  memory-growth failure leaves the active layer unchanged.

On success, the worker returns a replacement layer buffer and the UI dispatches
one undoable command. Existing undo history is preserved for WASM filters.

## Development and testing

Keep a bundle inside `examples/plugins/` when it should serve as a maintained
example. Test the plugin host and bundled examples with:

```sh
cargo test -p ogre-plugins
```

For interactive testing:

1. Copy the bundle into the directory displayed by Plugin Manager.
2. Click **Rescan**.
3. Open a small disposable `.ogre` document.
4. Select an unlocked raster layer.
5. Run the plugin.
6. Check pixels at tile boundaries, including X or Y equal to 255 and 256.
7. Undo once and confirm the original document returns.

### Common errors

| Error or symptom | Cause |
| --- | --- |
| `could not read plugin.toml: {error}` | Missing, unreadable, or larger than 64 KiB |
| `could not parse plugin.toml: {error}` | Invalid TOML |
| <code>invalid plugin kind `{kind}`; expected `wasm` or `lua`</code> | Unsupported `kind` |
| <code>missing required field `entry`</code> | No entry path |
| <code>entry path `{entry}` must be a relative path inside the plugin directory</code> | Absolute path or traversal |
| `No active raster layer` | WASM needs an active raster layer |
| `plugin filter has no exported memory` | Missing `memory` export |
| <code>plugin filter has no exported `process` function</code> | Wrong name or signature |
| `plugin filter execution failed (out of fuel or trap)` | Trap or fuel limit |
| Visible seams | Filter reads neighboring pixels without a tile halo |
| Lua script exceeded instruction limit | Runaway or overly expensive Lua |

The Plugin Manager shows discovery and runtime errors in the application.

## Compatibility policy

Plugin manifests and runtimes are still local application interfaces. Pin a
plugin to the Arte Ogre versions it was tested with. In particular, treat the
raw WASM ABI as the executable `1.8.x` contract and recheck this guide before
shipping against a later release.
