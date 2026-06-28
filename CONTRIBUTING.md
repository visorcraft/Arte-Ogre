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

## Development workflow

Arte Ogre follows a **test-driven, checkbox-driven** flow:

> write a failing test → implement → verify it passes → commit.

A peer code review ends each significant change.

## Before you push: the quality gate

Every change must pass the per-crate gate — **warnings are hard errors**:

```sh
cargo test   -p <crate>
cargo clippy -p <crate> --all-targets -- -D warnings
cargo fmt    --check
cargo doc    -p <crate>
```

`scripts/gate.sh` wraps test + clippy + fmt for `ogre-core`. `ogre-core`
is `#![deny(unsafe_code)]`. Proptest suites run with `PROPTEST_CASES=1024`;
release-only perf/golden tests are `#[ignore]`'d and run with `--release
--ignored`.

`Cargo.lock` is tracked — Arte Ogre ships a binary app — so commit lockfile
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

- **`ogre-core`** — headless engine and **ground truth**: tiled buffers,
  layers/documents, selection, history, commands, ops, the CPU reference
  compositor.
- **`ogre-gpu`** — interactive `wgpu` compositor; recomposites only dirty
  tiles per edit.
- **`ogre-ui`** — the `eframe`/`egui_dock` app shell, tools, and panels.
- **`ogre`** — the thin binary.
- **`ogre-io`** — file formats: native `.ogre`, `.ora`, PSD/EXR/TIFF/PNG/
  JPEG/WebP, ICC color.
- **`ogre-plugins`** — sandboxed WASM (`wasmtime`) and Lua (`mlua`) filters.
- **`ogre-vector`** — vector path rasterization.

## Engine invariants (violating these = silent correctness bugs)

- **Single mutation path.** Every document edit is an `ogre-core` `Command`
  pushed onto `History` (undoable, marks the renderer dirty). Never mutate
  `Document` directly from UI or GPU code.
- **GPU is golden-tested against CPU.** Every GPU compositor output must
  match `ogre_core::composite_document` within `1e-4` per channel. The CPU
  compositor is ground truth.
- **Pixel format.** `Rgba32F` — straight (non-premultiplied) alpha, **linear**
  light, origin top-left, +y down.
- **Tile space.** Tiles are 256×256, stored in layer-local space
  (`doc_coord = local_coord + layer.offset`). Tile math uses **floored**
  division (`div_euclid`/`rem_euclid`), never truncating `/` or `%`.
- **Killer feature.** The exact-position Cut/Copy-to-New-Layer guarantee is
  pinned by byte-identical round-trip tests; keep them green.
- **Untrusted input is validated.** File loaders and plugins parse untrusted
  data — bound allocations and reject malformed manifests rather than
  trusting on-disk sizes.

## Coding standards

- Rust 2021. Format with `cargo fmt`; no `#[allow(...)]` to silence a lint
  without a one-line justification.
- Prefer the smallest change that works. Match the style of the surrounding
  code.
- Document public items; `cargo doc` must build clean.

## Attribution

Commits are authored solely by the human committer. **Never** add an AI or
agent as a contributor anywhere — no `Co-Authored-By` trailers, no
"Generated with…" lines, and no mention of any AI assistant in commit
messages, PR descriptions, code, comments, or docs.

## Licence

Arte Ogre is **GPL-3.0-only**. By contributing, you agree your contributions
are licensed under the same terms. See [LICENSE](LICENSE).
