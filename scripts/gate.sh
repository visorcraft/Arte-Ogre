#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."

cargo test -p ogre-core
cargo clippy -p ogre-core --all-targets -- -D warnings
cargo fmt --check
