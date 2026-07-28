# Contributing to Arte Ogre

Thanks for taking the time to help Arte Ogre. This document describes how
to propose a change, what we expect from a pull request, and the coding
standards and engine invariants that apply to the codebase.

If anything here is unclear or out of date, open an issue or a PR.

## Code of conduct

Be kind, be specific, assume good faith. Disagree about the technical
details, not the person. Public reviews stay focused on the diff.

## How to propose a change

Arte Ogre uses a standard **fork → branch → pull request** workflow on
GitHub.

1. **Fork** [`visorcraft/Arte-Ogre`](https://github.com/visorcraft/Arte-Ogre)
   to your account.
2. **Clone** your fork and add the upstream remote:

   ```sh
   git clone git@github.com:<you>/Arte-Ogre.git
   cd Arte-Ogre
   git remote add upstream https://github.com/visorcraft/Arte-Ogre.git
   ```

3. **Branch** from `master`. Pick a descriptive, kebab-case branch name:
   `fix-tile-deserialize-bounds`, `feature/quick-selection`,
   `docs/contributing-update`.

   ```sh
   git fetch upstream
   git switch -c my-change upstream/master
   ```

4. **Make focused commits.** One logical change per commit. Run the
   quality gate (below) before pushing.
5. **Open a pull request** against `master` on `visorcraft/Arte-Ogre`:
   - **What.** One-paragraph summary of the change.
   - **Why.** Bug fix? New feature? Doc fix? Link the issue if one exists.
   - **How to test.** The exact commands a reviewer should run.
   - **Risk.** What might break? What did you not test?

PRs that touch UI behavior should include a screenshot or a short
recording. PRs that touch the compositor, the file formats, or the tile
engine should call out which invariant tests cover the change.

## Development setup

Prerequisites:

- Current stable Rust toolchain with Cargo
- Linux development environment
- Vulkan-capable GPU and working Vulkan drivers for interactive/GPU testing
- `cargo-about` only when regenerating third-party license files
- `linuxdeploy`, `appimagetool`, and ImageMagick only for AppImage packaging

Clone and launch:

```sh
git clone https://github.com/visorcraft/Arte-Ogre.git
cd Arte-Ogre
cargo run -p ogre
```

Arte Ogre currently has no command-line file or option interface. Open test
files with **File → Open**.

Useful builds:

```sh
cargo build --release -p ogre
cargo run -p ogre --no-default-features
```

The first command produces a production-mode binary for testing, not a
shippable release. The second omits optional AI matte refinement.

Feature flags:

- `ogre`, `ogre-ui`, and `ogre-io`: `ml` enables IS-Net matte refinement
  through `tract-onnx`. It is enabled by default in the shipped binary.
- `ogre-io`: `svg` enables SVG import/export and is enabled by default.
- `ogre-gpu`: `egui` enables `CanvasRenderer::register_with_egui`.

## Development workflow

Arte Ogre follows a **test-driven, checkbox-driven** flow:

> write a failing test → implement → verify it passes → commit.

A peer code review ends each significant change.

## Before you push: the quality gate

Every change must pass the per-crate gate - **warnings are hard errors**:

```sh
cargo test   -p <crate>
cargo clippy -p <crate> --all-targets -- -D warnings
cargo fmt    --check
cargo doc    -p <crate>          # use --no-deps to skip dependency docs
```

`scripts/gate.sh` wraps test + clippy + fmt for `ogre-core`. `ogre-core`
is `#![deny(unsafe_code)]`. Proptest suites run with `PROPTEST_CASES=1024`;
release-only perf/golden tests are `#[ignore]`'d and run with `--release
--ignored`.

Exact extended checks:

```sh
PROPTEST_CASES=1024 cargo test -p ogre-core
cargo test -p ogre-gpu --release --test composite_perf -- --ignored
cargo machete
```

High-value invariant tests:

```sh
cargo test -p ogre-core copy_to_new_layer_retains_exact_position
cargo test -p ogre-core --test killer_feature cut_move_undo_redo_loop_is_byte_identical
cargo test -p ogre-core --test vector_parity
```

For a cross-crate change, use the workspace gate:

```sh
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --check
cargo doc --workspace --no-deps
```

`Cargo.lock` is tracked - Arte Ogre ships a binary app - so commit lockfile
changes alongside the dependency change that caused them. After a dependency
change, regenerate the license docs with `scripts/licenses.sh` (needs
`cargo-about`).

## Architecture at a glance

A Cargo workspace, one responsibility per crate:

```
ogre-core ─► ogre-gpu ─► ogre-ui ─► ogre (binary)
   ▲            ▲           ▲
   └── ogre-io ─┴─ ogre-plugins ─┘   ogre-vector ─► ogre-core
```

- **`ogre-core`** - headless engine and **ground truth**: tiled buffers,
  layers/documents, selection, history, commands, ops, the CPU reference
  compositor.
- **`ogre-gpu`** - interactive `wgpu` compositor; recomposites only dirty
  tiles per edit.
- **`ogre-ui`** - the `eframe`/`egui_dock` app shell, tools, and panels.
- **`ogre`** - the thin binary.
- **`ogre-io`** - file formats: native `.ogre`, `.ora`, PSD/EXR/TIFF/PNG/
  JPEG/WebP, ICC color.
- **`ogre-plugins`** - sandboxed WASM (`wasmtime`) and Lua (`mlua`) filters.
- **`ogre-vector`** - vector path rasterization.

See [Architecture](docs/ARCHITECTURE.md) for the data model, coordinates,
command flow, renderer, background workers, and change map.

## Engine invariants (violating these = silent correctness bugs)

- **Single mutation path.** Every document edit is an `ogre-core` `Command`
  pushed onto `History` (undoable, marks the renderer dirty). Never mutate
  `Document` directly from UI or GPU code.
- **GPU is golden-tested against CPU.** Every GPU compositor output must
  match `ogre_core::composite_document` within `1e-4` per channel. The CPU
  compositor is ground truth.
- **Pixel format.** `Rgba32F` - straight (non-premultiplied) alpha, **linear**
  light, origin top-left, +y down.
- **Tile space.** Tiles are 256×256, stored in layer-local space
  (`doc_coord = local_coord + layer.offset`). Tile math uses **floored**
  division (`div_euclid`/`rem_euclid`), never truncating `/` or `%`.
- **Killer feature.** The exact-position Cut/Copy-to-New-Layer guarantee is
  pinned by byte-identical round-trip tests; keep them green.
- **Untrusted input is validated.** File loaders and plugins parse untrusted
  data - bound allocations and reject malformed manifests rather than
  trusting on-disk sizes.

## Coding standards

- Rust 2021. Format with `cargo fmt`; no `#[allow(...)]` to silence a lint
  without a one-line justification.
- Prefer the smallest change that works. Match the style of the surrounding
  code.
- Document public items; `cargo doc` must build clean.

## Documentation changes

Public behavior belongs in the public docs:

- User-visible workflow or shortcut: [User Guide](docs/USER_GUIDE.md)
- Import/export behavior: [File Formats](docs/FILE_FORMATS.md)
- Crate, data-flow, or invariant change: [Architecture](docs/ARCHITECTURE.md)
- Plugin manifest, Lua API, or WASM ABI: [Plugin Author Guide](docs/PLUGINS.md)
- Known failure or diagnostic: [Troubleshooting](docs/TROUBLESHOOTING.md)
- Landing-page claim or setup path: [README](README.md)

Keep limitations explicit. Do not promise an interface that executable code
does not implement. Check relative links and run `git diff --check`.

## Release packaging

A release is the single-file AppImage built in release mode. A bare
`target/release/ogre` binary is not a shippable Arte Ogre release.

Before packaging:

1. Start from a clean checkout of the intended release commit.
2. Update crate versions consistently and refresh `Cargo.lock`.
3. Run the workspace quality gate above.
4. If dependencies changed, run `scripts/licenses.sh` and review
   `CREDITS.md` plus `THIRD_PARTY_LICENSES.md`.
5. Review README version/feature claims and public compatibility docs.

Build:

```sh
scripts/build_appimage.sh
```

The script:

- Runs `cargo build --release -p ogre`
- Installs the binary inside the AppDir as `arte-ogre`
- Builds the 256×256 icon from `assets/ArteOgre.png`
- Installs the `arte-ogre.desktop` entry
- Runs `linuxdeploy` with `NO_STRIP=1` for `DT_RELR` compatibility
- Produces `target/appimage/Arte_Ogre-x86_64.AppImage`

Smoke-test the exact artifact:

```sh
target/appimage/Arte_Ogre-x86_64.AppImage
```

At minimum, create a document, paint, use Copy/Cut to New Layer, undo/redo,
save/reopen `.ogre`, and export/reopen PNG. Test native Wayland and
`ARTE_OGRE_X11=1` when both are available. Publish that tested AppImage, not a
separately rebuilt binary.

## Attribution

Commits are authored solely by the human committer. **Never** add an AI or
agent as a contributor anywhere - no `Co-Authored-By` trailers, no
"Generated with…" lines, and no mention of any AI assistant in commit
messages, PR descriptions, code, comments, or docs.

## Licence

Arte Ogre is **GPL-3.0-only**. By contributing, you agree your contributions
are licensed under the same terms. See [LICENSE](LICENSE).
