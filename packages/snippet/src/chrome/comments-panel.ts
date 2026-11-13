/**
 * Comments drawer (DOCX) — a right-side slide-in panel, present-only, keeping the
 * working card↔highlight wiring. Driven off the replayable comments:changed bus
 * event. Independent of dev mode. Per-author color from the comment.
 */
import type { Kernel } from "@parasma/headless";
import { el } from "../dom";

type CommentData = {
  id: number;
  author: string;
  date: string | null;
  initials: string | null;
  text: string;
  page: number;
  anchorY: number;
  color: string;
};

/**
 * @advanced Tier-C seam (`@parasma/snippet/chrome`). Wired by mount.ts as:
 * `{ kernel, scrollToAnchor: (page, y) => dom.scrollToAnchorPos(page, y) }`.
 */
export interface CommentsPanelDeps {
  kernel: Kernel;
  /** Scroll a clicked comment's anchor into view (snippet owns the scroll). */
  scrollToAnchor(page: number, y: number): void;
}

export interface CommentsPanelHandle {
  root: HTMLElement;
  open(): void;
  close(): void;
  toggle(): void;
  /** Open the panel + focus a card (the page-canvas highlight click path). */
  focusCard(id: number): void;
  readonly count: number;
  /** Notify when the count changes (toolbar badge / present-only). */
  onCountChange(cb: (n: number) => void): void;
  /**
   * Notify when the drawer's ACTUAL open state changes — the panel closes
   * itself (× button, doc switch, zero comments), so the toolbar button's
   * aria-pressed must follow the panel, not its own click count.
   */
  onOpenChange(cb: (open: boolean) => void): void;
  destroy(): void;
}

function fmtDate(iso: string | null): string {
  if (!iso) return "";
  const d = new Date(iso);
  return Number.isNaN(d.getTime()) ? iso : d.toLocaleDateString();
}

export function createCommentsPanel(deps: CommentsPanelDeps): CommentsPanelHandle {
  const { kernel } = deps;

  const countEl = el("span", { class: "ps-badge" }, ["0"]);
  const closeBtn = el("button", { class: "ps-btn ps-btn--ps-icon", "aria-label": "close comments" }, ["×"]);
  const head = el("div", { class: "ps-comments-head" }, [
    el("span", {}, ["Comments · ", countEl]),
    closeBtn,
  ]);
  const list = el("div", { class: "ps-comments-list" });
  const root = el("div", { class: "ps-comments", role: "complementary" }, [head, list]);

  let count = 0;
  let countCb: ((n: number) => void) | null = null;
  let openCb: ((open: boolean) => void) | null = null;

  closeBtn.addEventListener("click", () => close());

  function setOpen(on: boolean): void {
    const was = root.getAttribute("data-open") === "1";
    if (on) root.setAttribute("data-open", "1");
    else root.removeAttribute("data-open");
    if (was !== on) openCb?.(on);
  }
  function open(): void {
    setOpen(true);
  }
  function close(): void {
    setOpen(false);
  }
  function toggle(): void {
    setOpen(root.getAttribute("data-open") !== "1");
  }

  function render(comments: CommentData[]): void {
    list.innerHTML = "";
    count = comments.length;
    countEl.textContent = String(count);
    countCb?.(count);
    if (count === 0) {
      close();
      return;
    }
    for (const c of comments) {
      const card = el("div", { class: "ps-card", "data-id": String(c.id), style: `--ps-cc:${c.color}` }, [
        el("div", { class: "ps-card-head" }, [
          el("span", { class: "ps-card-author" }, [c.author || "Anonymous"]),
          el("span", { class: "ps-card-date" }, [fmtDate(c.date)]),
        ]),
        el("div", { class: "ps-card-body" }, [c.text]),
      ]);
      card.addEventListener("click", () => {
        void kernel.exec("comments.focus", c.id);
        focusCard(c.id);
        deps.scrollToAnchor(c.page, c.anchorY);
      });
      list.append(card);
    }
  }

  function focusCard(id: number): void {
    open();
    for (const card of list.querySelectorAll<HTMLElement>(".ps-card")) {
      const on = card.dataset.id === String(id);
      card.toggleAttribute("data-focused", on);
      if (on) card.scrollIntoView({ behavior: "smooth", block: "nearest" });
    }
  }

  const unsub = kernel.bus.subscribe("comments:changed", ({ comments, focused }) => {
    render(comments as CommentData[]);
    if (focused != null) focusCard(focused);
  });

  return {
    root,
    open,
    close,
    toggle,
    focusCard,
    get count() {
      return count;
    },
    onCountChange(cb) {
      countCb = cb;
    },
    onOpenChange(cb) {
      openCb = cb;
    },
    destroy() {
      unsub();
    },
  };
}
