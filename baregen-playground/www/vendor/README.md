# Vendored dependencies

## viz.js

- Package: [`@viz-js/viz`](https://www.npmjs.com/package/@viz-js/viz) 3.28.0 (MIT)
- File: `dist/viz.js` from the npm tarball, copied verbatim.
- Why vendored: the playground must work as static files with no CDN
  access. This build is fully self-contained (Graphviz compiled to
  plain JS via Emscripten; no separate `.wasm` fetch at runtime).

To update, download the npm tarball (`npm pack @viz-js/viz`) and copy
`package/dist/viz.js` over `viz.js`.

## highlight.js

- Package: [`@highlightjs/cdn-assets`](https://www.npmjs.com/package/@highlightjs/cdn-assets)
  11.11.1 (BSD-3-Clause)
- Files: `es/core.min.js`, `es/languages/rust.min.js` (browser ESM
  builds), and `styles/github.min.css`, copied verbatim into
  `highlight/`.
- Why this package: the `highlight.js` npm package's `es/` entry points
  re-export CommonJS files and only work under Node's interop, not as
  browser modules; the cdn-assets package ships true single-file ESM.

To update, download the npm tarball (`npm pack @highlightjs/cdn-assets`)
and copy the three files over `highlight/`.
