/**
 * DomClipboardAdapter — the ONLY DOM/clipboard-touching piece of the copy
 * feature. The plugin owns the model/decision; this
 * adapter owns the browser triggers and the actual `navigator.clipboard` write,
 * always inside the user-gesture window. It NEVER emits bus
 * events itself — after the write actually LANDS (or fails) it calls the plugin's
 * `clipboard.reportCopied` / `clipboard.reportError` commands, which emit; so the
 * "Copied N chars" flash reflects the real OS outcome on ALL tiers.
 *
 * Triggers: a hidden contenteditable "copy sink" (so a canvas viewer still fires
 * the `copy` event — Safari) + a window `keydown` Ctrl/⌘+C backstop, deduped
 * within ~150ms (the stamp clears after each write so a repeat ⌘C on the
 * SAME selection still copies). Write tiers: on the `copy` event the
 * synchronous `setData` tier runs FIRST inside the event tick from the fresh sync
 * cache (text/plain + TSV are both text), reports success, then best-
 * effort attempts the richer ClipboardItem write; otherwise (keydown path, or a
 * stale/worker cache) the async ClipboardItem tiers run — (1) promise-valued item
 * (keeps transient activation across the async/worker round-trip; Safari), (2)
 * await-blob-then-write for older Chromium. A missing text/plain never writes an
 * empty blob; stale + worker mode reports an error rather than writing
 * wrong content.
 */
import type { Kernel } from "@parasma/headless";
import {
  serializeTextPlain,
  serializeTextHtml,
  serializeCellsTsv,
  serializeCellsHtml,
  type CachedCopy,
  type CopyOutcome,
} from "@parasma/headless";
import type { DocSelection, ObjectRef, TextPos, Viewer } from "@parasma/core";
import type { RootScope } from "../root-scope";

export interface DomClipboardAdapterOptions {
  kernel: Kernel;
  /** The DOM seam (realm window/document, shadow-correct activeElement/selection). */
  scope: RootScope;
  /** The SAME Viewer the plugin drives (for the tier-3 in-process sync extract). */
  viewer: Viewer;
  /** The mount root the hidden copy sink is appended to. */
  root: HTMLElement;
  /**
   * The host's "focus is in interactive chrome" test (find box, inputs). The
   * adapter must NOT steal a ⌘C aimed at the find box. Excludes the copy sink
   * (see DomToolAdapter.isInteractiveFocus / the `data-clipboard-sink` guard).
   */
  isInteractiveFocus: () => boolean;
}

const DEDUP_MS = 150;
const PRIME_MS = 150;

/** Lexicographic (para, byte) order. */
function order(a: TextPos, b: TextPos): [TextPos, TextPos] {
  if (a.para < b.para || (a.para === b.para && a.byte <= b.byte)) return [a, b];
  return [b, a];
}

// Takes a KNOWN-present flavor value (never undefined): the caller must
// have confirmed text/plain, so we never fabricate an empty blob that would
// silently clobber the user's clipboard.
function asTextBlob(v: string | Blob): Blob {
  return typeof v === "string" ? new Blob([v], { type: "text/plain" }) : v;
}

// The text/html sibling. The value is always the serialized
// HTML string (or an already-built Blob); never fabricated, so an absent flavor
// is skipped, not written empty.
function asHtmlBlob(v: string | Blob): Blob {
  return typeof v === "string" ? new Blob([v], { type: "text/html" }) : v;
}

export class DomClipboardAdapter {
  private readonly kernel: Kernel;
  private readonly scope: RootScope;
  private readonly viewer: Viewer;
  private readonly isInteractiveFocus: () => boolean;
  private readonly sink: HTMLDivElement;

  // Selection state tracked synchronously off the bus (the selection seq is the guard).
  private lastSeq = 0;
  private canCopy = false;
  private localCache: CachedCopy | null = null;
  // The ONE tracked text-ish selection (a DocSelection: WML text, SML
  // cells range, or PML text span), the arg of the unified tier-3 sync extract
  // `viewer.extractRangeSyncInProcess`. Its `kind` picks the serializer pair
  // (cells → TSV + HTML <table>; text/pmlText → the DocFragment text flavors).
  private syncSel: DocSelection | null = null;
  // A selected IMAGE object. Mutually exclusive with
  // `syncSel`. image/png cannot ride the sync `clipboardData.setData` tier
  // (setData carries no Blob and there is no sync image escape hatch), so an
  // object copy uses the async ClipboardItem tier ONLY (transient activation
  // survives the round-trip).
  private objectSel: { ref: ObjectRef } | null = null;

  private primeTimer = 0;
  private lastStamp: { at: number; seq: number } | null = null;
  // The collapsed→non-collapsed edge tracker: the sink is focused + its
  // Range rebuilt ONCE on the edge (or on focus loss), not on every drag frame.
  private wasNonCollapsed = false;

  private readonly unsubs: Array<() => void> = [];
  private readonly onSinkCopy: (e: ClipboardEvent) => void;
  private readonly onSinkBeforeInput: (e: Event) => void;
  private readonly onKeyDown: (e: KeyboardEvent) => void;

  constructor(opts: DomClipboardAdapterOptions) {
    this.kernel = opts.kernel;
    this.scope = opts.scope;
    this.viewer = opts.viewer;
    this.isInteractiveFocus = opts.isInteractiveFocus;

    // Hidden copy sink: a 1px contenteditable that captures the `copy` event on a
    // canvas viewer and provides a graceful degradation payload. Read-only.
    this.sink = this.scope.doc.createElement("div");
    this.sink.setAttribute("contenteditable", "true");
    this.sink.setAttribute("tabindex", "-1");
    // NO aria-hidden: `aria-hidden` on a FOCUSABLE node is a
    // WCAG violation (hidden from AT yet in the focus order). The sink is a 1px
    // opacity:0 offscreen copy buffer, blurred back to the document right after
    // each copy (handleSinkCopy), so it never traps AT focus; the aria-label
    // gives a stray landing a meaningful name instead of an empty editable.
    this.sink.setAttribute("aria-label", "copy buffer");
    this.sink.setAttribute("data-clipboard-sink", "");
    Object.assign(this.sink.style, {
      position: "fixed",
      top: "0",
      left: "0",
      width: "1px",
      height: "1px",
      opacity: "0",
      overflow: "hidden",
      pointerEvents: "none",
      whiteSpace: "pre",
    } as Partial<CSSStyleDeclaration>);
    opts.root.appendChild(this.sink);

    this.onSinkBeforeInput = (e) => e.preventDefault(); // read-only, still copyable
    this.onSinkCopy = (e) => this.handleSinkCopy(e);
    this.onKeyDown = (e) => this.handleKeyDown(e);

    this.sink.addEventListener("beforeinput", this.onSinkBeforeInput);
    this.sink.addEventListener("copy", this.onSinkCopy);
    this.scope.win.addEventListener("keydown", this.onKeyDown);

    this.unsubs.push(
      this.kernel.bus.subscribe("selection:changed", (e) => this.onSelectionChanged(e)),
    );
  }

  private onSelectionChanged(e: {
    kind: "none" | "text" | "object" | "cells" | "pmlText";
    collapsed: boolean;
    anchor?: { para: number; byte: number };
    focus?: { para: number; byte: number };
    object?: ObjectRef;
    sheet?: number;
    range?: { r0: number; c0: number; r1: number; c1: number };
    pml?: {
      slide: number;
      targetKind: number;
      targetA: number;
      targetB: number;
      anchor: { para: number; byte: number };
      focus: { para: number; byte: number };
    };
    seq: number;
  }): void {
    this.lastSeq = e.seq;
    if (e.kind === "object" && e.object) {
      // Object selection: no text range/cache (no image priming). Focus the
      // sink so ⌘C / the copy event targets it; the image rides the async tier.
      this.objectSel = { ref: e.object };
      this.syncSel = null;
      this.canCopy = false;
      this.localCache = null;
      this.wasNonCollapsed = false;
      if (!this.isInteractiveFocus()) this.armSink(" ");
      return;
    }
    this.objectSel = null;
    if (e.kind === "pmlText" && !e.collapsed && e.pml) {
      // A PML text span (one shape/cell). Same text tiers as
      // WML text (plain + html), serialized from the shared DocFragment.
      const p = e.pml;
      const [start, end] = order(p.anchor, p.focus);
      this.syncSel = {
        kind: "pmlText",
        slide: p.slide,
        target: { targetKind: p.targetKind, targetA: p.targetA, targetB: p.targetB },
        start,
        end,
      };
      this.canCopy = true;
      const focusLost = this.scope.activeElement() !== this.sink;
      if (!this.isInteractiveFocus() && (!this.wasNonCollapsed || focusLost)) {
        this.armSink(" ");
      }
      this.wasNonCollapsed = true;
      this.schedulePrime();
      return;
    }
    if (e.kind === "cells" && e.sheet != null && e.range) {
      // An SML cells range. Even a collapsed single cell is
      // copyable (Excel copies the active cell). Prime the TSV/HTML text cache.
      this.syncSel = { kind: "cells", sheet: e.sheet, range: e.range };
      this.canCopy = true;
      const focusLost = this.scope.activeElement() !== this.sink;
      if (!this.isInteractiveFocus() && (!this.wasNonCollapsed || focusLost)) {
        this.armSink(" ");
      }
      this.wasNonCollapsed = true;
      this.schedulePrime();
      return;
    }
    if (e.kind === "text" && !e.collapsed && e.anchor && e.focus) {
      const [start, end] = order(e.anchor, e.focus);
      this.syncSel = { kind: "text", start, end };
      this.canCopy = true;
      // Focus the sink so ⌘C / the copy event targets it — but don't steal focus
      // from interactive chrome (find box). Arm with a placeholder; the debounced
      // prime fills in the real preview + the sync fallback cache.
      // Focus + (re)build the sink Range ONCE on the
      // collapsed->non-collapsed edge, or if focus was lost (e.g. blurred
      // after a prior copy). During a steady drag the sink stays focused and
      // only the debounced prime updates its text node IN PLACE, so a drag no
      // longer re-focuses + rebuilds a Range ~60x/s.
      const focusLost = this.scope.activeElement() !== this.sink;
      if (!this.isInteractiveFocus() && (!this.wasNonCollapsed || focusLost)) {
        this.armSink(" ");
      }
      this.wasNonCollapsed = true;
      this.schedulePrime();
    } else {
      this.syncSel = null;
      this.canCopy = false;
      this.localCache = null;
      this.wasNonCollapsed = false;
      if (this.scope.activeElement() === this.sink) this.sink.blur();
    }
  }

  /** Put `text` in the sink, select it, and focus (so a copy targets it). */
  private armSink(text: string): void {
    this.sink.textContent = text || " ";
    const sel = this.scope.getSelection();
    if (sel) {
      sel.removeAllRanges();
      const r = this.scope.createRange();
      r.selectNodeContents(this.sink);
      sel.addRange(r);
    }
    this.sink.focus({ preventScroll: true });
  }

  /**
   * Update the sink's text WITHOUT re-focus / Range rebuild. Mutating
   * the existing text node's data in place keeps the selectNodeContents Range
   * valid (it spans the sink's contents, not a specific node reference), so a
   * copy still targets the fresh text. Falls back to a full arm only if the
   * node was lost; the caller already gates on sink focus.
   */
  private updateSinkText(text: string): void {
    const node = this.sink.firstChild;
    if (node && node.nodeType === Node.TEXT_NODE) {
      node.nodeValue = text || " ";
    } else {
      this.armSink(text);
    }
  }

  /** Debounced pre-serialization for the tier-3 sync fallback + sink preview. */
  private schedulePrime(): void {
    if (this.primeTimer) this.scope.win.clearTimeout(this.primeTimer);
    this.primeTimer = this.scope.win.setTimeout(() => {
      this.primeTimer = 0;
      if (!this.canCopy) return;
      void this.kernel.exec("clipboard.serialize").then((c) => {
        if (c && c.seq === this.lastSeq) {
          this.localCache = c;
          // Refresh the sink text in place (no re-focus / Range
          // rebuild) only while it is already the focus target.
          if (this.scope.activeElement() === this.sink) this.updateSinkText(c.plain);
        }
      });
    }, PRIME_MS);
  }

  private stampGesture(seq: number): void {
    this.lastStamp = { at: performance.now(), seq };
  }

  private dedupHit(seq: number): boolean {
    return (
      this.lastStamp !== null &&
      performance.now() - this.lastStamp.at < DEDUP_MS &&
      this.lastStamp.seq === seq
    );
  }

  /** PRIMARY trigger: the copy event on the focused sink. */
  private handleSinkCopy(e: ClipboardEvent): void {
    // An object selection copies the IMAGE (async tier only).
    if (this.objectSel) {
      if (this.dedupHit(this.lastSeq)) {
        e.preventDefault();
        return;
      }
      this.stampGesture(this.lastSeq);
      e.preventDefault();
      void this.writeImageCopy();
      queueMicrotask(() => {
        if (this.scope.activeElement() === this.sink) this.sink.blur();
      });
      return;
    }
    if (!this.canCopy) return;
    if (this.dedupHit(this.lastSeq)) {
      // The keydown backstop already wrote this gesture — swallow the sink's
      // default so it doesn't overwrite with the placeholder text.
      e.preventDefault();
      return;
    }
    this.stampGesture(this.lastSeq);
    e.preventDefault();
    void this.writeThreeTier(e);
    // Release the hidden sink after the copy so arrows / PageUp-Down
    // target the viewer scroller again (the 1px offscreen sink would otherwise
    // swallow keyboard navigation). Deferred to a microtask so it cannot disturb
    // the in-flight copy event's own processing; the sync setData tier inside
    // writeThreeTier already ran (before its first await).
    queueMicrotask(() => {
      if (this.scope.activeElement() === this.sink) this.sink.blur();
    });
  }

  /**
   * Perform an OS copy of the CURRENT selection from an
   * explicit host gesture (the context-menu Copy item's click). Mirrors the ⌘C
   * backstop path: the async ClipboardItem tiers only (no `clipboardData`), which
   * runs `clipboard.copy` and writes to `navigator.clipboard` inside the click's
   * user-activation window. No-op when nothing is selected. Public so the host's
   * DomContextMenuAdapter wires Copy → this without duplicating the write tiers.
   */
  copyNow(): void {
    if (this.objectSel) {
      this.stampGesture(this.lastSeq);
      void this.writeImageCopy();
      return;
    }
    if (!this.canCopy) return;
    this.stampGesture(this.lastSeq);
    void this.writeThreeTier(undefined);
  }

  /** BACKSTOP trigger: window Ctrl/⌘+C. Never preventDefault the keydown. */
  private handleKeyDown(e: KeyboardEvent): void {
    if (!(e.metaKey || e.ctrlKey) || e.altKey || (e.key !== "c" && e.key !== "C")) return;
    if (this.isInteractiveFocus()) return; // ⌘C in the find box copies find text
    if (this.scope.activeElement() === this.sink) return; // the copy event handles it
    if (this.objectSel) {
      this.stampGesture(this.lastSeq);
      void this.writeImageCopy(); // async ClipboardItem tier only
      return;
    }
    if (!this.canCopy) return;
    this.stampGesture(this.lastSeq);
    void this.writeThreeTier(undefined); // tiers 1/2 only (no clipboardData here)
  }

  /**
   * Object image copy. Resolves the outcome FIRST, then builds
   * a ClipboardItem from the PRESENT flavors only (never feed a possibly-
   * rejecting png promise into ClipboardItem, which would sink html in Chromium).
   * image/png is absent in worker mode → html-only write, NOT a failure.
   * A CHART object copy additionally carries text/plain (the
   * data-TSV): pasting into a text editor yields the series numbers, into an
   * RTE the picture. There is no sync setData tier for an object copy, so the
   * TSV rides this async ClipboardItem write on both gestures. An image object
   * copy has no text/plain flavor, so this arm stays inert for images.
   */
  private async writeImageCopy(): Promise<void> {
    // NOTE (deliberate exception to root scoping): navigator.clipboard / ClipboardItem
    // are genuinely process-global (there is no per-realm/per-root clipboard) and
    // are LEFT global by design — the OS clipboard is shared. Only the DOM
    // window/document/selection concerns are root-scoped.
    const hasClipboardItem =
      typeof ClipboardItem !== "undefined" && !!navigator.clipboard?.write;
    if (!hasClipboardItem) {
      this.reportError("no-clipboarditem");
      this.clearStamp();
      return;
    }
    const o = await this.kernel.exec("clipboard.copy");
    const png = o?.flavors["image/png"];
    const html = o?.flavors["text/html"];
    const plain = o?.flavors["text/plain"]; // chart TSV; absent for images
    const types: Record<string, Blob> = {};
    if (png instanceof Blob) types["image/png"] = png; // absent in worker mode
    if (html != null) types["text/html"] = asHtmlBlob(html);
    if (plain != null) types["text/plain"] = asTextBlob(plain);
    if (Object.keys(types).length === 0) {
      this.reportError("image-encode-failed");
      this.clearStamp();
      return;
    }
    try {
      await navigator.clipboard.write([new ClipboardItem(types)]);
      this.reportCopied(o!.seq, 0);
    } catch {
      this.reportError("write-failed");
    }
    this.clearStamp();
  }

  /**
   * The tiered clipboard write. `e` is present only on the copy-event path, which enables the
   * synchronous setData tier. Reporting: every terminal path reports the
   * ACTUAL outcome via clipboard.reportCopied / clipboard.reportError (the plugin
   * is the sole bus emitter). On the copy-event path the setData tier runs
   * FIRST, inside the event tick, from the fresh pre-serialized cache; it is NOT
   * contingent on the async tiers. The dedup stamp is cleared once the
   * write concludes so a fresh gesture on the SAME selection isn't swallowed.
   */
  private async writeThreeTier(e?: ClipboardEvent): Promise<void> {
    const hasClipboardItem =
      typeof ClipboardItem !== "undefined" && !!navigator.clipboard?.write;

    // COPY-EVENT PATH: synchronous setData FIRST, from the fresh cache.
    // text/plain + TSV are both text, so setData carries them. If it lands the
    // copy is DONE (reported now); the async ClipboardItem write below only adds
    // richer flavors and never re-reports (a harmless no-op for plain text).
    if (e) {
      const plain = this.freshPlain();
      if (plain != null && e.clipboardData) {
        e.clipboardData.setData("text/plain", plain);
        // Write the rich-text flavor too. text/html is
        // ADDITIVE — the paste target picks the richest flavor it understands.
        const html = this.freshHtml();
        if (html != null) e.clipboardData.setData("text/html", html);
        e.preventDefault();
        this.reportCopied(this.lastSeq, plain.length);
        if (hasClipboardItem) void this.writeAsync(false).catch(() => {});
        this.clearStamp();
        return;
      }
    }

    // No synchronous content (keydown path, or a stale/worker-mode cache on the
    // copy-event path). Fall to the async ClipboardItem tiers, reporting the
    // actual outcome. No fresh cache AND no ClipboardItem ⇒ report error
    // rather than silently write nothing.
    if (!hasClipboardItem) {
      this.reportError("no-serialization");
      this.clearStamp();
      return;
    }
    await this.writeAsync(true);
    this.clearStamp();
  }

  /** The fresh, seq-matched text/plain for the current selection, or null.
   *  Stale cache: try the ONE in-process sync extract (null in worker mode → we
   *  return null and never write stale/wrong content). */
  private freshPlain(): string | null {
    if (this.localCache && this.localCache.seq === this.lastSeq) return this.localCache.plain;
    if (this.syncSel) {
      if (this.syncSel.kind === "cells") {
        const f = this.viewer.extractRangeSyncInProcess(this.syncSel);
        if (f) return serializeCellsTsv(f);
      } else {
        const f = this.viewer.extractRangeSyncInProcess(this.syncSel);
        if (f) return serializeTextPlain(f);
      }
    }
    return null;
  }

  /** The fresh, seq-matched text/html for the current selection, or null. */
  private freshHtml(): string | null {
    if (this.localCache && this.localCache.seq === this.lastSeq) return this.localCache.html;
    if (this.syncSel) {
      if (this.syncSel.kind === "cells") {
        const f = this.viewer.extractRangeSyncInProcess(this.syncSel);
        if (f) return serializeCellsHtml(f);
      } else {
        const f = this.viewer.extractRangeSyncInProcess(this.syncSel);
        if (f) return serializeTextHtml(f);
      }
    }
    return null; // worker mode / no cache → html rides the async path only
  }

  /**
   * Async ClipboardItem write (tiers 1 + 2). The promise-valued entry keeps
   * transient activation across the async/worker extractRange round-trip (Safari)
   * and RESOLVES to the text blob, or REJECTS when there is no text/plain — a
   * rejected entry aborts the write, so we never clobber the clipboard with an
   * empty text/plain blob. Reports the outcome only when `report` is set
   * (it is NOT on the copy-event path, where the sync setData tier already
   * reported).
   */
  private async writeAsync(report: boolean): Promise<void> {
    const outcomeP = this.kernel.exec("clipboard.copy");
    // Resolve to the write payload, or REJECT when there is no text/plain
    // so a rejected ClipboardItem entry aborts the write — never an empty blob.
    // The payload additionally carries the OPTIONAL text/html blob (null
    // when the flavor is absent, e.g. an all-image selection) so both tiers write
    // it alongside text/plain WITHOUT ever dropping text/plain.
    const payload = outcomeP.then((o) => {
      const plain = o?.flavors["text/plain"];
      if (o == null || plain == null) throw new Error("no-text");
      const html = o.flavors["text/html"];
      return {
        plainBlob: asTextBlob(plain),
        htmlBlob: html == null ? null : asHtmlBlob(html),
        seq: o.seq,
        charCount: o.charCount,
      };
    });
    const plainBlobP = payload.then((p) => p.plainBlob);
    // A promise-valued text/html entry that REJECTS when the flavor is absent, so
    // its ClipboardItem key simply doesn't settle rather than writing empty.
    const htmlBlobP = payload.then((p) => {
      if (p.htmlBlob == null) throw new Error("no-html");
      return p.htmlBlob;
    });
    payload.catch(() => {}); // pre-empt unhandled rejections
    plainBlobP.catch(() => {});
    htmlBlobP.catch(() => {});

    try {
      // Tier 1: promise-valued item, both text flavors (Safari; survives the
      // async round-trip and a clipboard round-trip preserves the HTML).
      await navigator.clipboard.write([
        new ClipboardItem({ "text/plain": plainBlobP, "text/html": htmlBlobP }),
      ]);
      if (report) {
        const p = await payload; // already resolved (the write consumed it)
        this.reportCopied(p.seq, p.charCount);
      }
      return;
    } catch {
      // Tier 2: older Chromium rejects promise values — await then write a Blob
      // map of ONLY the present flavors (text/plain always; text/html when built).
      // `await payload` rethrows on no-text → report error, no empty blob.
      try {
        const p = await payload;
        const types: Record<string, Blob> = { "text/plain": p.plainBlob };
        if (p.htmlBlob) types["text/html"] = p.htmlBlob;
        await navigator.clipboard.write([new ClipboardItem(types)]);
        if (report) this.reportCopied(p.seq, p.charCount);
      } catch {
        if (report) this.reportError("write-failed");
      }
    }
  }

  /** Clear the dedup stamp once a write concludes so a fresh gesture on
   *  the SAME unchanged selection within the window isn't swallowed as a dup. */
  private clearStamp(): void {
    this.lastStamp = null;
  }

  /** Report the ACTUAL write outcome through the plugin (sole emitter). */
  private reportCopied(seq: number, charCount: number): void {
    void this.kernel.exec("clipboard.reportCopied", { seq, charCount });
  }
  private reportError(reason: string): void {
    void this.kernel.exec("clipboard.reportError", { reason });
  }

  destroy(): void {
    if (this.primeTimer) this.scope.win.clearTimeout(this.primeTimer);
    this.sink.removeEventListener("beforeinput", this.onSinkBeforeInput);
    this.sink.removeEventListener("copy", this.onSinkCopy);
    this.scope.win.removeEventListener("keydown", this.onKeyDown);
    for (const u of this.unsubs.splice(0)) u();
    this.sink.remove();
  }
}
