/**
 * Explorer panel — an OPT-IN, data-driven left sidebar listing openable
 * documents. APP-SHELL affordance, not viewer chrome (off by default; created
 * only when mountViewer receives an `explorer` config). The consumer supplies
 * the entries[] + an open() resolver; this builder owns the PANEL behaviour:
 * format-tier grouping (+ optional entry.group sub-tier), live substring filter,
 * collapsible groups, active-leaf highlight + reveal, whole-panel collapse.
 *
 * Styled to the snippet's precision-instrument aesthetic (Hanken Grotesk
 * labels, JetBrains Mono tabular badges, hairlines, one amber accent on the
 * active leaf). Deliberately minimal: no upload/recent sections, persistence,
 * icon rail or resize handle — the tree is format-grouped (always) + an
 * optional entry.group sub-tier.
 */
import { el } from "../dom";
import { icon } from "./icons";
import type { ExplorerConfig, ExplorerEntry, ExplorerFormat } from "../types";

// The tree disclosure chevron: points down when open, right when collapsed.
function setDisc(disc: HTMLElement, open: boolean): void {
  disc.replaceChildren(icon(open ? "chevron-down" : "chevron-right", 11));
}

const FMT_LABEL: Record<ExplorerFormat, string> = {
  docx: "DOCX",
  xlsx: "XLSX",
  pptx: "PPTX",
  csv: "CSV",
};

function fmtOf(name: string): ExplorerFormat {
  const p = name.toLowerCase();
  if (p.endsWith(".xlsx")) return "xlsx";
  if (p.endsWith(".pptx")) return "pptx";
  if (p.endsWith(".csv")) return "csv";
  return "docx";
}

function formatOf(entry: ExplorerEntry): ExplorerFormat {
  return entry.format ?? fmtOf(entry.name);
}

/**
 * @advanced Tier-C seam (`@parasma/snippet/chrome`). Wired by mount.ts as:
 * `{ config: options.explorer, onSelect: (id) => void openEntry(id) }`.
 */
export interface CreateExplorerDeps {
  config: ExplorerConfig;
  /** Leaf click → the host loads the entry (mount owns the load). */
  onSelect(id: string): void;
}

export interface ExplorerHandle {
  root: HTMLElement;
  /** Highlight + reveal the active leaf (called after a successful load). */
  markActive(id: string): void;
  /** Replace the entry set (e.g. after an upload) + rebuild the tree. */
  setEntries(entries: ExplorerEntry[]): void;
  /** Collapse/expand the whole panel (toggles when `open` omitted). */
  toggle(open?: boolean): void;
  isOpen(): boolean;
  /** A toolbar-side re-open affordance can mirror collapsed state via this. */
  onCollapsedChange(cb: (collapsed: boolean) => void): void;
  destroy(): void;
}

// Open/close a tree group without rebuilding it (preserves the rest of the tree).
function setGroupOpen(group: HTMLElement, open: boolean): void {
  const children = group.querySelector<HTMLElement>(":scope > .ps-tree-children");
  const disc = group.querySelector<HTMLElement>(":scope > .ps-tree-row > .ps-disc");
  if (children) children.hidden = !open;
  if (disc) setDisc(disc, open);
  group.dataset.open = open ? "1" : "0";
}

function makeRow(opts: {
  cls: string;
  label: string;
  badge?: number;
  fmt?: ExplorerFormat;
}): HTMLDivElement {
  const row = el("div", { class: `ps-tree-row ${opts.cls}` });
  const disc = el("span", { class: "ps-disc" }, [icon("chevron-down", 11)]);
  row.appendChild(disc);
  if (opts.fmt) {
    row.appendChild(el("span", { class: "ps-fmt-dot", "data-fmt": opts.fmt }));
  }
  row.appendChild(el("span", { class: "ps-tree-label" }, [opts.label]));
  if (opts.badge !== undefined) {
    // data-total: the unfiltered count, restored when the filter query clears
    // (applyFilter rewrites the badge to the visible-leaf count while filtering).
    row.appendChild(
      el("span", { class: "ps-tree-badge", "data-total": String(opts.badge) }, [
        String(opts.badge),
      ]),
    );
  }
  return row;
}

// A collapsible group: a clickable header row toggling a children box.
function makeAccordion(
  row: HTMLDivElement,
  openByDefault: boolean,
): { group: HTMLDivElement; children: HTMLDivElement } {
  const children = el("div", { class: "ps-tree-children" });
  children.hidden = !openByDefault;
  const group = el("div", { class: "ps-tree-group" }, [row, children]);
  group.dataset.open = openByDefault ? "1" : "0";
  const disc = row.querySelector<HTMLElement>(".ps-disc");
  if (disc) setDisc(disc, openByDefault);
  row.addEventListener("click", () => {
    const open = Boolean(children.hidden); // `hidden` may also be "until-found"
    children.hidden = !open;
    if (disc) setDisc(disc, open);
    group.dataset.open = open ? "1" : "0";
  });
  return { group, children };
}

export function createExplorer(deps: CreateExplorerDeps): ExplorerHandle {
  const { config } = deps;
  const showFilter = config.filter !== false;
  const collapsible = config.collapsible !== false;

  let entries: ExplorerEntry[] = config.entries.slice();
  let selectedId: string | null = config.selectedId ?? null;
  let collapsedCb: ((collapsed: boolean) => void) | null = null;

  function makeLeaf(entry: ExplorerEntry): HTMLElement {
    const leaf = el("div", { class: "ps-tree-leaf", role: "button", tabindex: "0" }, [
      el("span", { class: "ps-leaf-label" }, [entry.name]),
    ]);
    leaf.dataset.id = entry.id;
    leaf.dataset.haystack = `${entry.name} ${entry.group ?? ""}`.toLowerCase();
    if (entry.id === selectedId) leaf.classList.add("is-selected");
    const fire = () => deps.onSelect(entry.id);
    leaf.addEventListener("click", fire);
    leaf.addEventListener("keydown", (e) => {
      if (e.key === "Enter" || e.key === " ") {
        e.preventDefault();
        fire();
      }
    });
    return leaf;
  }

  // ── Head ────────────────────────────────────────────────────────────────
  const titleEl = el("span", { class: "ps-explorer-title" }, [config.title ?? "Documents"]);
  const collapseBtn = el(
    "button",
    { class: "ps-btn ps-btn--ps-icon ps-explorer-collapse", "aria-label": "collapse panel", title: "Collapse (⌘B)" },
    [icon("panel-left-close")],
  );
  const head = el("div", { class: "ps-explorer-head" }, [titleEl]);
  if (collapsible) head.appendChild(collapseBtn);

  // ── Filter ──────────────────────────────────────────────────────────────
  const searchInput = el("input", {
    class: "ps-explorer-search",
    type: "search",
    placeholder: "Filter…",
    "aria-label": "filter documents",
    spellcheck: "false",
  }) as HTMLInputElement;
  const filterWrap = el("div", { class: "ps-explorer-filter" }, [searchInput]);

  // ── Tree ────────────────────────────────────────────────────────────────
  const tree = el("div", { class: "ps-explorer-tree", role: "tree" });
  const emptyState = el("div", { class: "ps-explorer-empty" }, [
    "No documents — supply entries.",
  ]);
  // Zero-match state: shown when a filter query hides every leaf (a blank void
  // otherwise reads as "the documents are gone").
  const noMatchState = el("div", { class: "ps-explorer-empty ps-explorer-nomatch" });
  noMatchState.style.display = "none";

  const body = el("div", { class: "ps-explorer-body" }, [tree, noMatchState, emptyState]);
  const inner = el("div", { class: "ps-explorer-inner" }, [head]);
  if (showFilter) inner.appendChild(filterWrap);
  inner.appendChild(body);
  const root = el("div", { class: "ps-explorer", role: "complementary" }, [inner]);

  // ── Reveal (collapse prior active group, open new, highlight + scroll) ─────
  let activeGroup: HTMLElement | null = null;
  function revealActivePath(id: string): void {
    let leafEl: HTMLElement | null = null;
    for (const l of tree.querySelectorAll<HTMLElement>(".ps-tree-leaf")) {
      if (l.dataset.id === id) {
        leafEl = l;
        break;
      }
    }
    if (!leafEl) return;
    // The innermost group containing the leaf (entry.group sub-tier, else format).
    const innerGroup = leafEl.closest<HTMLElement>(".ps-tree-group");
    if (innerGroup && innerGroup !== activeGroup) {
      if (activeGroup) setGroupOpen(activeGroup, false);
      activeGroup = innerGroup;
    }
    // Open the whole ancestor chain so the leaf is visible.
    let g: HTMLElement | null = innerGroup;
    while (g) {
      setGroupOpen(g, true);
      g = g.parentElement?.closest<HTMLElement>(".ps-tree-group") ?? null;
    }
    leafEl.scrollIntoView({ behavior: "smooth", block: "nearest" });
  }

  function markActive(id: string): void {
    selectedId = id;
    for (const l of tree.querySelectorAll<HTMLElement>(".ps-tree-leaf")) {
      l.classList.toggle("is-selected", l.dataset.id === id);
    }
    revealActivePath(id);
  }

  // ── Filter: substring hide + collapse-empty + force-open-on-match ──────────
  function applyFilter(q: string): void {
    const query = q.trim().toLowerCase();
    let visibleTotal = 0;
    for (const leaf of tree.querySelectorAll<HTMLElement>(".ps-tree-leaf")) {
      const hay = leaf.dataset.haystack ?? "";
      const show = !query || hay.includes(query);
      leaf.style.display = show ? "" : "none";
      if (show) visibleTotal++;
    }
    for (const group of tree.querySelectorAll<HTMLElement>(".ps-tree-group")) {
      const leaves = group.querySelectorAll<HTMLElement>(".ps-tree-leaf");
      const visible = [...leaves].filter((l) => l.style.display !== "none").length;
      group.style.display = visible > 0 || leaves.length === 0 ? "" : "none";
      // Badge shows the MATCH count while filtering, the total otherwise
      // (a "12" badge over 2 visible leaves read as a broken filter).
      const badge = group.querySelector<HTMLElement>(
        ":scope > .ps-tree-row > .ps-tree-badge",
      );
      if (badge) badge.textContent = query ? String(visible) : badge.dataset.total ?? "";
      if (query && visible > 0) {
        const children = group.querySelector<HTMLElement>(":scope > .ps-tree-children");
        const disc = group.querySelector<HTMLElement>(":scope > .ps-tree-row > .ps-disc");
        if (children) children.hidden = false;
        if (disc) setDisc(disc, true);
        group.dataset.open = "1";
      }
    }
    // Zero-match: surface the query instead of a blank void.
    const noMatch = Boolean(query) && visibleTotal === 0 && entries.length > 0;
    noMatchState.textContent = noMatch ? `No matches for “${q.trim()}”` : "";
    noMatchState.style.display = noMatch ? "block" : "none";
  }

  // ── buildTree: FORMAT tier (always) + optional entry.group sub-tier ────────
  function buildTree(): void {
    tree.innerHTML = "";
    activeGroup = null;
    // "block", not "" — the class's CSS default is display:none, so "" (remove
    // the inline override) would keep the zero-entry message invisible.
    emptyState.style.display = entries.length === 0 ? "block" : "none";

    const fmts: ExplorerFormat[] = ["docx", "xlsx", "pptx", "csv"];
    for (const fmt of fmts) {
      const fmtEntries = entries.filter((e) => formatOf(e) === fmt);
      if (fmtEntries.length === 0) continue;

      const fmtRow = makeRow({
        cls: "is-format",
        label: FMT_LABEL[fmt],
        badge: fmtEntries.length,
        fmt,
      });
      const { group, children } = makeAccordion(fmtRow, true);

      // Does this format have any explicit sub-groups?
      const hasGroups = fmtEntries.some((e) => e.group);
      if (hasGroups) {
        const groups = new Map<string, ExplorerEntry[]>();
        for (const e of fmtEntries) {
          const key = e.group ?? "Other";
          const arr = groups.get(key) ?? [];
          arr.push(e);
          groups.set(key, arr);
        }
        const sorted = [...groups.entries()].sort((a, b) => b[1].length - a[1].length);
        let first = true;
        for (const [name, list] of sorted) {
          const subRow = makeRow({ cls: "is-group", label: name, badge: list.length });
          const { group: subGroup, children: subChildren } = makeAccordion(
            subRow,
            first || sorted.length <= 3,
          );
          first = false;
          for (const e of [...list].sort((a, b) => a.name.localeCompare(b.name))) {
            subChildren.appendChild(makeLeaf(e));
          }
          children.appendChild(subGroup);
        }
      } else {
        for (const e of [...fmtEntries].sort((a, b) => a.name.localeCompare(b.name))) {
          children.appendChild(makeLeaf(e));
        }
      }
      tree.appendChild(group);
    }

    if (showFilter) applyFilter(searchInput.value);
    if (selectedId) {
      for (const l of tree.querySelectorAll<HTMLElement>(".ps-tree-leaf")) {
        l.classList.toggle("is-selected", l.dataset.id === selectedId);
      }
      revealActivePath(selectedId);
    }
  }

  // ── Collapse ──────────────────────────────────────────────────────────────
  let open = true;
  function setCollapsed(collapsed: boolean): void {
    open = !collapsed;
    root.toggleAttribute("data-collapsed", collapsed);
    collapsedCb?.(collapsed);
  }
  function toggle(force?: boolean): void {
    setCollapsed(force === undefined ? open : !force);
  }

  // ── Events ──────────────────────────────────────────────────────────────
  if (showFilter) {
    searchInput.addEventListener("input", () => applyFilter(searchInput.value));
  }
  if (collapsible) {
    collapseBtn.addEventListener("click", () => setCollapsed(open));
  }

  buildTree();

  return {
    root,
    markActive,
    setEntries(next) {
      entries = next.slice();
      buildTree();
    },
    toggle,
    isOpen: () => open,
    onCollapsedChange(cb) {
      collapsedCb = cb;
    },
    destroy() {
      root.remove();
    },
  };
}
