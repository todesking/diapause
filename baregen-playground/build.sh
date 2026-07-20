#!/usr/bin/env bash
# Builds the playground as a self-contained static site in dist/.
#
# Usage: ./build.sh
#
# Prerequisites:
#   rustup target add wasm32-unknown-unknown
#   cargo install wasm-pack   # or any other wasm-pack install method
#
# Output layout (dist/):
#   index.html, style.css, main.js   copied from www/
#   vendor/viz.js                    vendored Graphviz bundle, from www/
#   pkg/baregen_playground.js        wasm-bindgen glue (wasm-pack --target web)
#   pkg/baregen_playground_bg.wasm   the compiled transform
set -euo pipefail

cd "$(dirname "$0")"

if ! command -v wasm-pack >/dev/null 2>&1; then
    echo "error: wasm-pack not found in PATH" >&2
    echo "install it with: cargo install wasm-pack" >&2
    exit 1
fi

# wasm-pack output is an intermediate artifact; dist/ is the site.
# --no-typescript / --no-pack: the site loads the ESM glue directly, so
# .d.ts files and package.json would be dead weight.
wasm-pack build --target web --release --no-typescript --no-pack --out-dir pkg

rm -rf dist
mkdir -p dist/pkg
cp -R www/. dist/
cp pkg/baregen_playground.js pkg/baregen_playground_bg.wasm dist/pkg/

echo "playground assembled in $(pwd)/dist"
echo "serve it with: python3 -m http.server --directory dist"
