# Security Policy

Arte Ogre is an offline, no-telemetry desktop image editor. It still parses
untrusted input — image files, native `.ogre`/`.ora` documents, and sandboxed
WASM/Lua plugins — so we take security reports seriously.

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

- **File loaders** (`ogre-io`) — `.ogre`, `.ora`, PSD, EXR, TIFF, PNG, JPEG,
  WebP. Maliciously crafted files that cause crashes, unbounded memory or CPU
  use, or memory-safety issues.
- **Plugin sandbox** (`ogre-plugins`) — escapes from the `wasmtime` fuel/memory
  caps or the Lua sandbox.
- **The engine** (`ogre-core`, `ogre-gpu`) — memory-safety or correctness bugs
  reachable from untrusted input.

Out of scope:

- The optional AI matte-refine model download (only fetched when you ask for
  it) and third-party dependency advisories already tracked upstream.
- Issues that require a local attacker who already controls your machine.

## Hardening notes

- `ogre-core` is built with `#![deny(unsafe_code)]`.
- The native `.ogre` loader bounds canvas, buffer, and vector geometry on
  load, rejecting oversized or malformed manifests instead of allocating
  blindly.
- Tile decoding caps the decompressed size of each tile.
- Plugins run under a `wasmtime` fuel limit and a 512 MiB memory cap.
- No network calls, no telemetry.
