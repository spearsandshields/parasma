/**
 * Demo harness — proves docx/xlsx/pptx render with the polished chrome, and now
 * the OPT-IN explorer panel. This is the snippet in dev mode (the repo's
 * default dev surface):
 * the demo supplies the fixture entries + an open() resolver, and enables BOTH
 * { explorer, dev } — but the two flags are independent (Toggle dev turns dev off
 * while the explorer stays; "Clean viewer" remounts with NO explorer to prove
 * default-OFF = clean viewer).
 */
import { mountViewer, type ViewerHandle, type ExplorerConfig, type ExplorerEntry } from "../src/index";

const host = document.getElementById("host")!;

// The demo owns WHERE the entries come from. The snippet ships no fixtures —
// the FULL safe catalog is copied (gitignored) from the repo-root /fixtures
// corpus by scripts/copy-demo-fixtures.ts (predev), and the explorer entries
// are built from manifest.json. NEVER any fixtures/user/ file (skipped at copy).
type Fmt = "docx" | "xlsx" | "pptx" | "csv";
function fmtOf(p: string): Fmt {
  const s = p.toLowerCase();
  if (s.endsWith(".xlsx")) return "xlsx";
  if (s.endsWith(".pptx")) return "pptx";
  if (s.endsWith(".csv")) return "csv";
  return "docx";
}
interface ManifestItem {
  path: string;
  category: string;
  name: string;
}

// ── Upload persistence (IndexedDB) ──────────────────────────────────────────
// The HOST owns where files come from AND their caching (viewer-not-app
// boundary). Uploaded files are cached here so they survive a reload and show
// in the explorer's "Uploads" group (the "recents" pattern). The
// snippet ships none of this; it just renders the bytes the host hands it.
const IDB_NAME = "parasma-snippet-demo";
const IDB_STORE = "uploads";
const UPLOAD_PREFIX = "upload:";
interface Upload {
  id: string;
  name: string;
  format: Fmt;
  bytes: ArrayBuffer;
}
// ONE connection for the page's lifetime (every put/get/getAll used to open a
// fresh connection that was never closed). Cached as a promise so concurrent
// callers share the same open; reset on failure so a later call can retry.
let idbConn: Promise<IDBDatabase> | null = null;
function openIDB(): Promise<IDBDatabase> {
  idbConn ??= new Promise<IDBDatabase>((res, rej) => {
    const r = indexedDB.open(IDB_NAME, 1);
    r.onupgradeneeded = () => {
      if (!r.result.objectStoreNames.contains(IDB_STORE)) {
        r.result.createObjectStore(IDB_STORE, { keyPath: "id" });
      }
    };
    r.onsuccess = () => res(r.result);
    r.onerror = () => rej(r.error);
  }).catch((e) => {
    idbConn = null;
    throw e;
  });
  return idbConn;
}
async function idbPut(u: Upload): Promise<void> {
  const db = await openIDB();
  await new Promise<void>((res, rej) => {
    const tx = db.transaction(IDB_STORE, "readwrite");
    tx.objectStore(IDB_STORE).put(u);
    tx.oncomplete = () => res();
    tx.onerror = () => rej(tx.error);
  });
}
async function idbAll(): Promise<Upload[]> {
  const db = await openIDB();
  return new Promise((res, rej) => {
    const tx = db.transaction(IDB_STORE, "readonly");
    const req = tx.objectStore(IDB_STORE).getAll();
    req.onsuccess = () => res(req.result as Upload[]);
    req.onerror = () => rej(req.error);
  });
}
async function idbGet(id: string): Promise<ArrayBuffer | null> {
  const db = await openIDB();
  return new Promise((res, rej) => {
    const tx = db.transaction(IDB_STORE, "readonly");
    const req = tx.objectStore(IDB_STORE).get(id);
    req.onsuccess = () => res((req.result as Upload | undefined)?.bytes ?? null);
    req.onerror = () => rej(req.error);
  });
}
async function idbDelete(id: string): Promise<void> {
  const db = await openIDB();
  await new Promise<void>((res, rej) => {
    const tx = db.transaction(IDB_STORE, "readwrite");
    tx.objectStore(IDB_STORE).delete(id);
    tx.oncomplete = () => res();
    tx.onerror = () => rej(tx.error);
  });
}

// ?slots — tier-B slot showcase: a floating bottom page navigator built ONLY on
// the public API (slots.viewportOverlay factory + on("pageChange") + goToPage +
// state), plus a plain-element toolbarTrailing badge. Exercises BOTH slot source
// branches (factory and element).
const slotsDemo = new URLSearchParams(location.search).has("slots");

function buildFloatingNav(h: ViewerHandle): HTMLElement {
  const nav = document.createElement("div");
  nav.style.cssText =
    "position:absolute;bottom:18px;left:50%;transform:translateX(-50%);" +
    "display:flex;align-items:center;gap:10px;padding:6px 12px;" +
    "background:var(--ps-panel);border:1px solid var(--ps-hairline-strong);" +
    "box-shadow:var(--ps-shadow);font-family:var(--ps-font-ui);font-size:12px;color:var(--ps-text);";
  const label = document.createElement("span");
  label.style.cssText = "font-family:var(--ps-font-mono);font-feature-settings:'tnum' 1;";
  const btn = (text: string, delta: number): HTMLButtonElement => {
    const b = document.createElement("button");
    b.textContent = text;
    b.style.cssText =
      "appearance:none;background:transparent;border:1px solid var(--ps-hairline);" +
      "color:var(--ps-text);padding:2px 9px;cursor:pointer;font-size:13px;";
    b.addEventListener("click", () => h.goToPage(h.state.currentPage + delta));
    return b;
  };
  // Pages are a PAGED-surface concept (docx/pptx): spreadsheets render as the
  // continuous grid in the snippet (no print-preview mode), so the nav hides
  // for excel entirely. Kind comes from state on the load event.
  const sync = (): void => {
    label.textContent = `${h.state.currentPage} / ${h.state.pageCount}`;
    nav.style.display = h.state.documentKind === "excel" ? "none" : "flex";
  };
  h.on("pageChange", sync);
  h.on("load", sync);
  // pageCount STREAMS in after `load` (pageReady per page, loadComplete total) —
  // there is no public count event, so a BYO nav uses the documented kernel
  // escape hatch (the bus replays, so late subscription still catches up).
  h.kernel.bus.subscribe("pageReady", sync);
  h.kernel.bus.subscribe("loadComplete", sync);
  sync();
  nav.append(btn("‹", -1), label, btn("›", +1));
  return nav;
}

function buildSlotBadge(): HTMLElement {
  const badge = document.createElement("span");
  badge.textContent = "SLOT";
  badge.style.cssText =
    "font-family:var(--ps-font-mono);font-size:10px;letter-spacing:0.14em;" +
    "color:var(--ps-accent);border:1px solid var(--ps-accent);padding:2px 6px;";
  return badge;
}

// BYO-font overrides: normalized DOCUMENT family → blob: URL of user-sourced
// bytes. Read live by the fontResolver closure below (so an upload after mount
// only needs refreshFonts, and remounts inherit automatically).
const fontOverrides = new Map<string, string>();
const normFam = (s: string): string => s.toLowerCase().replace(/[^a-z0-9]/g, "");

let handle: ViewerHandle | null = null;
let dark = true;
let dev = true;
let useLocalFonts = false;
let explorerOn = true;
let explorer: ExplorerConfig | null = null;
let defaultSrc = "";

let fixtureEntries: ExplorerEntry[] = [];
let uploadEntries: ExplorerEntry[] = [];

// Uploads first (the "recents"/cached files), then the fixture catalog.
const allEntries = (): ExplorerEntry[] => [...uploadEntries, ...fixtureEntries];

// The entry currently open in the viewer — remounts (the Clean-viewer/zen and
// explorer toggles) re-open THIS, not the default fixture, so toggling chrome
// never yanks the user back to the opening doc.
let currentSrc = "";

// open(): upload ids resolve to their CACHED bytes (IndexedDB); fixture ids are URLs.
// A MISSING upload throws a clear error — handing back an empty ArrayBuffer used
// to surface as a cryptic engine zip error ("Could not find EOCD").
// Every explorer selection routes through here, so it doubles as the
// currentSrc tracker.
const openEntry = (id: string): Promise<ArrayBuffer> | string => {
  currentSrc = id;
  return id.startsWith(UPLOAD_PREFIX)
    ? idbGet(id).then((b) => {
        if (!b || b.byteLength === 0) throw new Error(`upload not found: ${id}`);
        return b;
      })
    : id;
};

// Build the explorer from the full fixture manifest (grouped format → category)
// PLUS any cached uploads from a previous session.
async function buildExplorer(): Promise<void> {
  const manifest = (await fetch("fixtures/manifest.json").then((r) => r.json())) as ManifestItem[];
  // Dedup by CATEGORY+NAME so the explorer shows no two leaves with the same
  // name within a group (cross-category same names are DISTINCT fixtures —
  // ppt/chart vs spreadsheet/chart; a bare-name dedup silently hid the xlsx
  // chart). The published manifest is already user-free + deduped the same way;
  // this is belt-and-suspenders at render time.
  const seenName = new Set<string>();
  fixtureEntries = manifest
    .filter((m) => !m.path.startsWith("fixtures/user/"))
    .filter((m) => {
      const nameKey = `${m.category}/${m.name}`;
      if (seenName.has(nameKey)) return false;
      seenName.add(nameKey);
      return true;
    })
    .map((m) => ({ id: m.path, name: m.name, format: fmtOf(m.path), group: m.category }));
  uploadEntries = (await idbAll().catch(() => [])).map((u) => ({
    id: u.id,
    name: u.name,
    format: u.format,
    group: "Uploads",
  }));
  // A sensible opening doc: first rich preference present, else the first entry.
  const prefs = [
    "fixtures/image/image-anchored.docx",
    "fixtures/text/imprint.docx",
    "fixtures/comment/comment-basic.docx",
  ];
  defaultSrc = prefs.find((p) => fixtureEntries.some((e) => e.id === p)) ?? fixtureEntries[0]?.id ?? "";
  explorer = { entries: allEntries(), open: openEntry, title: "Documents", selectedId: defaultSrc };
}

// Push the current entry set into the live panel (after an upload) + mark active.
function refreshExplorer(activeId?: string): void {
  if (explorer) explorer.entries = allEntries();
  handle?.explorer?.setEntries(allEntries());
  if (activeId) handle?.explorer?.setActive(activeId);
}

// SERIALIZED remount: an epoch guard + disabled toggles while one is in flight
// (REPRODUCED: double-click 'Clean viewer' → two live .parasma-snippet roots +
// a leaked kernel/wasm Viewer, eventually crashing the tab).
let remountSeq = 0;
function setTogglesDisabled(disabled: boolean): void {
  for (const id of ["explorer", "dev", "theme"]) {
    const btn = document.getElementById(id) as HTMLButtonElement | null;
    if (btn) btn.disabled = disabled;
  }
}
async function remount(): Promise<void> {
  const seq = ++remountSeq;
  setTogglesDisabled(true);
  // Snapshot live state the user may have changed INSIDE the viewer (toolbar ◐
  // theme toggle, ⌥⇧D dev chord, zoom, scroll position) so a remount doesn't
  // revert it — the Clean-viewer/zen toggle must feel like a chrome change, not
  // a reload back to the default fixture at 100%/page 1.
  let prevZoom: number | null = null;
  let prevPage: number | null = null;
  if (handle) {
    dark = handle.state.theme === "dark";
    dev = handle.state.dev;
    prevZoom = handle.state.zoom;
    prevPage = handle.state.currentPage;
  }
  const src = currentSrc || defaultSrc;
  if (explorer) explorer.selectedId = src;
  handle?.destroy();
  handle = null;
  try {
    // Mount WITHOUT options.src — the doc is re-opened explicitly below so the
    // upload-id case (IndexedDB bytes, not a URL) takes the same path as a
    // fixture URL, and zoom/page restore can await the load.
    // Dev override (?worker=true|false|auto): force the engine transport so the
    // worker-backed grid can be exercised against any format. Absent ⇒ the
    // batteries-included "auto" router (docx off-thread, xlsx/pptx in-process).
    const workerParam = new URLSearchParams(location.search).get("worker");
    const workerOverride =
      workerParam === "true" ? true : workerParam === "false" ? false : workerParam === "auto" ? "auto" : undefined;
    const h = await mountViewer(host, {
      theme: dark ? "dark" : "light",
      dev,
      useLocalFonts,
      explorer: explorerOn ? (explorer ?? undefined) : undefined,
      // BYO-font seam: route a family to the user-supplied blob URL when one is
      // loaded (checked by DOCUMENT name first, then the substitute family).
      fontResolver: (info) =>
        fontOverrides.get(normFam(info.documentFamily)) ??
        fontOverrides.get(normFam(info.family)) ??
        info.defaultUrl,
      ...(workerOverride !== undefined ? { worker: workerOverride } : {}),
      ...(slotsDemo
        ? {
            // The BYO-nav story done COHERENTLY: the floating navigator
            // REPLACES the toolbar's cluster (toolbarPageNav:false), never
            // duplicates it.
            slots: { viewportOverlay: buildFloatingNav, toolbarTrailing: buildSlotBadge() },
            toolbarPageNav: false,
          }
        : {}),
    });
    // Superseded while mounting (another remount started): destroy, don't adopt.
    if (seq !== remountSeq) {
      h.destroy();
      return;
    }
    handle = h;
    // Dev affordance: the live ViewerHandle on the console (pairs with __pv,
    // the raw wasm viewer) for load/goToPage/state probing during debugging.
    (globalThis as { __ph?: ViewerHandle }).__ph = h;
    // Keep the demo's theme flag in sync with the viewer's own toolbar toggle,
    // so the next remount (and the demo's Theme button) starts from reality.
    h.on("themeChange", (t) => {
      dark = t === "dark";
    });
    // Re-open the doc that was open before the remount (upload → cached bytes;
    // fixture → URL), then restore zoom + page best-effort. Falls back to the
    // default fixture if the entry vanished (e.g. evicted upload).
    try {
      if (src.startsWith(UPLOAD_PREFIX)) {
        const bytes = await idbGet(src);
        if (!bytes || bytes.byteLength === 0) throw new Error(`upload not found: ${src}`);
        await h.load(bytes, src.slice(UPLOAD_PREFIX.length));
      } else if (src) {
        await h.loadUrl(src);
      }
      currentSrc = src;
    } catch {
      if (defaultSrc && src !== defaultSrc) {
        await h.loadUrl(defaultSrc);
        currentSrc = defaultSrc;
      }
    }
    if (seq !== remountSeq) return; // superseded during the load — newest wins
    h.explorer?.setActive(currentSrc);
    // Restore zoom/page AFTER the mount's own post-load zoom pass (the PPTX
    // fit-page runs one rAF after load; restoring earlier gets overridden a
    // frame later — observed: 1.5 restored, then clobbered to the 0.26 fit).
    await new Promise<void>((r) => requestAnimationFrame(() => requestAnimationFrame(() => r())));
    if (seq !== remountSeq) return;
    if (prevZoom !== null && Math.abs(prevZoom - 1) > 0.001) h.zoom.set(prevZoom);
    if (prevPage !== null && prevPage > 1) h.goToPage(prevPage);
  } finally {
    if (seq === remountSeq) setTogglesDisabled(false);
  }
}

void (async () => {
  await buildExplorer();
  await remount();
})();

document.getElementById("theme")!.addEventListener("click", () => {
  // Toggle from the VIEWER's live state (the in-viewer ◐ also flips it).
  dark = !(handle ? handle.state.theme === "dark" : dark);
  handle?.setTheme(dark ? "dark" : "light");
});

document.getElementById("dev")!.addEventListener("click", () => {
  // Toggle from the VIEWER's live state (the in-viewer ⌥⇧D also flips it).
  dev = !(handle ? handle.state.dev : dev);
  handle?.setDev(dev);
});

// Local-fonts toggle — LIVE: setLocalFontsEnabled (inert setter)
// + viewer.refreshFonts() re-fetches the intent-changed faces (the intent-keyed
// session cache re-fetches on a flip), overwrites the wasm faces, and re-lays
// out the open document in place — no remount, no cache-bust needed. Falls back
// to a remount only when no viewer is mounted yet. Dev only (the byte-source is
// dev-injected).
const localFontsBtn = document.getElementById("localfonts") as HTMLButtonElement;
localFontsBtn.addEventListener("click", () => {
  useLocalFonts = !useLocalFonts;
  localFontsBtn.textContent = `Local fonts: ${useLocalFonts ? "ON" : "OFF"}`;
  localFontsBtn.setAttribute("aria-pressed", useLocalFonts ? "true" : "false");
  const h = handle;
  if (h) {
    h.viewer.setLocalFontsEnabled(useLocalFonts);
    h.viewer.refreshFonts().catch((err) => {
      console.warn("[demo] refreshFonts failed:", err);
    });
  } else {
    void remount(); // no viewer yet — the mount option carries the flag
  }
});

// Prove default-OFF: remount WITHOUT options.explorer ⇒ clean viewer, no sidebar.
// FOCUS — a real document-only mode, not a duplicate of the toolbar's ☰
// explorer collapse: it hides ALL chrome (toolbar/formula/tabs via the
// chromeless tier, the explorer panel, and the dev instrument), leaving the
// document edge-to-edge; pressing again (or Esc) restores exactly what was
// open before. Pure visibility flips — NO remount (a remount destroys the
// wasm Viewer + its session font cache → ~1s CJK re-parse per toggle).
const focusBtn = document.getElementById("explorer") as HTMLButtonElement;
focusBtn.textContent = "Focus";
focusBtn.title = "Document-only view — Esc or press again to restore";
let focusOn = false;
let preFocus: { explorerOpen: boolean; dev: boolean } | null = null;
function setFocus(on: boolean): void {
  const h = handle;
  if (!h || on === focusOn) return;
  focusOn = on;
  if (on) {
    preFocus = { explorerOpen: h.explorer?.isOpen() ?? false, dev: h.state.dev };
    h.setControls(false);
    h.explorer?.toggle(false);
    h.setDev(false);
  } else {
    h.setControls(true);
    h.explorer?.toggle(preFocus?.explorerOpen ?? true);
    h.setDev(preFocus?.dev ?? dev);
    preFocus = null;
  }
  focusBtn.setAttribute("aria-pressed", on ? "true" : "false");
}
focusBtn.addEventListener("click", () => setFocus(!focusOn));
document.addEventListener("keydown", (e) => {
  if (e.key === "Escape" && focusOn) setFocus(false);
});

// Upload (button + drag-drop) — demonstrates the HOST supplying bytes to the
// viewer via handle.load(). The snippet itself ships no upload UI (viewer-not-app
// boundary); the consumer owns file selection, exactly as shown here.
async function loadFile(file: File): Promise<void> {
  const bytes = await file.arrayBuffer();
  const id = UPLOAD_PREFIX + file.name;
  const format = fmtOf(file.name);
  // Cache FIRST (structured-clone into IndexedDB while bytes are intact), then
  // add/replace its "Uploads" entry, surface it in the panel, and render it.
  await idbPut({ id, name: file.name, format, bytes }).catch(() => {});
  uploadEntries = [
    { id, name: file.name, format, group: "Uploads" },
    ...uploadEntries.filter((e) => e.id !== id),
  ];
  refreshExplorer(id);
  currentSrc = id; // remounts re-open this upload, not the default fixture
  try {
    await handle?.load(bytes, file.name);
  } catch (e) {
    // A CORRUPT file must not persist: evict the IDB record + its Uploads entry,
    // or it re-errors every session from the cache.
    await idbDelete(id).catch(() => {});
    uploadEntries = uploadEntries.filter((x) => x.id !== id);
    refreshExplorer();
    console.error(`[demo] failed to load ${file.name}; evicted from uploads:`, e);
  }
}
const fileInput = document.getElementById("file") as HTMLInputElement;
document.getElementById("open")!.addEventListener("click", () => fileInput.click());
fileInput.addEventListener("change", () => {
  const f = fileInput.files?.[0];
  if (f) void loadFile(f);
  fileInput.value = ""; // allow re-picking the same file
});

// ── BYO font (substitution testing) ─────────────────────────────────────────
// "Load font…" renders a DOCUMENT family with a user-sourced .ttf/.otf — e.g.
// prove a Word-vs-Parasma page-break drift is font metrics by loading the exact
// face Word substitutes (Segoe UI, Verdana, …). Built ENTIRELY on public seams:
// the file becomes a blob: URL the fontResolver returns for that family, and
// refreshFonts() re-resolves + re-lays-out live. Nothing is uploaded anywhere —
// the bytes never leave the browser (we cannot and do not redistribute fonts;
// the user sources the file themselves).
const fontBtn = document.getElementById("font") as HTMLButtonElement;
const fontFile = document.getElementById("fontfile") as HTMLInputElement;
fontBtn.addEventListener("click", () => fontFile.click());
fontFile.addEventListener("change", () => {
  const f = fontFile.files?.[0];
  fontFile.value = "";
  if (!f) return;
  const guess = f.name.replace(/\.(ttf|otf)$/i, "").replace(/[-_](regular|bold|italic|bolditalic)$/i, "");
  const fam = prompt(
    "Render which DOCUMENT font family with this file?\n(the name the doc declares, e.g. \"Open Sans\" or \"DejaVu Sans\")",
    guess,
  );
  if (!fam?.trim()) return;
  const key = normFam(fam);
  const prev = fontOverrides.get(key);
  if (prev) URL.revokeObjectURL(prev);
  fontOverrides.set(key, URL.createObjectURL(f));
  fontBtn.textContent = `Fonts: ${fontOverrides.size}`;
  // REMOUNT, not refreshFonts: the core's loaded-face dedupe is intent-keyed
  // (cdn|family|weight|style — the resolver URL is deliberately not part of the
  // key) and SESSION-level, so neither refresh nor a doc reload re-fetches a
  // family whose face already loaded. A fresh mount consults the resolver
  // clean. (A resolver-URL-aware live refresh is not implemented in core yet.)
  void remount();
});
// Drag-drop onto the viewer host.
host.addEventListener("dragover", (e) => {
  e.preventDefault();
  // Literal accent: #host is OUTSIDE .parasma-snippet, so var(--ps-accent) is
  // out of scope here and the old fallback (#6b8afd blue) always won.
  host.style.outline = "2px dashed #e0a33e";
});
host.addEventListener("dragleave", () => (host.style.outline = ""));
host.addEventListener("drop", (e) => {
  e.preventDefault();
  host.style.outline = "";
  const f = e.dataTransfer?.files?.[0];
  if (f) void loadFile(f);
});

// Expose for manual poking in the console (demo only).
(window as unknown as { __demo: () => ViewerHandle | null }).__demo = () => handle;
(window as unknown as { __upload: (f: File) => Promise<void> }).__upload = loadFile;

// ── Perf probe (grid performance harness) ───────────────────────────────────
// Console/automation driver for the live perf gates. Files arrive via the normal
// host-upload path (never from fixtures); `load` bypasses the IndexedDB
// recents cache so the timing is pure load→loadComplete.
//   await __perf.load(file)   → ms from handle.load() to the "loadComplete" bus event
//   await __perf.scrollTo(y)  → ms from scrollTop=y to the 2nd rAF (frame committed)
//   await __perf.scrollEnd()  → scrollTo(bottom); returns { ms, scrollHeight }
//   __perf.gridInfo()         → { scrollHeight, clientHeight, scrollTop } of .ps-gridview
const perfProbe = {
  async load(file: File): Promise<number> {
    const h = handle;
    if (!h) throw new Error("__perf.load: no viewer mounted");
    const bytes = await file.arrayBuffer();
    const t0 = performance.now();
    // Subscribe BEFORE load() — loadComplete can fire before load() resolves.
    // The bus replays the sticky "latest" loadComplete SYNCHRONOUSLY during
    // subscribe: that delivery is the PREVIOUS doc's completion (and ran before
    // `un` initialized — the TDZ console error). Skip deliveries until the
    // subscribe call has returned, then count only the real event.
    const done = new Promise<void>((res) => {
      let live = false;
      const un = h.kernel.bus.subscribe("loadComplete", () => {
        if (!live) return; // sticky replay of the prior doc — not this load
        un();
        res();
      });
      live = true;
    });
    await h.load(bytes, file.name);
    await done;
    return performance.now() - t0;
  },
  gridScroller(): HTMLElement {
    const el = host.querySelector<HTMLElement>(".ps-gridview");
    if (!el) throw new Error("__perf: no .ps-gridview (grid mode not active?)");
    return el;
  },
  async scrollTo(y: number): Promise<number> {
    const sc = perfProbe.gridScroller();
    const t0 = performance.now();
    sc.scrollTop = y;
    await new Promise(requestAnimationFrame);
    await new Promise(requestAnimationFrame);
    return performance.now() - t0;
  },
  async scrollEnd(): Promise<{ ms: number; scrollHeight: number }> {
    const sc = perfProbe.gridScroller();
    const ms = await perfProbe.scrollTo(sc.scrollHeight - sc.clientHeight);
    return { ms, scrollHeight: sc.scrollHeight };
  },
  gridInfo(): { scrollHeight: number; clientHeight: number; scrollTop: number } {
    const sc = perfProbe.gridScroller();
    return { scrollHeight: sc.scrollHeight, clientHeight: sc.clientHeight, scrollTop: sc.scrollTop };
  },
  /** The mounted handle's kernel — probe-only bus access for phase timing. */
  kernel(): ViewerHandle["kernel"] {
    if (!handle) throw new Error("__perf.kernel: no viewer mounted");
    return handle.kernel;
  },
};
(window as unknown as { __perf: typeof perfProbe }).__perf = perfProbe;
