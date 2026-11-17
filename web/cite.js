// brain-cite — an element inspector for citing UI regions back to the agent.
//
// Why this exists as a plain script rather than wasm/app code: it is a
// measurement tool over whatever the app happens to render, so it must not
// depend on the app's component tree, its stores, or a rebuild. It reads the
// DOM and nothing else. The app root is #app; everything outside it is ours.
//
// Contract:
//   E           arm / disarm (ignored while a form control has focus)
//   Esc         disarm
//   hover       outline the candidate + fill the reference panel
//   click       clear every .hilite
//   click+drag  lasso; the outermost fully-contained elements get .hilite and
//               a plaintext reference block goes to the clipboard on release
//
// "Outermost fully-contained" is the rule that makes a lasso legible. Mark
// every contained element and a lasso over a card paints the card *and* its
// paragraphs *and* their inline spans, so the yellow is a solid block with no
// structure. Marking only elements whose parent is not contained gives one
// band per top-level thing inside the lasso, which is what a reader means by
// "these paragraphs".
//
// .hilite survives disarming on purpose: the point is to leave marks on the
// page for the agent to find via the browser tools, not just to copy text.

const ROOT = "#app";
const DRAG_THRESHOLD_PX = 4;
const TEXT_SNIPPET = 300;

let armed = false;
let target = null; // hovered element
let drag = null; // {x0,y0,x,y,moved}
let lastCopy = null;

// A completed lasso must survive the `click` the browser fires when the drag
// ends. That click arrives *after* mouseup, by which point `drag` has already
// been released, so a guard reading `drag.moved` is necessarily false by then
// and the click handler wipes the marks the lasso just painted -- the citation
// copies correctly while the page looks untouched, which is worse than failing
// loudly. The flag is set on release and consumed by that click, or expires.
let lassoJustFinished = false;
let lassoFlagTimer = 0;

const disabled = () => localStorage.getItem("brainCite") === "0";

// ── ui ──────────────────────────────────────────────────────────────────────

function ui(tag, cls) {
  const el = document.createElement(tag);
  el.className = cls;
  el.setAttribute("data-cite-ui", "");
  return el;
}

const icon = ui("div", "cite-icon");
icon.innerHTML =
  '<svg viewBox="0 0 24 24" width="17" height="17" aria-hidden="true">' +
  '<path fill="#f5c518" stroke="#1a1d24" stroke-width="1.2" ' +
  'd="M14.6 3.2 20.8 9.4 9.9 20.3 3.7 14.1Z M8.2 18.6 5.4 21.5 2 22l.5-3.4 3.1-3.1Z"/>' +
  '<path fill="#1a1d24" d="m15.9 4.5 3.6 3.6-1.3 1.3-3.6-3.6Z"/></svg>';

const panel = ui("div", "cite-panel");
panel.innerHTML =
  '<div class="cite-head"><span>brain-cite</span><b class="cite-count"></b></div>' +
  '<dl class="cite-dl">' +
  '<div><dt>url</dt><dd class="f-url"></dd></div>' +
  '<div><dt>xpath</dt><dd class="f-xpath"></dd></div>' +
  '<div><dt>element</dt><dd class="f-tag"></dd></div>' +
  '<div><dt>href</dt><dd class="f-href"></dd></div>' +
  '<div><dt>bounds</dt><dd class="f-bounds"></dd></div>' +
  '<div><dt>scroll</dt><dd class="f-scroll"></dd></div>' +
  '<div><dt>text</dt><dd class="f-text"></dd></div>' +
  "</dl>" +
  '<div class="cite-hint">click&nbsp;=&nbsp;clear &nbsp;·&nbsp; drag&nbsp;=&nbsp;lasso+copy</div>';

const lasso = ui("div", "cite-lasso");
const toast = ui("div", "cite-toast");
const layers = [icon, panel, lasso, toast];

let mounted = false;
function mount() {
  if (mounted) return;
  mounted = true;
  for (const el of layers) document.body.appendChild(el);
}
function unmount() {
  for (const el of layers) el.remove();
  mounted = false;
}

function showPanel(on) {
  panel.classList.toggle("show", on);
  icon.classList.toggle("show", on);
  if (!on) lasso.classList.remove("show");
}

function place(x, y) {
  icon.style.transform = `translate(${x}px, ${y}px)`;
  const pad = 14;
  const r = panel.getBoundingClientRect();
  let px = x + 18;
  let py = y + 18;
  if (px + r.width > innerWidth - pad) px = x - r.width - 18;
  if (py + r.height > innerHeight - pad) py = y - r.height - 18;
  panel.style.transform = `translate(${Math.max(pad, px)}px, ${Math.max(pad, py)}px)`;
}

function set(field, value) {
  const el = panel.querySelector(field);
  if (el) el.textContent = value;
}

// ── measurement ─────────────────────────────────────────────────────────────

const n = (v) => Math.round(v);

// The inspector's own state classes are not part of the element's identity, so
// they must never appear in a citation: reporting `p.cite-target` describes the
// tool, not the thing being cited.
const OURS = new Set(["cite-target", "hilite"]);
function classList(el) {
  return (el.getAttribute("class") || "")
    .trim()
    .split(/\s+/)
    .filter((c) => c && !OURS.has(c))
    .slice(0, 3);
}
function tagOf(el) {
  const cls = classList(el);
  return el.tagName.toLowerCase() + (cls.length ? "." + cls.join(".") : "");
}

function xpath(el) {
  if (!(el instanceof Element)) return "";
  if (el.id) return `//*[@id="${el.id}"]`;
  const seg = [];
  for (let cur = el; cur && cur.nodeType === 1; cur = cur.parentElement) {
    if (cur.id) {
      seg.unshift(`/*[@id="${cur.id}"]`);
      return "/" + seg.join("/").replace(/^\/+/, "");
    }
    const sib = cur.parentElement
      ? Array.prototype.filter.call(
          cur.parentElement.children,
          (c) => c.tagName === cur.tagName,
        )
      : [cur];
    const i = sib.indexOf(cur) + 1;
    seg.unshift(sib.length > 1 ? `${cur.tagName.toLowerCase()}[${i}]` : cur.tagName.toLowerCase());
  }
  return "/" + seg.join("/");
}

function describe(el) {
  const r = el.getBoundingClientRect();
  set(".f-url", location.href);
  set(".f-xpath", xpath(el) || "—");
  set(".f-tag", tagOf(el));
  // Shown on hover for the same reason it is copied: anchor labels are short by
  // design, so the destination is the only place the full path survives.
  const anchor = el.matches("a[href]") ? el : el.closest("a[href]");
  set(".f-href", anchor ? anchor.getAttribute("href") : "—");
  set(".f-bounds", `x=${n(r.left)} y=${n(r.top)} w=${n(r.width)} h=${n(r.height)}`);
  set(".f-scroll", `x=${n(scrollX)} y=${n(scrollY)}`);
  const text = (el.innerText || el.textContent || "").replace(/\s+/g, " ").trim();
  set(".f-text", text.length > 400 ? text.slice(0, 400) + "…" : text || "—");
}

function clearTarget() {
  if (target) target.classList.remove("cite-target");
  target = null;
}

function setTarget(el) {
  if (el === target) return;
  clearTarget();
  target = el;
  if (target) target.classList.add("cite-target");
}

// ── lasso ───────────────────────────────────────────────────────────────────

// Hand-drawn lassos land a few pixels short of the edge they meant to reach.
// Without slack, being 1px short of a paragraph silently changes the answer
// from "this paragraph" to "the three inline spans inside it" — a different
// citation than the one that was drawn. 3px is enough for the pointer to be
// approximate without letting a deliberate half-selection claim a whole block.
const CONTAIN_TOL_PX = 3;

const inside = (r, b) =>
  r.left >= b.left - CONTAIN_TOL_PX &&
  r.top >= b.top - CONTAIN_TOL_PX &&
  r.right <= b.right + CONTAIN_TOL_PX &&
  r.bottom <= b.bottom + CONTAIN_TOL_PX;

function boxFrom(x0, y0, x1, y1) {
  return {
    left: Math.min(x0, x1),
    top: Math.min(y0, y1),
    right: Math.max(x0, x1),
    bottom: Math.max(y0, y1),
  };
}

// The outermost fully-contained elements with rendered text.
function lassoHits(x0, y0, x1, y1) {
  const b = boxFrom(x0, y0, x1, y1);
  const root = document.querySelector(ROOT);
  if (!root) return [];
  const contained = new Set();
  for (const el of root.querySelectorAll("*")) {
    if (el.closest("[data-cite-ui]")) continue;
    const t = el.tagName;
    if (t === "SCRIPT" || t === "STYLE" || t === "LINK" || t === "META") continue;
    const r = el.getBoundingClientRect();
    if (r.width < 1 || r.height < 1) continue;
    if (!(el.innerText || el.textContent || "").trim()) continue;
    if (inside(r, b)) contained.add(el);
  }
  const hits = [];
  for (const el of contained) {
    let hasContainedAncestor = false;
    for (let p = el.parentElement; p; p = p.parentElement) {
      if (contained.has(p)) {
        hasContainedAncestor = true;
        break;
      }
    }
    if (!hasContainedAncestor) hits.push(el);
  }
  // Reading order, so the copied list matches what the eye sees.
  return hits.sort((a, b2) => {
    const ra = a.getBoundingClientRect();
    const rb = b2.getBoundingClientRect();
    return Math.abs(ra.top - rb.top) > 3 ? ra.top - rb.top : ra.left - rb.left;
  });
}

function drawLasso(x0, y0, x1, y1) {
  const b = boxFrom(x0, y0, x1, y1);
  lasso.style.left = `${b.left}px`;
  lasso.style.top = `${b.top}px`;
  lasso.style.width = `${b.right - b.left}px`;
  lasso.style.height = `${b.bottom - b.top}px`;
  lasso.classList.add("show");
}

// The selection is the .hilite class set, not its paint. Marks stay on the DOM
// after E disarms -- that is the handle the agent reads -- but they are only
// *visible* while body.citing is present, so disarming hides a stale-looking
// highlight without destroying the citation.
const hasSelection = () => document.querySelectorAll(".hilite").length > 0;

function setCount(k) {
  panel.querySelector(".cite-count").textContent = k ? `${k} selected` : "";
}

function clearHilite() {
  for (const el of document.querySelectorAll(".hilite")) el.classList.remove("hilite");
  setCount(0);
}

// ── clipboard ───────────────────────────────────────────────────────────────

function reference(hits, b, via) {
  const lines = [
    "brain-cite reference",
    `url:     ${location.href}`,
    `when:    ${new Date().toISOString()}`,
    `scroll:  x=${n(scrollX)} y=${n(scrollY)}`,
    `via:     ${via}`, // lasso | click
    `region:  x=${n(b.left)} y=${n(b.top)} w=${n(b.right - b.left)} h=${n(b.bottom - b.top)}`,
    `matched: ${hits.length} element(s) — .hilite applied in the DOM`,
    "",
  ];
  hits.forEach((el, i) => {
    const r = el.getBoundingClientRect();
    const text = (el.innerText || el.textContent || "").replace(/\s+/g, " ").trim();
    const anchor = el.matches("a[href]") ? el : el.closest("a[href]");
    lines.push(
      `[${i + 1}] xpath:  ${xpath(el)}`,
      `    tag:    ${tagOf(el)}`,
      // The href is emitted because anchor labels are deliberately short (a
      // filename, not a path): without it the reference cannot say what the
      // link points at, and the citation is unreconstructable from text alone.
      ...(anchor ? [`    href:   ${anchor.getAttribute("href")}`] : []),
      `    bounds: x=${n(r.left)} y=${n(r.top)} w=${n(r.width)} h=${n(r.height)}`,
      `    text:   ${JSON.stringify(text.length > TEXT_SNIPPET ? text.slice(0, TEXT_SNIPPET) + "…" : text)}`,
      "",
    );
  });
  return lines.join("\n");
}

async function copy(text, count) {
  lastCopy = { text, count, at: Date.now() };
  try {
    await navigator.clipboard.writeText(text);
  } catch {
    // Clipboard API needs focus + a secure context; 127.0.0.1 qualifies, but a
    // blurred window or an older browser does not. The textarea path is the
    // fallback, and `last` keeps the reference retrievable either way.
    const ta = ui("textarea", "cite-fallback");
    ta.value = text;
    document.body.appendChild(ta);
    ta.select();
    try {
      document.execCommand("copy");
    } catch {}
    ta.remove();
  }
  flash(count ? `copied ${count} reference${count === 1 ? "" : "s"}` : "lasso matched nothing — click to clear");
}

let toastTimer = 0;
function flash(msg) {
  toast.textContent = msg;
  toast.classList.add("show");
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => toast.classList.remove("show"), 1800);
}

// ── modes ───────────────────────────────────────────────────────────────────

function arm() {
  if (armed || disabled()) return;
  armed = true;
  mount();
  document.documentElement.classList.add("cite-armed");
  document.body.classList.add("citing"); // reveals .hilite -- see hasSelection()
  showPanel(true);
  set(".f-url", location.href);
  set(".f-xpath", "—");
  set(".f-tag", "—");
  set(".f-bounds", "—");
  set(".f-scroll", `x=${n(scrollX)} y=${n(scrollY)}`);
  set(".f-text", "hover something…");
  // A selection from before is still live, so report its true count, not 0.
  setCount(document.querySelectorAll(".hilite").length);
}

function disarm() {
  if (!armed) return;
  armed = false;
  drag = null;
  clearTarget();
  document.documentElement.classList.remove("cite-armed");
  document.body.classList.remove("citing");
  showPanel(false);
  // .hilite intentionally persists on the DOM — that is the handle for the
  // agent. Whether it is *visible* is a body.citing CSS concern, not a data one.
}

// ── events ──────────────────────────────────────────────────────────────────

const inForm = (t) =>
  !!t &&
  (t.tagName === "INPUT" ||
    t.tagName === "TEXTAREA" ||
    t.tagName === "SELECT" ||
    t.tagName === "BUTTON" ||
    t.isContentEditable);

document.addEventListener(
  "keydown",
  (e) => {
    if (disabled() || e.metaKey || e.ctrlKey || e.altKey) return;
    if (e.key === "Escape" && armed) {
      disarm();
      return;
    }
    if (e.key.toLowerCase() !== "e") return;
    if (inForm(e.target) || inForm(document.activeElement)) return; // typing, not pointing
    e.preventDefault();
    armed ? disarm() : arm();
  },
  true,
);

document.addEventListener(
  "mousemove",
  (e) => {
    if (!armed) return;
    const { clientX: x, clientY: y } = e;
    place(x, y);
    if (drag) {
      if (!drag.moved && Math.hypot(x - drag.x0, y - drag.y0) > DRAG_THRESHOLD_PX) drag.moved = true;
      if (drag.moved) {
        drag.x = x;
        drag.y = y;
        drawLasso(drag.x0, drag.y0, x, y);
      }
      return;
    }
    const el = document.elementFromPoint(x, y);
    if (!el || el.closest("[data-cite-ui]")) return;
    setTarget(el);
    describe(el);
  },
  true,
);

// A click while armed is a gesture in our tool, never in the app: stop the SPA
// from navigating out from under the citation.
// A click is one gesture with two meanings, decided by whether a selection
// already exists: with marks present it is "clear"; with none it is "take this
// one element", which is the fast path for citing a single field and the reason
// a hover outline exists at all.
const selectable = (el) => {
  if (!(el instanceof Element) || el.closest("[data-cite-ui]")) return null;
  const root = document.querySelector(ROOT);
  if (!root || !root.contains(el)) return null;
  const r = el.getBoundingClientRect();
  if (r.width < 1 || r.height < 1) return null;
  if (!(el.innerText || el.textContent || "").trim()) return null;
  return el;
};

document.addEventListener(
  "click",
  async (e) => {
    if (!armed) return;
    if (e.target.closest("[data-cite-ui]")) return;
    e.preventDefault();
    e.stopPropagation();
    // The trailing click of a lasso is neither a clear nor a new selection.
    if (lassoJustFinished) {
      lassoJustFinished = false;
      clearTimeout(lassoFlagTimer);
      return;
    }
    if (hasSelection()) {
      clearHilite();
      flash("cleared");
      return;
    }
    // Prefer the element the hover outline is on -- that is the one the user is
    // aiming at -- and fall back to hit-testing the click point.
    const el =
      selectable(target) ?? selectable(document.elementFromPoint(e.clientX, e.clientY));
    if (!el) {
      flash("nothing to select here");
      return;
    }
    const r = el.getBoundingClientRect();
    el.classList.add("hilite");
    setCount(1);
    await copy(
      reference([el], { left: r.left, top: r.top, right: r.right, bottom: r.bottom }, "click"),
      1,
    );
  },
  true,
);

document.addEventListener(
  "mousedown",
  (e) => {
    if (!armed || e.button !== 0) return;
    if (e.target.closest("[data-cite-ui]")) return; // panel is interactive; don't lasso from it
    e.preventDefault();
    // A fresh press supersedes any unconsumed flag, so a click that never fired
    // cannot swallow this gesture's clear.
    lassoJustFinished = false;
    clearTimeout(lassoFlagTimer);
    drag = { x0: e.clientX, y0: e.clientY, x: e.clientX, y: e.clientY, moved: false };
  },
  true,
);

document.addEventListener(
  "mouseup",
  async (e) => {
    if (!armed || !drag) return;
    const d = drag;
    drag = null;
    if (!d.moved) return; // a plain click: the click handler owns that case
    lasso.classList.remove("show");
    const hits = lassoHits(d.x0, d.y0, e.clientX, e.clientY);
    clearHilite();
    for (const el of hits) el.classList.add("hilite");
    setCount(hits.length);
    // Guard the click the browser is about to fire for this same drag.
    lassoJustFinished = true;
    clearTimeout(lassoFlagTimer);
    lassoFlagTimer = setTimeout(() => {
      lassoJustFinished = false;
    }, 350);
    await copy(reference(hits, boxFrom(d.x0, d.y0, e.clientX, e.clientY), "lasso"), hits.length);
  },
  true,
);

// ── handle ──────────────────────────────────────────────────────────────────

window.brainCite = {
  on: arm,
  off: disarm,
  clear: clearHilite,
  get armed() {
    return armed;
  },
  get last() {
    return lastCopy;
  },
  hilited: () => Array.from(document.querySelectorAll(".hilite")).map(xpath),
  selected: () => document.querySelectorAll(".hilite").length,
  help: () =>
    "press E to arm; hover for a reference; with no selection a click cites the hovered element " +
    "(marks it .hilite and copies a reference), and with a selection a click clears it; " +
    "click+drag lassos, marks .hilite and copies. Marks stay on the DOM when E disarms but are " +
    "hidden until you re-arm. brainCite.last holds the last copy.",
};

if (!disabled()) console.info("brain-cite: press E to inspect and cite, Esc to exit. window.brainCite for the API.");
