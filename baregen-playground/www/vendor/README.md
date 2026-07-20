# Vendored dependencies

## viz.js

- Package: [`@viz-js/viz`](https://www.npmjs.com/package/@viz-js/viz) 3.28.0 (MIT)
- File: `dist/viz.js` from the npm tarball, copied verbatim.
- Why vendored: the playground must work as static files with no CDN
  access. This build is fully self-contained (Graphviz compiled to
  plain JS via Emscripten; no separate `.wasm` fetch at runtime).

To update, download the npm tarball (`npm pack @viz-js/viz`) and copy
`package/dist/viz.js` over `viz.js`.
