/**
 * Dev instrument (dev:true) — a deliberate developer surface, not a console dump.
 * A slim DEV BAR (build/status strip) expanding to a paneled drawer with tabs:
 *   LOG    — structured rows {t · level dot · channel · message}
 *   TIMING — phase bars from real performance marks (fetch→parse→layout→paint→fonts)
 *   INFO   — parserInfo / documentKind / pageCount / pageSize / sheets / dpr / zoom
 *   IMAGES — imageRefDiagnostics() table, decoded column lit live off imageReady
 *   BUS    — a live tap of EVERY kernel bus event (the biggest debug upgrade)
 *
 * Globals policy (leak-safety): attaches ONE namespaced window.__parasma +
 * keeps the verbatim window.__capture/__captureAll signatures (the screenshot
 * harness contract). destroy() deletes all.
 * Collapse state persisted under a snippet-scoped localStorage key.
 */
import type { Kernel } from "@parasma/headless";
import { parserInfo, createWorkerTransport, Viewer } from "@parasma/core";
import { el } from "../dom";
import { createInstalledLocallyProbe } from "../local-font-probe";

const LS_OPEN = "parasma.snippet.dev.open";
type Lvl = "ok" | "err" | "info";
type Tab = "LOG" | "TIMING" | "INFO" | "IMAGES" | "FONTS" | "BUS";

const ALL_EVENTS = [
  "loader:status-changed",
  "loader:loaded",
  "loader:error",
  "loader:load-started",
  "fonts:status-changed",
  "fonts:ready",
  "fonts:variant-arrived",
  "fonts:refreshed",
  "zoom:changed",
  "sheets:changed",
  "grid:mode-changed",
  "grid:resetup",
  "comments:changed",
  "pageReady",
  "loadComplete",
] as const;

export interface DevShellDeps {
  kernel: Kernel;
  viewer: Viewer;
  /**
   * Live dev-mode probe (mount's `data-dev`, runtime-toggleable via ⌥⇧D).
   * Event-driven work with a real cost (the FONTS pane execs `fonts.debug`,
   * which hits the wasm probe or a worker RPC per font arrival) is gated on
   * this so production mounts (`dev:false`) pay nothing. Absent → always dev
   * (test harnesses).
   */
  isDev?: () => boolean;
  /**
   * Coexistence: the localStorage key the drawer open/closed state
   * persists under. Injectable so a second co-mounted instance (or an
   * author-your-own chrome) can isolate its state; defaults to the shared
   * snippet-scoped key (standalone-identical). mount passes nothing.
   */
  openStateKey?: string;
}

export interface DevShellHandle {
  root: HTMLElement;
  /** Mark a fresh load (resets the timing baseline t0). */
  markLoadStart(): void;
  /** Record a timing phase landmark. */
  mark(phase: "parse" | "layout" | "first-paint" | "fonts-ready"): void;
  log(channel: string, message: string, level?: Lvl): void;
  /** Refresh the INFO + IMAGES tables (after a load settles). */
  refresh(): void;
  /** Attach the window globals (called when dev turns on). */
  attachGlobals(): void;
  /** Remove the window globals (dev off / destroy). */
  detachGlobals(): void;
  destroy(): void;
}

export function createDevShell(deps: DevShellDeps): DevShellHandle {
  const { kernel, viewer } = deps;
  const isDev = deps.isDev ?? (() => true);
  const openKey = deps.openStateKey ?? LS_OPEN;

  // ── DOM ────────────────────────────────────────────────────────────────────
  const buildEl = el("span", { class: "ps-dev-build" }, [el("b", {}, ["—"])]);
  const tabsWrap = el("div", { class: "ps-dev-tabs" });
  const tabButtons: Record<Tab, HTMLButtonElement> = {} as never;
  (["LOG", "TIMING", "INFO", "IMAGES", "FONTS", "BUS"] as Tab[]).forEach((t, i) => {
    const b = el("button", { class: "ps-dev-tab", "aria-selected": i === 0 ? "true" : "false" }, [t]);
    b.addEventListener("click", () => selectTab(t));
    tabButtons[t] = b;
    tabsWrap.append(b);
  });

  const disc = el("button", { class: "ps-dev-disc", "aria-label": "toggle dev drawer" }, ["⌄"]);
  const bar = el("div", { class: "ps-dev-bar" }, [
    el("span", { class: "ps-dev-title" }, ["DEV"]),
    tabsWrap,
    buildEl,
    el("div", { class: "ps-dev-toggle" }, [
      el("span", { class: "ps-dev-chord" }, ["⌥⇧D"]),
      disc,
    ]),
  ]);

  const logPane = el("div", { class: "ps-dev-pane", "data-tab": "LOG", "data-active": "1" });
  const timingPane = el("div", { class: "ps-dev-pane", "data-tab": "TIMING" });
  const infoPane = el("div", { class: "ps-dev-pane", "data-tab": "INFO" });
  const imagesPane = el("div", { class: "ps-dev-pane", "data-tab": "IMAGES" });
  const fontsPane = el("div", { class: "ps-dev-pane", "data-tab": "FONTS" });
  const busPane = el("div", { class: "ps-dev-pane", "data-tab": "BUS" });
  const body = el("div", { class: "ps-dev-body" }, [logPane, timingPane, infoPane, imagesPane, fontsPane, busPane]);

  const root = el("div", { class: "ps-dev" }, [bar, body]);

  // ── Drawer open/close ───────────────────────────────────────────────────────
  let open = safeOpen();
  applyOpen();
  disc.addEventListener("click", () => {
    open = !open;
    try {
      localStorage.setItem(openKey, open ? "1" : "0");
    } catch {
      /* ignore */
    }
    applyOpen();
  });
  function applyOpen(): void {
    root.setAttribute("data-open", open ? "1" : "0");
    disc.textContent = open ? "⌄" : "⌃";
  }

  let activeTab: Tab = "LOG";
  function selectTab(t: Tab): void {
    activeTab = t;
    for (const [k, b] of Object.entries(tabButtons)) {
      b.setAttribute("aria-selected", k === t ? "true" : "false");
    }
    for (const pane of body.querySelectorAll<HTMLElement>(".ps-dev-pane")) {
      // The CSS matches [data-active="1"] — set the VALUE, not a bare attribute.
      // toggleAttribute would set data-active="" which never matches, hiding
      // every pane after the first tab switch (the "Log never loads" bug).
      if (pane.dataset.tab === t) pane.setAttribute("data-active", "1");
      else pane.removeAttribute("data-active");
    }
    if (!open) {
      open = true;
      applyOpen();
    }
  }

  // ── Timing ──────────────────────────────────────────────────────────────────
  let t0 = performance.now();
  const marks = new Map<string, number>();
  function markLoadStart(): void {
    t0 = performance.now();
    marks.clear();
    renderTiming();
    // Reset the per-load LOG so timestamps stay coherent with the fresh t0
    // (TIMING/INFO/IMAGES already rebuild per load; BUS stays a running tap).
    logPane.innerHTML = "";
  }
  function mark(phase: "parse" | "layout" | "first-paint" | "fonts-ready"): void {
    if (!marks.has(phase)) {
      marks.set(phase, performance.now() - t0);
      renderTiming();
    }
  }
  function renderTiming(): void {
    timingPane.innerHTML = "";
    const phases: [string, string][] = [
      ["parse", "PARSE"],
      ["layout", "LAYOUT"],
      ["first-paint", "FIRST PAINT"],
      ["fonts-ready", "FONTS READY"],
    ];
    const max = Math.max(1, ...[...marks.values()]);
    for (const [key, label] of phases) {
      const ms = marks.get(key);
      const bar = el("div", { class: "ps-bar" });
      if (ms != null) bar.style.width = `${Math.max(2, (ms / max) * 100)}%`;
      else bar.style.opacity = "0.18";
      timingPane.append(
        el("div", { class: "ps-phase" }, [
          el("span", { class: "ps-pn" }, [label]),
          bar,
          el("span", { class: "ps-ms" }, [ms != null ? `${ms.toFixed(1)}ms` : "—"]),
        ]),
      );
    }
  }

  // ── Log ─────────────────────────────────────────────────────────────────────
  function row(t: number, channel: string, message: string, level: Lvl): HTMLElement {
    return el("div", { class: "ps-row", "data-lvl": level }, [
      el("span", { class: "ps-t" }, [`+${t.toFixed(0)}ms`]),
      el("span", { class: "ps-lvl" }),
      el("span", { class: "ps-chan" }, [channel]),
      el("span", { class: "ps-msg" }, [message]),
    ]);
  }
  function log(channel: string, message: string, level: Lvl = "info"): void {
    const t = performance.now() - t0;
    logPane.append(row(t, channel, message, level));
    autoScroll(logPane);
  }
  let logHover = false;
  logPane.addEventListener("mouseenter", () => (logHover = true));
  logPane.addEventListener("mouseleave", () => (logHover = false));
  function autoScroll(pane: HTMLElement): void {
    if (!logHover) body.scrollTop = body.scrollHeight;
  }

  // ── Bus tap ───────────────────────────────────────────────────────────────
  // `pageReady` is rAF-coalesced into one "×N (last idx)" row per frame: a
  // lazily-paginated 20k+-page spreadsheet streams tens of thousands of
  // pageReady events in under a second, and a DOM row per event (create +
  // cap-evict) costs more main-thread time than the engine layout itself.
  const busUnsubs: Array<() => void> = [];
  let prCount = 0;
  let prLast: unknown = null;
  let prRaf = false;
  const flushPageReady = (): void => {
    prRaf = false;
    if (prCount === 0) return;
    const t = performance.now() - t0;
    const label = prCount === 1 ? summarize(prLast) : `×${prCount} (last ${summarize(prLast)})`;
    prCount = 0;
    busPane.append(row(t, "pageReady", label, "info"));
    if (busPane.childElementCount > 400) busPane.firstElementChild?.remove();
    if (activeTab === "BUS") autoScroll(busPane);
  };
  for (const ev of ALL_EVENTS) {
    busUnsubs.push(
      kernel.bus.subscribe(ev as never, (payload: unknown) => {
        if (ev === "pageReady") {
          prCount++;
          prLast = payload;
          if (!prRaf) {
            prRaf = true;
            requestAnimationFrame(flushPageReady);
          }
          return;
        }
        const t = performance.now() - t0;
        const lvl: Lvl = ev === "loader:error" ? "err" : ev.endsWith(":ready") || ev === "loadComplete" ? "ok" : "info";
        busPane.append(row(t, ev, summarize(payload), lvl));
        if (busPane.childElementCount > 400) busPane.firstElementChild?.remove();
        if (activeTab === "BUS") autoScroll(busPane);
      }),
    );
  }
  function summarize(p: unknown): string {
    if (p == null) return "";
    if (typeof p === "number" || typeof p === "string") return String(p);
    try {
      const s = JSON.stringify(p);
      return s.length > 120 ? s.slice(0, 117) + "…" : s;
    } catch {
      return String(p);
    }
  }

  // ── INFO + IMAGES ───────────────────────────────────────────────────────────
  let copyBtn: HTMLButtonElement | null = null;
  async function refresh(): Promise<void> {
    // INFO
    let info = "—";
    try {
      info = await parserInfo();
    } catch {
      /* ignore */
    }
    // Viewer reads are async (the worker-ready seam). This
    // refresh() is already async, so await each one directly.
    const size = await (async () => {
      try {
        return await viewer.pageSize();
      } catch {
        return null;
      }
    })();
    let scale = 1;
    try {
      scale = (await kernel.exec("zoom.scale")) ?? 1;
    } catch {
      /* ignore */
    }
    const sheets = await viewer.sheetInfo();
    const pageCount = await viewer.pageCount();
    const comments = await viewer.comments();
    const kv: [string, string][] = [
      ["parserInfo", info],
      ["documentKind", lastKind],
      ["pageCount", String(pageCount)],
      ["pageSize", size ? `${Math.round(size.widthPx)}×${Math.round(size.heightPx)}px` : "—"],
      ["sheets", sheets.length ? sheets.map((s) => s.name).join(", ") : "—"],
      ["comments", String(comments.length)],
      ["devicePixelRatio", String(window.devicePixelRatio || 1)],
      ["zoom", `${Math.round(scale * 100)}%`],
    ];
    infoPane.innerHTML = "";
    const dl = el("dl", { class: "ps-kv" });
    for (const [k, v] of kv) {
      dl.append(el("dt", {}, [k]), el("dd", {}, [v]));
    }
    infoPane.append(dl);
    copyBtn = el("button", { class: "ps-btn ps-dev-copy" }, ["Copy debug bundle"]);
    copyBtn.addEventListener("click", () => copyBundle(kv));
    infoPane.append(copyBtn);

    // IMAGES
    const refs = await viewer.imageRefDiagnostics();
    imagesPane.innerHTML = "";
    if (refs.length === 0) {
      imagesPane.append(el("div", { class: "ps-row" }, [el("span", { class: "ps-msg" }, ["no embedded images"])]));
    } else {
      const tbl = el("table", { class: "ps-table" });
      const thead = el("thead", {}, [
        el("tr", {}, [
          el("th", {}, ["relId"]),
          el("th", {}, ["mime"]),
          el("th", {}, ["bytes"]),
          el("th", {}, ["decoded"]),
        ]),
      ]);
      const tbody = el("tbody");
      for (const r of refs) {
        const dec = decodedIds.has(r.relId);
        tbody.append(
          el("tr", { "data-rel": r.relId }, [
            el("td", {}, [r.relId]),
            el("td", {}, [r.mime]),
            el("td", {}, [String(r.byteLength)]),
            el("td", {}, [el("span", { class: dec ? "ps-yes" : "ps-no" }, [dec ? "yes" : "—"])]),
          ]),
        );
      }
      tbl.append(thead, tbody);
      imagesPane.append(tbl);
    }

    // FONTS — live pane, rendered separately (also re-runs on fonts:* events).
    await renderFonts();
  }
  let lastKind = "—";

  // ── FONTS (live) ────────────────────────────────────────────────────────────
  // One row per font the document USES, from the MERGED fonts.debug report
  // (headless plugin-fonts): TS-side provenance (plan resolution, byte origin +
  // freshness, registered variants) + the wasm probe's live FINAL-STATE
  // resolution (resolves-now / fell-back-to-default). Re-rendered on every
  // fonts:* bus event so the pane tracks face arrivals and refreshes
  // LIVE; rows whose (origin | resolvesNow | loadedVariants) changed since the
  // last render flash briefly (.ps-flash) so a toggle's effect is visible.
  type DbgProbe = {
    resolvesNow: string | null;
    fellBackToDefault: boolean | null;
    alias: string | null;
  };
  type DbgRow = {
    declared: string;
    resolved: string | null;
    origin: string | null;
    freshness: string | null;
    matchTier: number | string | null;
    matchedFamily: string | null;
    coverage: string;
    probe: DbgProbe | "unavailable";
    loadedVariants: string[];
    metrics: { unitsPerEm: number; ascentEm: number; descentEm: number; lineGapEm: number } | null;
  };
  type DbgReport = {
    useLocalFonts: boolean;
    refreshing: boolean;
    probeAvailable: boolean;
    permissionState: string;
    catalogReady: boolean;
    fonts: DbgRow[];
  };
  // Per-declared-family change signature from the previous render (flash keying).
  const lastFontSig = new Map<string, string>();
  // Subtle { changed } feedback from the last refreshFonts() (the live toggle).
  let lastRefreshNote = "";
  // Permissionless canvas-measure probe (shared with mount's one-click rule) —
  // the no-catalog fallback for the coverage column. See local-font-probe.ts
  // for the empirics (fonts.check refuted; Office-cache false-negatives).
  const installedLocally = createInstalledLocallyProbe();
  /** Normalize like the ladder: lowercase alphanumerics only. */
  const normFont = (n: string): string => n.toLowerCase().replace(/[^a-z0-9]/g, "");
  /** The exact/fallback judgment cell (tooltip keeps the registration name). */
  function renderAsCell(r: {
    declared: string;
    resolved: string | null;
    matchedFamily: string | null;
    origin: string | null;
    coverage: string;
  }): HTMLElement {
    let label: string;
    if (r.coverage === "missing" || r.resolved === null) {
      label = "engine default";
    } else {
      const painted = r.matchedFamily ?? r.resolved;
      label =
        normFont(painted) === normFont(r.declared)
          ? `exact (${r.origin ?? "?"})`
          : `fallback → ${painted}${r.origin === "local" ? " (local)" : ""}`;
    }
    const td = el("td", {}, [label]);
    td.title = `registered in the engine as "${r.resolved ?? "(default)"}"${r.matchedFamily ? `; local bytes = ${r.matchedFamily}` : ""}`;
    return td;
  }

  async function renderFonts(): Promise<void> {
    // Production mounts (dev off) skip the exec entirely — fonts.debug hits
    // the wasm probe / worker RPC, and this renderer is subscribed to every
    // font arrival. The pane re-renders on the next trigger after dev turns on.
    if (!isDev()) return;
    let report: DbgReport | null = null;
    try {
      report = (await kernel.exec("fonts.debug")) ?? null;
    } catch {
      /* plugin-fonts absent / kernel not booted */
    }
    fontsPane.innerHTML = "";
    if (!report) {
      lastFontSig.clear();
      fontsPane.append(
        el("div", { class: "ps-row" }, [el("span", { class: "ps-msg" }, ["fonts.debug unavailable"])]),
      );
      return;
    }
    // The live useLocalFonts toggle: setLocalFontsEnabled stays inert
    // by contract; viewer.refreshFonts() is the sole apply trigger. The pane
    // re-renders off fonts:refreshed (wired below), so the { changed } feedback
    // lands next to the toggle and stale rows flash via the signature diff.
    const toggle = el("input", { type: "checkbox", class: "ps-dev-localfonts" });
    toggle.checked = report.useLocalFonts;
    toggle.addEventListener("change", () => {
      const enabled = toggle.checked;
      viewer.setLocalFontsEnabled(enabled);
      lastRefreshNote = "refreshing…";
      log("FONT", `useLocalFonts → ${enabled}; refreshFonts()…`, "info");
      void viewer
        .refreshFonts()
        .then((r) => {
          lastRefreshNote = r.changed ? "applied (re-laid out)" : "no change";
          log("FONT", `refreshFonts: ${lastRefreshNote}`, r.changed ? "ok" : "info");
          // changed:false still needs a header repaint (the on/off flag moved);
          // changed:true re-renders via the fonts:refreshed bus listener anyway.
          void renderFonts();
        })
        .catch((e) => {
          lastRefreshNote = "failed";
          log("FONT", `refreshFonts failed: ${String((e as Error)?.message ?? e)}`, "err");
        });
    });
    fontsPane.append(
      el("dl", { class: "ps-kv" }, [
        el("dt", {}, ["useLocalFonts"]),
        el("dd", {}, [toggle, report.useLocalFonts ? " on" : " off", lastRefreshNote ? ` — ${lastRefreshNote}` : ""]),
        el("dt", {}, ["probe"]),
        el("dd", {}, [
          report.probeAvailable ? "available" : "OFF — rebuild wasm with --features debug-probe",
        ]),
        el("dt", {}, ["refreshing"]),
        el("dd", {}, [report.refreshing ? "yes" : "no"]),
        // The Font Access permission + session-catalog readiness.
        el("dt", {}, ["permission"]),
        el("dd", {}, [
          `${report.permissionState}${report.catalogReady ? " — catalog ready" : " — no catalog"}`,
        ]),
      ]),
    );
    const rows = report.fonts;
    if (rows.length === 0) {
      lastFontSig.clear();
      fontsPane.append(el("div", { class: "ps-row" }, [el("span", { class: "ps-msg" }, ["no fonts used"])]));
      return;
    }
    const VAR_ABBR: Record<string, string> = { regular: "R", bold: "B", italic: "I", boldItalic: "BI" };
    const tbl = el("table", { class: "ps-table" });
    const thead = el("thead", {}, [
      el("tr", {}, [
        el("th", {}, ["declared"]), el("th", {}, ["installed?"]), el("th", {}, ["coverage"]),
        el("th", {}, ["renders as"]), el("th", {}, ["origin"]), el("th", {}, ["resolves-now"]),
        el("th", {}, ["variants"]), el("th", {}, ["metrics"]),
      ]),
    ]);
    const tbody = el("tbody");
    const seen = new Set<string>();
    for (const r of rows) {
      const m = r.metrics;
      const probe = r.probe === "unavailable" ? null : r.probe;
      const resolvesNow = probe
        ? `${probe.resolvesNow ?? "—"}${probe.fellBackToDefault ? " ⚠ default" : ""}`
        : "—";
      const variants = r.loadedVariants.map((v) => VAR_ABBR[v] ?? v).join(" ") || "—";
      // The coverage classification (+ the ladder tier when local).
      // Without a catalog (no permission/enumeration yet) core can't see
      // installed candidates, so the CANVAS PROBE upgrades "substituted" AND
      // "missing" to the benefit case with a * (both count as improvable in
      // the CTA rule — the tab must agree with it; CSS-visible fonts only).
      const inst = installedLocally(r.declared);
      let coverage = r.coverage;
      if (
        !report.catalogReady &&
        (coverage === "substituted" || coverage === "missing") &&
        inst === true
      ) {
        coverage = `${coverage} — local available*`;
      }
      const coverageLabel = r.matchTier !== null ? `${coverage} (${typeof r.matchTier === "number" ? `tier ${r.matchTier}` : r.matchTier})` : coverage;
      // Change-flash: highlight the row when its live-facing facts moved.
      const sig = `${r.origin}|${resolvesNow}|${variants}|${coverageLabel}`;
      seen.add(r.declared);
      const changed = lastFontSig.has(r.declared) && lastFontSig.get(r.declared) !== sig;
      lastFontSig.set(r.declared, sig);
      tbody.append(
        el("tr", { class: changed ? "ps-flash" : "" }, [
          el("td", {}, [r.declared]),
          el("td", {}, [inst === null ? "—" : inst ? "yes" : "no*"]),
          el("td", {}, [coverageLabel]),
          // "renders as": the JUDGMENT the plumbing obscures — are the painted
          // bytes the DECLARED family (exact) or a stand-in (fallback → X)?
          // paintedFamily = the local matched family when local bytes serve,
          // else the plan's CDN face. The wasm REGISTRATION name (resolved —
          // local bytes register under it because RunStyle bakes it) is
          // engine plumbing, demoted to the cell tooltip.
          renderAsCell(r),
          el("td", {}, [`${r.origin ?? "—"}${r.freshness ? ` (${r.freshness})` : ""}`]),
          el("td", {}, [resolvesNow]),
          el("td", {}, [variants]),
          el("td", {}, [
            m
              ? `${m.unitsPerEm}upem ${m.ascentEm.toFixed(3)}/${m.descentEm.toFixed(3)}/${m.lineGapEm.toFixed(3)}`
              : "—",
          ]),
        ]),
      );
    }
    // Drop signatures for families no longer reported (doc switch) so their
    // reappearance later doesn't false-flash.
    for (const k of [...lastFontSig.keys()]) if (!seen.has(k)) lastFontSig.delete(k);
    tbl.append(thead, tbody);
    fontsPane.append(tbl);
    fontsPane.append(
      el("div", { class: "ps-row" }, [
        el("span", { class: "ps-msg" }, [
          "*installed? = CSS-visible system fonts (canvas probe); Office FontCache faces (e.g. Segoe UI on mac) read no here but ARE reachable via the dev local-font route. coverage = local-font coverage classification (catalog-fed when permission is granted; a trailing * marks the canvas-probe fallback).",
        ]),
      ]),
    );
  }
  // Live re-render triggers: per-face arrival, machine status edges, and
  // fonts:refreshed (the live re-apply after refreshFonts()).
  for (const ev of ["fonts:variant-arrived", "fonts:status-changed", "fonts:refreshed"] as const) {
    busUnsubs.push(kernel.bus.subscribe(ev as never, () => void renderFonts()));
  }

  // imageReady lights the decoded column live (joins the two streams). The
  // viewer fires imageReady on its own emitter (not the kernel bus), so we
  // subscribe directly on the viewer.
  const decodedIds = new Set<string>();
  const offImage = viewer.on("imageReady", (relId: string) => {
    decodedIds.add(relId);
    log("IMAGE", `decoded ${relId}`, "ok");
    const tr = imagesPane.querySelector<HTMLElement>(`tr[data-rel="${relId}"] td:last-child span`);
    if (tr) {
      tr.className = "ps-yes";
      tr.textContent = "yes";
    }
  });

  async function copyBundle(kv: [string, string][]): Promise<void> {
    const lines = [
      `UA: ${navigator.userAgent}`,
      `dpr: ${window.devicePixelRatio || 1}`,
      ...kv.map(([k, v]) => `${k}: ${v}`),
      "--- log tail ---",
      ...[...logPane.querySelectorAll<HTMLElement>(".ps-row")].slice(-30).map((r) => r.textContent ?? ""),
    ];
    try {
      await navigator.clipboard.writeText(lines.join("\n"));
      if (copyBtn) {
        const prev = copyBtn.textContent;
        copyBtn.textContent = "Copied ✓";
        setTimeout(() => copyBtn && (copyBtn.textContent = prev), 1200);
      }
    } catch {
      log("DEV", "clipboard unavailable", "err");
    }
  }

  // ── Build id ─────────────────────────────────────────────────────────────────
  void parserInfo()
    .then((s) => {
      buildEl.replaceChildren(el("b", {}, [s]));
    })
    .catch(() => {});

  function setLastKind(k: string): void {
    lastKind = k;
  }
  // Track kind off loader:loaded.
  const unsubKind = kernel.bus.subscribe("loader:loaded", (info: unknown) => {
    const k = (info as { documentKind?: string } | null)?.documentKind;
    if (k) setLastKind(k);
  });

  // ── Globals (leak-safety) ───────────────────────────────────────────────────
  type W = typeof window & {
    __parasma?: unknown;
    __capture?: (i: number) => Promise<string>;
    __captureAll?: () => Promise<string[]>;
  };
  const capture = async (idx: number): Promise<string> => {
    const { widthPx, heightPx } = await viewer.pageSize();
    const c = document.createElement("canvas");
    c.width = Math.round(widthPx);
    c.height = Math.round(heightPx);
    await viewer.renderPage(idx, c);
    return c.toDataURL("image/png");
  };
  const captureAll = async (): Promise<string[]> => {
    const out: string[] = [];
    const total = await viewer.pageCount();
    for (let i = 0; i < total; i++) out.push(await capture(i));
    return out;
  };
  function attachGlobals(): void {
    const w = window as W;
    w.__parasma = { kernel, viewer, capture, captureAll, createWorkerTransport, Viewer };
    w.__capture = capture;
    w.__captureAll = captureAll;
  }
  function detachGlobals(): void {
    const w = window as W;
    delete w.__parasma;
    delete w.__capture;
    delete w.__captureAll;
  }

  renderTiming();

  return {
    root,
    markLoadStart,
    mark,
    log,
    refresh: () => void refresh(),
    attachGlobals,
    detachGlobals,
    destroy() {
      for (const u of busUnsubs) u();
      offImage();
      unsubKind();
      detachGlobals();
    },
  };

  function safeOpen(): boolean {
    try {
      return localStorage.getItem(openKey) !== "0";
    } catch {
      return true;
    }
  }
}
