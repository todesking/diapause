// Playground front end: pipes the textarea through the wasm transform
// and renders the expansion, both CFG DOT graphs, and positioned errors.
//
// Expected layout next to this file (assembled by the static build):
//   pkg/     wasm-pack --target web output of baregen-playground
//   vendor/  self-contained @viz-js/viz ESM bundle

import { instance as vizInstance } from "./vendor/viz.js";
import hljs from "./vendor/highlight/core.min.js";
import rust from "./vendor/highlight/rust.min.js";

hljs.registerLanguage("rust", rust);

// Applies only to the transform + CFG/error rendering; the syntax
// layer repaints on every keystroke, since the textarea's own text is
// transparent and typing would otherwise be invisible until the
// debounce fires.
const DEBOUNCE_MS = 120;

// Selectable inputs, ordered simplest first. All are lifted from the
// baregen test suite, so they are known-good transforms; helpers they
// call (parse, combine, inner_sum, ...) are irrelevant to the macro,
// which never resolves names outside the function.
const EXAMPLES = [
  {
    name: "Basic yields",
    code: `#[baregen::coroutine(yield = u32)]
fn counter() {
    yield_!(1);
    yield_!(2);
}
`,
  },
  {
    name: "For loop + resume values",
    code: `#[baregen::coroutine(yield = u32, resume = u32)]
fn running_total(n: u32) -> u32 {
    let mut sum: u32 = 0;
    for i in 0u32..n {
        let bonus = yield_!(sum);
        sum += i + bonus;
    }
    sum
}
`,
  },
  {
    name: "Match arms",
    code: `#[baregen::coroutine(yield = u32, resume = u32)]
fn match_yield(x: u32) -> u32 {
    let mut out: u32 = 0;
    match x {
        0 => {
            let r = yield_!(10);
            out += r;
        }
        _ => {
            let r = yield_!(20);
            out += r * 2;
        }
    }
    out
}
`,
  },
  {
    name: "Loop with break",
    code: `#[baregen::coroutine(yield = u32, resume = bool)]
fn until_stop() -> u32 {
    let mut count: u32 = 0;
    loop {
        let stop = yield_!(count);
        if stop {
            yield_!(999);
            break;
        }
        count += 1;
    }
    count
}
`,
  },
  {
    name: "? operator",
    code: `#[baregen::coroutine(yield = u32)]
fn sum_two(a: &'static str, b: &'static str) -> Result<u32, ParseError> {
    let x: u32 = parse(a)?;
    yield_!(x);
    let y: u32 = parse(b)?;
    Ok(x + y)
}
`,
  },
  {
    name: "yield_all! delegation",
    code: `#[baregen::coroutine(yield = u32, resume = u32)]
fn outer(n: u32) -> u32 {
    let before = yield_!(n);
    let g: inner_sum::State = inner_sum(before);
    let sub: u32 = yield_all!(g);
    yield_!(sub);
    sub + n
}
`,
  },
  {
    name: "Expression-position yields",
    code: `#[baregen::coroutine(yield = u32, resume = u32)]
fn call_args(a: u32) -> u32 {
    combine(yield_!(a), yield_!(a + 1), seven(), a)
}
`,
  },
];

const DEFAULT_EXAMPLE = 1; // For loop + resume values

const el = {
  status: document.getElementById("status"),
  editor: document.getElementById("editor"),
  source: document.getElementById("source"),
  examples: document.getElementById("examples"),
  highlightLayer: document.getElementById("highlight-layer"),
  highlightContent: document.getElementById("highlight-content"),
  syntaxLayer: document.getElementById("syntax-layer"),
  syntaxContent: document.getElementById("syntax-content"),
  errors: document.getElementById("errors"),
  expanded: document.getElementById("expanded"),
  cfgSimplified: document.getElementById("view-cfg-simplified"),
  cfgRaw: document.getElementById("view-cfg-raw"),
  tabs: Array.from(document.querySelectorAll(".tabs [role=tab]")),
};

function showStatus(message) {
  el.status.textContent = message;
  el.status.hidden = message === "";
}

// --- Tabs ---------------------------------------------------------------

for (const tab of el.tabs) {
  tab.addEventListener("click", () => {
    for (const other of el.tabs) {
      const selected = other === tab;
      other.setAttribute("aria-selected", String(selected));
      document.getElementById(other.dataset.target).hidden = !selected;
    }
  });
}

// --- Error highlighting in the input ------------------------------------

/**
 * Converts a transform error position (1-based line, 0-based column in
 * characters) to a UTF-16 offset into `source`, clamped to the text.
 */
function toOffset(source, line, col) {
  const lines = source.split("\n");
  const lineIdx = Math.min(Math.max(line - 1, 0), lines.length - 1);
  let offset = 0;
  for (let i = 0; i < lineIdx; i++) offset += lines[i].length + 1;
  let taken = 0;
  for (const ch of lines[lineIdx]) {
    if (taken >= col) break;
    offset += ch.length;
    taken += 1;
  }
  return offset;
}

/** Returns merged, sorted [start, end) UTF-16 ranges for the errors. */
function errorRanges(source, errors) {
  const ranges = errors
    .map((e) => {
      const start = toOffset(source, e.line, e.col);
      let end = toOffset(source, e.end_line, e.end_col);
      // Zero-width spans (e.g. "unexpected end of input") would be
      // invisible; widen them to one character when possible.
      if (end <= start) end = Math.min(start + 1, source.length);
      return [start, end];
    })
    .filter(([s, e]) => e > s)
    .sort((a, b) => a[0] - b[0]);
  const merged = [];
  for (const [s, e] of ranges) {
    const last = merged[merged.length - 1];
    if (last && s <= last[1]) last[1] = Math.max(last[1], e);
    else merged.push([s, e]);
  }
  return merged;
}

function renderHighlights(source, errors) {
  el.highlightContent.textContent = "";
  let pos = 0;
  for (const [start, end] of errorRanges(source, errors)) {
    el.highlightContent.append(source.slice(pos, start));
    const mark = document.createElement("mark");
    mark.textContent = source.slice(start, end);
    el.highlightContent.append(mark);
    pos = end;
  }
  // The trailing newline keeps the layer as tall as the textarea.
  el.highlightContent.append(source.slice(pos) + "\n");
  syncHighlightScroll();
}

function syncHighlightScroll() {
  for (const layer of [el.highlightLayer, el.syntaxLayer]) {
    layer.scrollTop = el.source.scrollTop;
    layer.scrollLeft = el.source.scrollLeft;
  }
}

// --- Syntax highlighting ------------------------------------------------

/** Highlighted `code` as HTML; falls back to escaped plain text. */
function rustHtml(code) {
  try {
    return hljs.highlight(code, { language: "rust" }).value;
  } catch (err) {
    console.error(err);
    const div = document.createElement("div");
    div.textContent = code;
    return div.innerHTML;
  }
}

function renderSyntax(source) {
  // The trailing newline keeps the layer as tall as the textarea.
  el.syntaxContent.innerHTML = rustHtml(source) + "\n";
  // Let the stacked (narrow) layout grow the editor with its content;
  // reserve room for the textarea's horizontal scrollbar when lines
  // overflow. The wide layout ignores this and fills the viewport.
  // scrollHeight never reports less than the current height, so drop
  // the height to the minimum first or the editor would never shrink.
  el.editor.style.setProperty("--editor-content-height", "0px");
  const scrollbar =
    el.syntaxLayer.scrollWidth > el.syntaxLayer.clientWidth ? 20 : 0;
  el.editor.style.setProperty(
    "--editor-content-height",
    `${el.syntaxLayer.scrollHeight + scrollbar}px`,
  );
  syncHighlightScroll();
}

el.source.addEventListener("scroll", syncHighlightScroll);

// --- Error list ---------------------------------------------------------

function renderErrorList(source, errors) {
  el.errors.textContent = "";
  el.errors.hidden = errors.length === 0;
  for (const error of errors) {
    const item = document.createElement("li");
    const pos = document.createElement("span");
    pos.className = "pos";
    // Display columns 1-based, as compilers do.
    pos.textContent = `${error.line}:${error.col + 1}: `;
    item.append(pos, error.message);
    item.title = "Click to jump to this position";
    item.addEventListener("click", () => {
      const start = toOffset(source, error.line, error.col);
      el.source.focus();
      el.source.setSelectionRange(start, start);
      scrollLineIntoView(error.line);
    });
    el.errors.append(item);
  }
}

function scrollLineIntoView(line) {
  const style = getComputedStyle(el.source);
  const lineHeight = parseFloat(style.lineHeight) || 19;
  const target = (line - 1) * lineHeight;
  const view = el.source.clientHeight;
  if (target < el.source.scrollTop || target > el.source.scrollTop + view - lineHeight) {
    el.source.scrollTop = Math.max(0, target - view / 2);
  }
  syncHighlightScroll();
}

// --- CFG rendering ------------------------------------------------------

const vizPromise = vizInstance();

/** Renders `dot` to an element to place in a CFG view (never throws). */
async function renderCfg(dot, emptyMessage) {
  if (dot == null) {
    const p = document.createElement("p");
    p.className = "placeholder";
    p.textContent = emptyMessage;
    return p;
  }
  try {
    const viz = await vizPromise;
    return viz.renderSVGElement(dot);
  } catch (err) {
    const failure = document.createElement("div");
    failure.className = "render-error";
    failure.textContent = `Graphviz rendering failed: ${err}\n\n${dot}`;
    return failure;
  }
}

// --- Transform pipeline -------------------------------------------------

async function loadTransform() {
  try {
    const module = await import("./pkg/baregen_playground.js");
    await module.default();
    return module.transform;
  } catch (err) {
    console.error(err);
    showStatus(
      "Failed to load the wasm module (pkg/baregen_playground.js). " +
        "Build it with wasm-pack and serve the assembled directory.",
    );
    return null;
  }
}

let generation = 0;

async function run(transform) {
  const source = el.source.value;
  const report = transform(source);
  const current = ++generation;
  renderSyntax(source);
  renderHighlights(source, report.errors);
  renderErrorList(source, report.errors);
  el.expanded.innerHTML = rustHtml(
    report.expanded !== ""
      ? report.expanded
      : report.errors.length > 0
        ? "// expansion failed; see errors"
        : "",
  );
  // Graphviz rendering is async; drop results that a newer input has
  // superseded.
  const [simplified, raw] = await Promise.all([
    renderCfg(report.cfg_dot_simplified, "No CFG (lowering failed)."),
    renderCfg(report.cfg_dot_raw, "No CFG (lowering failed)."),
  ]);
  if (current !== generation) return;
  el.cfgSimplified.replaceChildren(simplified);
  el.cfgRaw.replaceChildren(raw);
}

async function main() {
  for (const [i, example] of EXAMPLES.entries()) {
    const option = document.createElement("option");
    option.value = String(i);
    option.textContent = example.name;
    el.examples.append(option);
  }
  el.examples.value = String(DEFAULT_EXAMPLE);
  el.source.value = EXAMPLES[DEFAULT_EXAMPLE].code;
  renderSyntax(el.source.value);
  // Registered before the wasm load so typing stays visible (the
  // textarea's own text is transparent) even if the load fails.
  el.source.addEventListener("input", () => renderSyntax(el.source.value));

  const transform = await loadTransform();
  if (transform == null) return;

  let timer = null;
  el.source.addEventListener("input", () => {
    clearTimeout(timer);
    timer = setTimeout(() => run(transform), DEBOUNCE_MS);
  });

  el.examples.addEventListener("change", () => {
    const example = EXAMPLES[Number(el.examples.value)];
    if (!example) return;
    clearTimeout(timer);
    el.source.value = example.code;
    el.source.scrollTop = 0;
    el.source.scrollLeft = 0;
    run(transform);
  });

  await run(transform);
}

main();
