/**
 * Sheet-tab strip (XLSX). Mono tab names attached to the grid's bottom edge
 * (workspace fusion); the active tab carries the accent TOP edge. Driven off
 * the replayable sheets:changed bus event.
 *
 * NO VIEW-MODE TOGGLE: the continuous grid IS the spreadsheet in the default
 * chrome — pages are a docx/pptx concept, and the old Grid|Print segment let
 * users wander into a print-layout preview that read as pagination. The kernel
 * capability remains (`grid.setMode` — plugin-grid still owns the mode model),
 * so a headless / tier-C consumer can build a print-preview toggle if their
 * product wants one; the snippet deliberately does not.
 */
import type { Kernel } from "@parasma/headless";
import { el } from "../dom";

type SheetTab = { name: string; firstPage: number; pageCount: number };

/**
 * @advanced Tier-C seam (`@parasma/snippet/chrome`). Wired by mount.ts as:
 * `{ kernel, onSheetChange: (i) => core.emit("sheetChange", i) }`.
 */
export interface SheetTabsDeps {
  kernel: Kernel;
  /** Notify the handle so external `on('sheetChange')` fires. */
  onSheetChange(index: number): void;
}

export interface SheetTabsHandle {
  root: HTMLElement;
  destroy(): void;
}

export function createSheetTabs(deps: SheetTabsDeps): SheetTabsHandle {
  const { kernel } = deps;

  const tabsEl = el("div", { class: "ps-tabs", role: "tablist" });

  let renderedSheets: SheetTab[] | null = null;

  const unsubSheets = kernel.bus.subscribe("sheets:changed", ({ active, sheets }) => {
    if (sheets.length === 0) {
      tabsEl.removeAttribute("data-show");
      tabsEl.innerHTML = "";
      renderedSheets = null;
      return;
    }
    if (sheets !== renderedSheets) {
      renderedSheets = sheets;
      tabsEl.innerHTML = "";
      for (const [i, tab] of sheets.entries()) {
        const btn = el(
          "button",
          {
            class: "ps-tab",
            role: "tab",
            "aria-selected": "false",
            title: `${tab.name} — ${tab.pageCount} page(s)`,
          },
          [tab.name],
        );
        btn.addEventListener("click", () => {
          void kernel.exec("sheets.select", i);
          deps.onSheetChange(i);
        });
        tabsEl.append(btn);
      }
    }
    // Workspace fusion: a SINGLE-sheet workbook (the CSV norm) hides the tab
    // row entirely — a lone tab adds nothing. Evaluated on EVERY
    // sheets:changed, outside the rebuild guard, so a multi→single switch
    // re-hides even when the rebuild bails.
    if (sheets.length > 1) tabsEl.setAttribute("data-show", "1");
    else tabsEl.removeAttribute("data-show");
    const tabs = tabsEl.querySelectorAll<HTMLElement>(".ps-tab");
    tabs.forEach((t, i) => t.setAttribute("aria-selected", i === active ? "true" : "false"));
  });

  return {
    root: tabsEl,
    destroy() {
      unsubSheets();
    },
  };
}
