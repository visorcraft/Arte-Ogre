#!/usr/bin/env bash
# Regenerate CREDITS.md and THIRD_PARTY_LICENSES.md from the live Cargo.lock.
#
#   scripts/licenses.sh
#
# Single source of truth for both files. The crate set comes from `cargo-about`
# (about.toml + about.hbs); our own workspace crates are excluded via
# `publish = false` + `[private] ignore = true` in about.toml. Run after any
# dependency change and commit the result. Idempotent: re-running with the same
# Cargo.lock + cargo-about version yields no diff.
#
# Requires: cargo-about (`cargo install cargo-about`).
set -euo pipefail

cd "$(dirname "$0")/.."
command -v cargo-about >/dev/null || { echo "error: cargo-about not installed (cargo install cargo-about)" >&2; exit 1; }

DATE="$(date +%F)"
CREDITS=CREDITS.md
TPL=THIRD_PARTY_LICENSES.md

# --- THIRD_PARTY_LICENSES.md: full license texts, grouped by cargo-about ---
{
  cat <<EOF
# Third-Party Licenses

This document lists the licenses of the third-party Rust crates distributed with Arte Ogre.
It was generated automatically from the workspace \`Cargo.lock\` on ${DATE} using \`cargo-about\`.

For a concise table of contributors, repositories, and licenses, see [CREDITS.md](CREDITS.md).

EOF
  cargo about generate about.hbs
  cat <<'EOF'
## KDE Breeze icons

The symbolic icon assets used in the Arte Ogre tool palette are vendored from the KDE Breeze icons project.
They are located in `crates/ogre-ui/assets/icons/`.

- **License:** LGPL-3.0-or-later / LGPL-2.1-only
- **Upstream:** https://invent.kde.org/frameworks/breeze-icons
- **Local details:** `crates/ogre-ui/assets/icons/LICENSE`
- **Full license texts:**
  - https://www.gnu.org/licenses/lgpl-3.0.html
  - https://www.gnu.org/licenses/old-licenses/lgpl-2.1.html
EOF
} > "$TPL"

# --- CREDITS.md: one sorted row per crate (deduped across shared licenses) ---
ROWS_HBS="$(mktemp --suffix=.hbs)"
trap 'rm -f "$ROWS_HBS"' EXIT
cat > "$ROWS_HBS" <<'EOF'
{{#each licenses}}{{#each used_by}}| {{crate.name}} | {{crate.version}} | {{{crate.license}}} | {{#if crate.repository}}[{{{crate.repository}}}]({{{crate.repository}}}){{else}}—{{/if}} |
{{/each}}{{/each}}
EOF
{
  cat <<EOF
# Credits and Acknowledgements

Arte Ogre builds on the work of many open-source projects. This file lists the third-party Rust crates bundled in the application and the asset sources we redistribute.

This file was generated automatically from \`Cargo.lock\` on ${DATE} using [\`cargo-about\`](https://github.com/EmbarkStudios/cargo-about).

## Rust crates

| Crate | Version | License | Repository |
|-------|---------|---------|------------|
EOF
  cargo about generate "$ROWS_HBS" | sed '/^[[:space:]]*$/d' | LC_ALL=C sort -u
  cat <<'EOF'

## JavaScript packages

This project does not currently use any JavaScript packages.

## Assets

| Asset | Source | License |
|-------|--------|---------|
| KDE Breeze icons | https://invent.kde.org/frameworks/breeze-icons | LGPL-3.0-or-later / LGPL-2.1-only |

The default fonts shipped with egui are covered by the crate entries above (see `epaint_default_fonts`).
EOF
} > "$CREDITS"

echo "regenerated $CREDITS and $TPL ($DATE)"
