/**
 * Formula bar (XLSX) — name-box (cell ref) + value, the SINGLE selection source.
 *
 * WORKSPACE FUSION: the row's VISIBILITY is owned by the document KIND
 * (`setActive`, wired off mount's kind site) so it renders as a stable part of
 * the spreadsheet workspace — the old cell-click-reveals-the-strip model grew a
 * 34px band on first click and reflowed the whole grid (pop-in). `show()`/
 * `clear()` now only fill/empty the CONTENT (`data-empty`); the row itself
 * never appears/disappears while a spreadsheet is open. Cleared on sheet
 * switch / grid⇄pages toggle / doc switch by the snippet. Both the page-canvas
 * click and the grid adapter feed it through one `show()` path.
 */
import { el } from "../dom";

// Moved to ../types (the chrome-free module) so `@parasma/snippet/headless`
// consumers can import it; re-exported here so chrome imports keep working.
export type { FormulaCell } from "../types";
import type { FormulaCell } from "../types";

export interface FormulaBarHandle {
  root: HTMLElement;
  /** Row visibility — true for spreadsheet kinds (mount's kind site owns it). */
  setActive(on: boolean): void;
  show(cell: FormulaCell | null): void;
  clear(): void;
}

/**
 * @advanced Tier-C seam (`@parasma/snippet/chrome`). Zero deps — mount.ts calls
 * it bare, drives `setActive` off the loaded document's kind, and feeds
 * `show()` from its one cell-selection path.
 */
export function createFormulaBar(): FormulaBarHandle {
  const nameBox = el("span", { class: "ps-namebox" });
  const valEl = el("span", { class: "ps-fval" });
  const root = el("div", { class: "ps-formula" }, [
    nameBox,
    el("span", { class: "ps-fx" }, ["fx"]),
    valEl,
  ]);

  function show(cell: FormulaCell | null): void {
    if (!cell) {
      // Empty the CONTENT only — the row stays (no reflow mid-session).
      root.setAttribute("data-empty", "1");
      nameBox.textContent = "";
      valEl.textContent = "";
      return;
    }
    root.removeAttribute("data-empty");
    nameBox.textContent = cell.ref;
    valEl.textContent = "";
    valEl.append(document.createTextNode(cell.raw));
    if (cell.display !== cell.raw) {
      valEl.append(el("span", { class: "ps-disp" }, [`→ ${cell.display}`]));
    }
  }

  return {
    root,
    setActive(on) {
      if (on) {
        root.setAttribute("data-active", "1");
        // Entering a spreadsheet: present-but-empty until a cell is selected.
        if (!root.getAttribute("data-empty") && !nameBox.textContent) show(null);
      } else {
        root.removeAttribute("data-active");
      }
    },
    show,
    clear() {
      show(null);
    },
  };
}
