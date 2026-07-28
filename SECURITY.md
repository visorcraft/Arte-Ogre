# Security Policy

Arte Ogre is a local-first, no-telemetry desktop image editor. It still parses
untrusted input - image files, native `.ogre`/`.ora` documents, SVG, and local
WASM/Lua plugins - so we take security reports seriously.

## Supported versions

Security fixes land on the latest `1.x` release line and `master`.

| Version | Supported |
| ------- | --------- |
| 1.x     | ✅        |
| < 1.0   | ❌        |

## Reporting a vulnerability

**Please do not open a public issue for security problems.**

Report privately through GitHub:

1. Go to the repository's **Security** tab.
2. Click **Report a vulnerability** to open a private advisory.

If you cannot use GitHub Security Advisories, reach the maintainers via
[visorcraft.com](https://www.visorcraft.com).

Please include:

- The affected version or commit.
- A description of the issue and its impact.
- Steps to reproduce, ideally with a minimal sample file or plugin.

We aim to acknowledge a report within a few days and to keep you updated as
we investigate. Please give us a reasonable window to ship a fix before any
public disclosure; we're happy to credit you in the release notes.

## Scope

In scope:

- **File loaders** (`ogre-io`) - `.ogre`, `.ora`, PSD, SVG/SVGZ, EXR, TIFF,
  PNG, JPEG, WebP, BMP, and other enabled raster decoders. Maliciously crafted
  files that cause crashes, unbounded memory or CPU use, path traversal, or
  memory-safety issues.
- **Plugin sandbox** (`ogre-plugins`) - escapes from the `wasmtime` fuel/memory
  caps, WASM capability boundary, or Lua sandbox.
- **The engine** (`ogre-core`, `ogre-gpu`) - memory-safety or correctness bugs
  reachable from untrusted input.
- **Network boundary** - update/model requests that upload document data,
  accept an unverified model, or write outside their intended cache path.

Out of scope:

- Availability of the GitHub API or the upstream model host.
- Third-party dependency advisories already tracked and fixed upstream, unless
  Arte Ogre exposes additional impact.
- Issues that require a local attacker who already controls your account and
  can replace application or plugin files.

## Hardening notes

- `ogre-core` is built with `#![deny(unsafe_code)]`.
- The native `.ogre` loader bounds canvas, buffer, and vector geometry on
  load, rejecting oversized or malformed manifests instead of allocating
  blindly.
- Tile decoding caps the decompressed size of each tile.
- WASM plugins receive no host imports or WASI capabilities. They run under a
  fuel limit and a 512 MiB linear-memory cap.
- Lua exposes only constrained document commands. Filesystem, process, package,
  dynamic-code-loading, and debug globals are removed; memory is capped at
  64 MiB and execution at 10,000,000 instructions.
- Plugin manifests are capped at 64 KiB and entry values must be contained
  relative paths with no absolute path or `..` component.
- AI model downloads stream to a `.part` file, require the expected size and a
  pinned SHA-256 checksum, then install by atomic rename.
- Arte Ogre has no telemetry.

## Network behavior

Normal editing, plugins, save/export, and model inference are local. Network
access happens only after an explicit user action:

- **Help → Check for Updates** queries the GitHub API with a short timeout.
- First use of **Refine edges with AI** downloads the pinned IS-Net model from
  GitHub Releases.

Neither path uploads document or image content.
