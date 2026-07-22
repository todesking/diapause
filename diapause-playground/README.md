# diapause playground

A browser playground for the `#[diapause::coroutine(...)]` transform.
Paste an annotated function and see the expanded code, the control-flow
graphs (raw and simplified), and positioned errors — all rendered
locally, with no server-side component.

The site is plain static files: the transform runs as WebAssembly and
Graphviz rendering uses a vendored [@viz-js/viz](https://www.npmjs.com/package/@viz-js/viz)
bundle, so no CDN or network access is needed at runtime.

## Prerequisites

- Rust with the wasm target: `rustup target add wasm32-unknown-unknown`
- [wasm-pack](https://rustwasm.github.io/wasm-pack/): `cargo install wasm-pack`

## Build

```sh
./build.sh
```

Extra arguments are forwarded to `wasm-pack build`. Notably, wasm-pack
downloads `wasm-opt` (binaryen) from GitHub releases on first use; on a
network where that download fails, skip the optimization pass with:

```sh
./build.sh --no-opt
```

This compiles the crate to wasm and assembles the complete site in
`dist/`:

```
dist/
├── index.html
├── main.js
├── style.css
├── pkg/
│   ├── diapause_playground.js       wasm-bindgen glue
│   └── diapause_playground_bg.wasm  the compiled transform
└── vendor/
    └── viz.js                      Graphviz bundle (see www/vendor/README.md)
```

`dist/` is self-contained; deploy it to any static file host as-is.
Pushes to `main` deploy it to GitHub Pages automatically
(`.github/workflows/deploy-playground.yml`), served at
<https://todesking.github.io/diapause/>.

## Run locally

The page uses ES modules and `fetch`, so it must be served over HTTP
(opening `index.html` via `file://` will not work):

```sh
python3 -m http.server --directory dist 8000
```

Then open <http://localhost:8000/>.

## Directory layout

- `src/` — the wasm crate: wraps `diapause-macro-core` in a
  `transform(source) -> report` API (expansion, CFG DOT, errors).
- `www/` — the static front end (HTML/CSS/JS sources).
- `pkg/` — wasm-pack output (generated, gitignored).
- `dist/` — the assembled site (generated, gitignored).
