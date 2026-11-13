/**
 * Search box (DOCX / XLSX) — the HEADED full-text-search UI over plugin-search
 * A floating find-bar pinned to the top-
 * right of the stage, opened with Ctrl/Cmd+F (standard browser/PDF/Word find UX).
 *
 * Holds ONLY the box DOM + its wiring to the plugin's kernel commands/events:
 *   - text input        → search.query (the plugin debounces; debounceMs is a
 *                         plugin option set in mount.ts, NOT here)
 *   - "n / m" counter   ← search:changed (activeIdx+1 / results.length)
 *   - prev / next + ↵/⇧↵ → search.prev / search.next (the plugin wraps)
 *   - Aa / "ab" toggles  → search.opts (the plugin re-runs the loop with them)
 *   - Sheet/Workbook seg → search.setScope (xlsx only; hidden for Word)
 *   - Esc / ×            → close + search.clear
 *
 * BOUNDARY: the plugin owns the model; this owns ONLY chrome. It does
 * NOT paint highlights — after a search:changed the CONSUMER (mount.ts) re-renders
 * the visible pages so the wasm highlight pass (already fed by the plugin via
 * viewer.setSearchHighlight) runs; this box just reflects the counter/state.
 *
 * The box keeps a LOCAL mirror of the toggle state (the source of truth is the
 * plugin store; the box reflects user intent) and pushes both flags through
 * `search.opts` whenever a toggle flips — the plugin adopts them and re-runs the
 * loop with the SAME text. Scope has its own command (`search.setScope`).
 */
import type { Kernel } from "@parasma/headless";
import { el } from "../dom";
import { icon } from "./icons";

/**
 * @advanced Tier-C seam (`@parasma/snippet/chrome`). Wired by mount.ts as:
 * `{ kernel }`.
 */
export interface SearchBoxDeps {
  kernel: Kernel;
  // No scroll/select/resolve deps: the search plugin and attach-dom own the
  // reveal, and the plugin owns the highlight repaint.
}

export interface SearchBoxHandle {
  root: HTMLElement;
  /** Open + focus the input (Ctrl/Cmd+F). No-op if already open. */
  open(): void;
  /** Close + clear the query/highlights (Esc / ×). */
  close(): void;
  /** Toggle open/closed. */
  toggle(): void;
  readonly isOpen: boolean;
  /**
   * Show/hide the Sheet/Workbook scope segment — visible only for spreadsheets.
   * Wired off the document kind in mount.ts (onLoaded).
   */
  setScopeVisible(visible: boolean): void;
  destroy(): void;
}

export function createSearchBox(deps: SearchBoxDeps): SearchBoxHandle {
  const { kernel } = deps;

  // ── DOM ──────────────────────────────────────────────────────────────────
  const input = el("input", {
    class: "ps-search-input",
    type: "text",
    placeholder: "Find in document",
    "aria-label": "Find in document",
    spellcheck: "false",
    autocomplete: "off",
  }) as HTMLInputElement;

  const counter = el("span", { class: "ps-search-count" }, [""]);

  const caseBtn = el("button", {
    class: "ps-btn ps-btn--ps-icon ps-search-toggle",
    "aria-pressed": "false",
    "aria-label": "Match case",
    title: "Match case",
  }, ["Aa"]) as HTMLButtonElement;
  const wordBtn = el("button", {
    class: "ps-btn ps-btn--ps-icon ps-search-toggle",
    "aria-pressed": "false",
    "aria-label": "Match whole word",
    title: "Match whole word",
  }, ["ab"]) as HTMLButtonElement;

  const prevBtn = el("button", {
    class: "ps-btn ps-btn--ps-icon",
    "aria-label": "Previous match",
    title: "Previous match (⇧↵)",
  }, [icon("chevron-left")]) as HTMLButtonElement;
  const nextBtn = el("button", {
    class: "ps-btn ps-btn--ps-icon",
    "aria-label": "Next match",
    title: "Next match (↵)",
  }, [icon("chevron-right")]) as HTMLButtonElement;

  const closeBtn = el("button", {
    class: "ps-btn ps-btn--ps-icon ps-search-close",
    "aria-label": "Close search",
    title: "Close (Esc)",
  }, ["×"]) as HTMLButtonElement;

  // Sheet/Workbook scope segment (xlsx only). Sheet is the default (Excel's
  // Within:Sheet); the active segment carries the accent press.
  const sheetSeg = el("button", { class: "ps-btn", "aria-pressed": "true" }, ["Sheet"]) as HTMLButtonElement;
  const bookSeg = el("button", { class: "ps-btn", "aria-pressed": "false" }, ["Book"]) as HTMLButtonElement;
  const scopeControl = el("div", { class: "ps-seg ps-search-scope" }, [sheetSeg, bookSeg]);
  scopeControl.style.display = "none";

  const toggles = el("div", { class: "ps-search-toggles" }, [caseBtn, wordBtn]);
  const nav = el("div", { class: "ps-search-nav" }, [prevBtn, nextBtn]);

  const root = el("div", { class: "ps-search", role: "search" }, [
    input,
    counter,
    toggles,
    scopeControl,
    nav,
    closeBtn,
  ]);

  // ── State (local mirror of the toggle/scope so re-queries carry them) ──────
  let open = false;
  let caseSensitive = false;
  let wholeWord = false;

  // Push the toggle state to the plugin; it adopts the opts and re-runs the loop
  // (with the same query text) under the new semantics.
  function applyOpts(): void {
    void kernel.exec("search.opts", { caseSensitive, wholeWord });
  }

  // ── Open / close ───────────────────────────────────────────────────────────
  function setOpen(on: boolean): void {
    open = on;
    root.toggleAttribute("data-open", on);
  }
  function doOpen(): void {
    setOpen(true);
    input.focus();
    input.select();
  }
  function doClose(): void {
    if (!open) return;
    setOpen(false);
    input.value = "";
    void kernel.exec("search.clear");
  }
  function toggle(): void {
    if (open) doClose();
    else doOpen();
  }

  // ── Input → query ───────────────────────────────────────────────────────────
  input.addEventListener("input", () => {
    void kernel.exec("search.query", input.value);
  });

  // Enter → next, Shift+Enter → prev, Esc → close. Keydown on the input so the
  // box owns its keys without a window-level capture fighting the host.
  input.addEventListener("keydown", (e) => {
    if (e.key === "Enter") {
      e.preventDefault();
      void kernel.exec(e.shiftKey ? "search.prev" : "search.next");
    } else if (e.key === "Escape") {
      e.preventDefault();
      doClose();
    }
  });

  prevBtn.addEventListener("click", () => {
    void kernel.exec("search.prev");
    input.focus();
  });
  nextBtn.addEventListener("click", () => {
    void kernel.exec("search.next");
    input.focus();
  });
  closeBtn.addEventListener("click", () => doClose());

  caseBtn.addEventListener("click", () => {
    caseSensitive = !caseSensitive;
    caseBtn.setAttribute("aria-pressed", caseSensitive ? "true" : "false");
    applyOpts();
    input.focus();
  });
  wordBtn.addEventListener("click", () => {
    wholeWord = !wholeWord;
    wordBtn.setAttribute("aria-pressed", wholeWord ? "true" : "false");
    applyOpts();
    input.focus();
  });

  sheetSeg.addEventListener("click", () => {
    sheetSeg.setAttribute("aria-pressed", "true");
    bookSeg.setAttribute("aria-pressed", "false");
    void kernel.exec("search.setScope", "sheet");
    input.focus();
  });
  bookSeg.addEventListener("click", () => {
    sheetSeg.setAttribute("aria-pressed", "false");
    bookSeg.setAttribute("aria-pressed", "true");
    void kernel.exec("search.setScope", "workbook");
    input.focus();
  });

  // ── search:changed → counter + repaint ──────────────────────────────────────
  // After every changed event the visible pages must re-render so the wasm
  // highlight pass (already fed by the plugin's setSearchHighlight) paints. The
  // plugin streams partial results, so this fires per batch — repaint is cheap
  // (the same per-frame invalidate-visible the font path uses).
  const unsubChanged = kernel.bus.subscribe("search:changed", (e) => {
    const total = e.results.length;
    const active = e.activeIdx;
    if (input.value === "") {
      counter.textContent = "";
      counter.removeAttribute("data-state");
    } else if (total === 0) {
      // Empty result while still searching reads as pending; "done" reads as 0.
      counter.textContent = e.status === "searching" ? "…" : "0/0";
      counter.setAttribute("data-state", e.status === "done" ? "empty" : "");
    } else {
      // Progressive count: while the streamed loop is still scanning, suffix "+"
      // to mean "more matches are still coming in" (e.g. 1/100+, 1/201+ …); drop it
      // the instant status flips to "done" for the final exact total. The plugin's
      // generation token already cancels a superseded query mid-stream, so a fast
      // typist never stacks scans.
      const n = active != null ? active + 1 : 0;
      const more = e.status === "searching" ? "+" : "";
      counter.textContent = `${n}/${total}${more}`;
      counter.removeAttribute("data-state");
    }
    // (Repaint after search:changed is the SEARCH PLUGIN's job now — it owns
    // pushHighlight + the invalidate; a second exec here would double-paint.)
  });

  // ── The reveal lives OUTSIDE the chrome ─────────────────────────────────────
  // Cross-sheet selection + settle + deferred-rect resolution live in the
  // search PLUGIN now (it emits `search:navigate-resolved`), and the default
  // scroll lives in attach-dom (`searchReveal: "auto"`). This box only drives
  // commands and renders state — the shape every BYO search bar gets too.

  return {
    root,
    open: doOpen,
    close: doClose,
    toggle,
    get isOpen() {
      return open;
    },
    setScopeVisible(visible) {
      scopeControl.style.display = visible ? "" : "none";
      if (!visible) {
        // Reset scope to the Sheet default when leaving a spreadsheet.
        sheetSeg.setAttribute("aria-pressed", "true");
        bookSeg.setAttribute("aria-pressed", "false");
      }
    },
    destroy() {
      unsubChanged();
    },
  };
}
