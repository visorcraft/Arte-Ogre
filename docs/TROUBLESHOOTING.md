# Troubleshooting

Start with the smallest matching section. Do not delete a document or recovery
file while diagnosing it; rename or copy it first.

## Capture basic diagnostics

Launch from a terminal so startup errors remain visible:

```sh
arte-ogre 2>&1 | tee arte-ogre.log
```

For an AppImage:

```sh
./Arte_Ogre-x86_64.AppImage 2>&1 | tee arte-ogre.log
```

Collect platform and Vulkan information:

```sh
uname -a
vulkaninfo --summary
lspci -k | grep -Ei -A3 'vga|3d|display'
```

Systemd user-journal messages from a recent launch:

```sh
journalctl --user --since '5 min ago' | grep -i ogre
```

The **Help → About** window shows the application version.

## AppImage does not start

### Permission denied

Make the file executable:

```sh
chmod +x Arte_Ogre-x86_64.AppImage
./Arte_Ogre-x86_64.AppImage
```

### FUSE is unavailable

Run through AppImage extraction:

```sh
./Arte_Ogre-x86_64.AppImage --appimage-extract-and-run
```

If that succeeds, install the distribution's AppImage/FUSE support or keep
using the extraction option.

### Wrong architecture

The packaged artifact is currently `x86_64`. Check:

```sh
uname -m
file Arte_Ogre-x86_64.AppImage
```

Build from source on another supported Rust target, but Linux desktop behavior
outside `x86_64` is not a release guarantee.

## Window flashes, then exits with code 101

This is often an NVIDIA kernel/userspace version mismatch after a driver
upgrade without a reboot, not an application defect. The loaded kernel module
and userspace driver must match.

On Arch Linux:

```sh
cat /proc/driver/nvidia/version
pacman -Q nvidia-utils
vulkaninfo --summary | grep deviceName
```

If the versions differ, reboot. A temporary launch path is:

```sh
env WGPU_BACKEND=vulkan ARTE_OGRE_X11=1 arte-ogre
```

Also inspect:

```sh
journalctl --user --since '5 min ago' | grep -Ei 'ogre|wgpu|vulkan|egl|nvidia'
```

If `vulkaninfo --summary` does not list the expected GPU, fix the Vulkan driver
before debugging Arte Ogre.

## GPU adapter or texture errors

Arte Ogre needs:

- A Vulkan-capable adapter and driver
- `Rgba32Float` texture support used by the renderer
- `TEXTURE_ADAPTER_SPECIFIC_FORMAT_FEATURES`
- Texture dimensions up to the current 8192 ceiling

Check the adapter:

```sh
vulkaninfo --summary
```

Try the explicit Vulkan backend:

```sh
WGPU_BACKEND=vulkan arte-ogre
```

For hybrid-GPU laptops, use the distribution's normal discrete-GPU launch
method and confirm that adapter appears in `vulkaninfo`. Environment variables
for GPU selection differ by driver and distribution.

If only one document fails, check its canvas size. The native loader and GPU
device reject dimensions above 8192.

## Wayland and X11

### File drag-and-drop does nothing

winit does not provide file drag-and-drop on Wayland. Use **File → Open** or
force X11:

```sh
ARTE_OGRE_X11=1 arte-ogre
```

With a document open, a dropped file is added as a layer. On the welcome
screen, it opens as a document. Only the first file in one drop is handled.

### Window is black while resizing on X11

This is a current winit/X11 limitation. It redraws after the resize gesture
ends. Use native Wayland when smooth live resizing matters:

```sh
unset ARTE_OGRE_X11
arte-ogre
```

### Wayland title-bar or taskbar icon is missing

Wayland resolves the icon from a desktop entry whose application ID is
`arte-ogre`. Install both files:

```sh
install -Dm644 AppDir/usr/share/applications/arte-ogre.desktop \
  ~/.local/share/applications/arte-ogre.desktop
install -Dm644 AppDir/usr/share/icons/hicolor/256x256/apps/arte-ogre.png \
  ~/.local/share/icons/hicolor/256x256/apps/arte-ogre.png
```

The AppImage build script creates these files in `AppDir`.

## Canvas, zoom, or panels look lost

- Press `Tab` to restore hidden chrome.
- Choose **View → Reset Layout**.
- Right-click the canvas size in the status bar to fit the document.
- Press `Ctrl+0` for 100% zoom.
- Use the status-bar zoom field; accepted range is 10% to 800%.

If the tools sidebar order is confusing, choose **Settings → Reset tools
sidebar order**.

## Painting or a filter does nothing

Check:

1. A raster layer is active.
2. The layer is visible.
3. The layer is unlocked.
4. The selection includes the target pixels.
5. Tool opacity, flow, and strength are above zero.
6. The pointer is inside the layer's document bounds.

Vector and adjustment layers cannot receive destructive raster brush/filter
operations. Choose a raster layer or use the tool's Vector mode.

If the canvas is busy with I/O, a plugin, or background removal, wait for that
operation to finish before editing.

## Copy, cut, or paste surprises

- **Copy/Cut to New Layer** uses the active raster layer and current selection.
  Cut needs an unlocked source; Copy can read a locked source.
- `Ctrl+C` copies the selected composite as sRGB RGBA8, not the full layer
  object.
- `Ctrl+V` creates a new raster layer and centers it at the pointer or canvas
  center.
- A paste larger than the canvas asks whether to resize.
- Clipboard alpha includes selection coverage.

For exact same-document placement, use the canvas context menu's **Copy to New
Layer** instead of clipboard round-tripping.

## Imported or exported image looks different

First identify whether the difference is expected:

- JPEG removes transparency over white and is lossy.
- ORA omits adjustment layers, vector layers, and masks on export.
- PSD import flattens hierarchy and imports blend modes as Normal.
- SVG Vector mode supports only a subset of SVG.
- SVG export embeds one flattened PNG.
- Current raster exporters do not embed ICC profile bytes.

See [File Formats](FILE_FORMATS.md) for the complete fidelity matrix.

For delivery, export a flat PNG, reopen it, and compare it with the `.ogre`
master.

## Native `.ogre` file will not open

The loader rejects malformed or unsafe data, including:

- Missing `OGRE` magic or unsupported version
- Empty canvas
- Dimensions above 8192
- Excessive manifest or tile sizes
- Invalid layer references
- Oversized vector geometry
- Corrupt MessagePack or zstd payloads

Keep the failing file unchanged for diagnosis. Report the exact error and a
minimal file privately if it may be a security problem. See
[Security Policy](../SECURITY.md).

Do not try to repair the binary container in place. Recover from a copy or an
autosave snapshot.

## Autosave and recovery

Autosave is controlled by **Settings → Autosave interval**. `0` disables it.
Recovery files are retained after an unclean exit and offered on the next
launch.

If no prompt appears:

1. Confirm autosave was enabled long enough to complete a snapshot.
2. Check the platform local-data directory for an `ogre/recovery` directory.
3. Copy any recovery files before another clean shutdown removes stale
   snapshots.

On a typical Linux setup, local data is under:

```text
$XDG_DATA_HOME/ogre/
```

or:

```text
$HOME/.local/share/ogre/
```

Platform directory rules can vary. Do not assume the fallback
`.arte-ogre/recovery` unless normal platform directory discovery failed.

Recovery preserves the saved document state, not earlier undo history or UI
layout.

## Preferences will not load

The Settings window displays the exact `prefs.toml` path. Exit Arte Ogre, move
that file aside, then relaunch:

```sh
mv /exact/path/prefs.toml /exact/path/prefs.toml.backup
```

Arte Ogre recreates defaults. Copy wanted values from the backup instead of
restoring a malformed file wholesale.

Typical Linux configuration location:

```text
$XDG_CONFIG_HOME/ogre/prefs.toml
```

or:

```text
$HOME/.config/ogre/prefs.toml
```

## AI background refinement

### First run is slow

First use downloads an approximately 170 MiB IS-Net model. The UI permits up to
600 seconds for AI refinement. Later runs use the cached model.

Cache path:

```text
$XDG_CACHE_HOME/arte-ogre/isnet-general-use.onnx
```

or:

```text
$HOME/.cache/arte-ogre/isnet-general-use.onnx
```

The download goes to a `.part` file, must match the expected byte length and
pinned SHA-256 checksum, then is atomically renamed.

### Download or checksum fails

Check connectivity to GitHub Releases and available cache disk space. A proxy,
captive portal, or HTML error response will fail size or checksum validation.

Preserve a suspect file, then move it aside and retry:

```sh
mv "$HOME/.cache/arte-ogre/isnet-general-use.onnx" \
  "$HOME/.cache/arte-ogre/isnet-general-use.onnx.backup"
```

Use the resolved `XDG_CACHE_HOME` path instead when set.

If AI remains unavailable, disable **Refine edges with AI**. Standard matte
removal still runs, and an AI inference failure automatically falls back to the
standard result.

### Build has no AI option

The executable may have been built with:

```sh
cargo build --release -p ogre --no-default-features
```

Use the normal default-feature build to include `ml`.

## Check for Updates fails

**Help → Check for Updates** contacts the GitHub API with a 10-second timeout.
Editing is unaffected by a failure. Check releases manually:

```text
https://github.com/visorcraft/Arte-Ogre/releases
```

Arte Ogre has no telemetry. Update checks and the optional model download are
the only explicit network paths.

## Plugin problems

Open **Plugins → Plugin Manager** and read the exact red error.

Common checks:

- Bundle is one immediate child of the displayed plugin directory.
- `plugin.toml` is valid UTF-8 TOML and no larger than 64 KiB.
- `kind` is exactly `lua` or `wasm`.
- `entry` is a relative contained path and exists.
- WASM has exported `memory` and the required `process` signature.
- A WASM run has an active raster layer.
- A tile filter is point-wise and does not need neighboring tiles.

The UI stops waiting after 60 seconds. Lua also has memory and instruction
limits; WASM has fuel and linear-memory limits. Full details and exact ABI are
in [Plugin Author Guide](PLUGINS.md).

## Slow interaction or high memory use

- Close unused document tabs.
- Keep canvases and imported sources no larger than needed.
- Hide expensive layer stacks while editing.
- Prefer sparse raster layers instead of filling enormous transparent bounds.
- Avoid repeated full-document filters when a selection can bound the work.
- Remember that duplicated untouched tiles are cheap, but editing both copies
  causes copy-on-write tile allocation.
- Large AI refinement and export operations run in the background but still
  consume CPU and memory.

Dirty-tile compositing keeps normal edits local, but a global adjustment,
transform, export, or cross-document operation can legitimately touch the full
canvas.

## Report a bug

For a normal bug, open a GitHub issue and include:

- Arte Ogre version from **Help → About**
- Distribution, desktop environment, Wayland or X11
- GPU and driver from `vulkaninfo --summary`
- Exact steps from a new blank document
- Exact terminal error
- Whether `WGPU_BACKEND=vulkan` or `ARTE_OGRE_X11=1` changes it
- A minimal non-sensitive `.ogre` or source file when file-specific
- Screenshot or short recording for UI/visual bugs

Do not attach confidential artwork. Do not open a public issue for a security
problem; use the private process in [Security Policy](../SECURITY.md).
