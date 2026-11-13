/**
 * <parasma-viewer> — the secondary entry, a LIGHT-DOM web component wrapping
 * mountViewer. Light DOM (not shadow) so the global @fontsource faces + theme
 * custom props cascade in. observedAttributes re-drive the handle; disconnect
 * destroys it.
 *
 *   <parasma-viewer src="/fixtures/document.docx" theme="dark" controls dev>
 *
 * ## The universal framework binding (no wrapper package required)
 *
 * Vue/Svelte/Angular/Lit (and React 19) bind custom-element PROPERTIES and
 * listen to native CustomEvents, so this element IS the framework binding:
 *
 *  - the {@link ParasmaViewerElement.options} PROPERTY accepts a full
 *    `Partial<MountOptions>` including function-valued fields (contextMenu /
 *    slots / explorer) that can never ride string attributes;
 *  - every {@link SnippetEventMap} event fans out as a DOM CustomEvent named
 *    per {@link SNIPPET_DOM_EVENTS} (bubbles + composed; the single payload as
 *    `detail`).
 *
 * ### Vue recipe
 *
 * ```
 * // vite.config: vue({ template: { compilerOptions: {
 * //   isCustomElement: (t) => t.startsWith("parasma-") } } })
 * <script setup lang="ts">
 * import "@parasma/snippet";            // registers <parasma-viewer>
 * // (SSR-safe to import; the element registers and boots client-only.)
 * import { ref } from "vue";
 * import type { DocumentInfo, MountOptions } from "@parasma/snippet";
 * const page = ref(1);
 * const info = ref<DocumentInfo | null>(null);
 * // HOIST the options object — an inline literal in the template is a fresh
 * // identity every render, and every assignment REMOUNTS the viewer.
 * // The inverse also holds: mutating this object in place never reassigns
 * // identity, so nothing re-applies — assign a NEW object to change options.
 * // (The annotation also types the seam/slot callback params.)
 * const options: Partial<MountOptions> = {
 *   contextMenu: (ctx) => ctx.items.filter((i) => i.id !== "copy-image"),
 *   slots: { viewportOverlay: (h) => makePageNav(h) },
 * };
 * function onPage(e: CustomEvent<number>) { page.value = e.detail; }
 * function onLoad(e: CustomEvent<DocumentInfo>) { info.value = e.detail; }
 * </script>
 * <template>
 *   <parasma-viewer src="/docs/report.docx" theme="dark" controls
 *     :options="options"
 *     @parasma-page-change="onPage"
 *     @parasma-load="onLoad" />
 * </template>
 * ```
 *
 * Svelte: `on:parasma-page-change`; Angular: `(parasma-page-change)`; Lit /
 * vanilla: `addEventListener("parasma-page-change", ...)` — identical contract.
 * React 19 sets `options` as a property pre-commit (single mount) but listens
 * via `ref.addEventListener`. Vue patches props BEFORE insertion (single
 * mount); for vanilla `container.append(el); el.options = {...}` the same-tick
 * microtask absorb below also yields a single mount.
 *
 * ### Precedence (attributes OVERRIDE options when present)
 *
 * `options` is the BASE; observed attributes override their specific fields —
 * attributes are the most explicit, DOM-visible signal.
 *
 * | field         | attribute present            | attribute absent          | attribute REMOVED (live)                 |
 * |---------------|------------------------------|---------------------------|------------------------------------------|
 * | src           | attr wins                    | options.src               | no-op (mount-time src, not a reload)     |
 * | theme         | attr wins (incl. "auto")     | options.theme ?? "auto"   | setTheme(options.theme) when concrete; "auto" cannot re-resolve without remount |
 * | controls      | true                         | options.controls ?? false | remount (re-reads precedence)            |
 * | dev           | true                         | options.dev ?? false      | setDev(options.dev ?? false)             |
 * | local-fonts   | true                         | options.useLocalFonts     | remount                                  |
 * | explorer      | empty-state panel (lowest)   | see below                 | remount                                  |
 *
 * BOOLEAN ASYMMETRY: presence-only attributes override only to TRUE; absence
 * is "unset", not "false" — to force false, omit the attribute and set
 * `options.controls = false`. NOTE the element defaults `controls` to FALSE
 * (bare-element back-compat) while direct mountViewer defaults it TRUE.
 *
 * EXPLORER exception: the bare `explorer` attribute carries NO data, so the
 * {@link ParasmaViewerElement.explorerConfig} property (most specific, kept for
 * back-compat) > `options.explorer` > bare-attr empty-state.
 *
 * ### Event contract
 *
 * CustomEvents are DELTAS after mount; read initial/reset state from
 * `el.viewerHandle.state` in your parasma-load/parasma-unload handlers (the
 * initial themeChange fires during mountViewer, before subscription; the
 * doc-switch pageChange reset is contractually silent). Teardown fires NO
 * parasma-unload: the real destroy() never emits `unload` (only warm unload()/
 * doc-switch does).
 */
import { mountViewer } from "./mount";
import type {
  ViewerHandle,
  ExplorerConfig,
  MountOptions,
  SnippetEvent,
  SnippetEventMap,
} from "./types";

/**
 * SnippetEventMap key → DOM CustomEvent type dispatched by <parasma-viewer>.
 * Uniform `parasma-` prefix + kebab-case: collision-safe ("load"/"error" are
 * native DOM event types with global machinery — error-tracking SDKs listen to
 * window "error", and these events bubble composed), and kebab-case is the
 * custom-elements-everywhere convention (`@parasma-page-change` in Vue,
 * `on:parasma-page-change` in Svelte, `(parasma-page-change)` in Angular).
 *
 * `satisfies Record<SnippetEvent, string>` = compile-time exhaustiveness: a new
 * SnippetEventMap entry fails tsc until it is named here.
 *
 * CONTRACT: CustomEvents are DELTAS after mount; re-sync from
 * `el.viewerHandle.state` in your parasma-load/parasma-unload handlers (the
 * initial themeChange and the silent doc-switch page reset are not
 * re-dispatched). Teardown fires NO parasma-unload (destroy() never emits
 * `unload`). Flags: bubbles:true, composed:true; `detail` is
 * the single event payload (null for the arg-less unload — the DOM default).
 */
export const SNIPPET_DOM_EVENTS = {
  load: "parasma-load",
  unload: "parasma-unload",
  error: "parasma-error",
  pageChange: "parasma-page-change",
  layoutTruncated: "parasma-layout-truncated",
  sheetChange: "parasma-sheet-change",
  zoomChange: "parasma-zoom-change",
  themeChange: "parasma-theme-change",
  linkActivate: "parasma-link-activate",
  cellSelect: "parasma-cell-select",
  commentFocus: "parasma-comment-focus",
  printProgress: "parasma-print-progress",
} as const satisfies Record<SnippetEvent, string>;

/**
 * Events dispatched CANCELABLE, where `preventDefault()` on the DOM event feeds
 * back into the viewer's decision. This is why link activation is an event and
 * not a function option: a function cannot ride an HTML attribute, so a
 * plain-markup consumer could never reach it — `addEventListener` +
 * `preventDefault()` works from anywhere.
 */
const CANCELABLE_DOM_EVENTS = new Set<SnippetEvent>(["linkActivate"]);

/**
 * DOM event name → CustomEvent<payload> map for typed addEventListener.
 * Arg-less events (unload) carry `detail: null` — the DOM spec defaults
 * CustomEventInit.detail to null (browsers normalize undefined → null).
 */
export type ParasmaViewerEventMap = {
  [E in SnippetEvent as (typeof SNIPPET_DOM_EVENTS)[E]]: CustomEvent<
    SnippetEventMap[E] extends [infer D] ? D : null
  >;
};

// SSR-safe base: `extends HTMLElement` evaluates at module scope, so importing
// this module server-side (Nuxt/SSR) must not throw before registerElement's
// own guard even runs. The element still registers and boots client-only.
const Base: typeof HTMLElement =
  typeof HTMLElement !== "undefined"
    ? HTMLElement
    : (class {} as unknown as typeof HTMLElement);

// Declaration-merged typed listener overloads (zero runtime, and NO global
// HTMLElementEventMap augmentation — the types stay scoped to this element).
export interface ParasmaViewerElement {
  addEventListener<K extends keyof ParasmaViewerEventMap>(
    type: K,
    listener: (this: ParasmaViewerElement, ev: ParasmaViewerEventMap[K]) => void,
    options?: boolean | AddEventListenerOptions,
  ): void;
  addEventListener(
    type: string,
    listener: EventListenerOrEventListenerObject,
    options?: boolean | AddEventListenerOptions,
  ): void;
  removeEventListener<K extends keyof ParasmaViewerEventMap>(
    type: K,
    listener: (this: ParasmaViewerElement, ev: ParasmaViewerEventMap[K]) => void,
    options?: boolean | EventListenerOptions,
  ): void;
  removeEventListener(
    type: string,
    listener: EventListenerOrEventListenerObject,
    options?: boolean | EventListenerOptions,
  ): void;
}

export class ParasmaViewerElement extends Base {
  static get observedAttributes(): string[] {
    return ["src", "theme", "controls", "dev", "explorer", "local-fonts"];
  }

  private handle: ViewerHandle | null = null;
  private booting = false;
  /**
   * Remount epoch: bumped by remount()/disconnect so an IN-FLIGHT boot whose
   * await resolves late can tell it was superseded and destroy its handle
   * instead of assigning it (the React-18-StrictMode mount/unmount shape used
   * to leak a full kernel + wasm Viewer + a window keydown listener).
   */
  private bootEpoch = 0;
  /**
   * Set when a config/attribute change lands while a boot is in flight AFTER
   * its snapshot was taken (a pre-snapshot / same-tick change is absorbed by
   * the deferred snapshot instead); boot()'s finally re-runs remount() so the
   * change is never dropped.
   */
  private pendingRemount = false;
  /**
   * True once the in-flight boot has read attributes+options. Until then a
   * change needs NO remount — the microtask-deferred snapshot will see it.
   */
  private snapshotTaken = false;
  private _explorerConfig: ExplorerConfig | null = null;
  private _options: Partial<MountOptions> | null = null;
  /** Active CustomEvent fan-out unsubscribers — exactly one set per mounted handle. */
  private fanoutUnsubs: Array<() => void> = [];

  /**
   * The full config PROPERTY — a `Partial<MountOptions>` INCLUDING the
   * function-valued fields (contextMenu / slots / explorer)
   * that can never ride string attributes. This is the universal framework
   * binding seam: Vue `:options`, React 19 property, Lit `.options`.
   *
   * Semantics: pre-mount it stores; EVERY live assignment REMOUNTS (the
   * explorerConfig precedent — no diffing, no per-field live-apply). Assign a
   * NEW object to change options: in-place mutation of a previously-assigned
   * (or reactive) object never reassigns identity, so NOTHING re-applies
   * (silently stale). The getter returns the live stored reference — mutating
   * it without re-assignment silently changes what the NEXT remount reads but
   * triggers none. Cheap live changes stay on the existing channels: the
   * theme/dev/src attributes live-apply, and `viewerHandle.setTheme/setDev/
   * zoom/goToPage` serve imperative hosts without a remount.
   *
   * Precedence: options is the BASE; present observed attributes OVERRIDE
   * their specific fields (see the header table).
   */
  set options(o: Partial<MountOptions> | null) {
    this._options = o;
    if (this.handle) void this.remount();
    else if (this.booting && this.snapshotTaken) this.pendingRemount = true;
    // else: plain store — the pending boot's deferred snapshot (or the next
    // connect) consumes it.
  }
  get options(): Partial<MountOptions> | null {
    return this._options;
  }

  /**
   * Imperative data path for the explorer panel. HTML attributes are strings, so
   * the bare `<parasma-viewer explorer>` attribute alone yields an EMPTY-state
   * panel; the real entries+open resolver come through this setter (set BEFORE
   * connect, or it remounts). file selection is the host's job.
   *
   * The general path is {@link ParasmaViewerElement.options}.explorer — this
   * setter predates it and wins over it for back-compat.
   */
  set explorerConfig(cfg: ExplorerConfig | null) {
    this._explorerConfig = cfg;
    if (this.handle) void this.remount();
    else if (this.booting && this.snapshotTaken) this.pendingRemount = true;
  }
  get explorerConfig(): ExplorerConfig | null {
    return this._explorerConfig;
  }

  /**
   * Rescue a pre-upgrade own-property shadow (the canonical custom-element
   * footgun): a consumer setting `el.options` BEFORE the element upgrades
   * creates an own DATA property that permanently shadows the class accessor.
   * Delete it and re-assign through the accessor.
   */
  private upgradeProperty(prop: "options" | "explorerConfig"): void {
    if (Object.prototype.hasOwnProperty.call(this, prop)) {
      const v = (this as Record<string, unknown>)[prop];
      delete (this as Record<string, unknown>)[prop];
      (this as Record<string, unknown>)[prop] = v; // routes through the accessor
    }
  }

  connectedCallback(): void {
    this.upgradeProperty("options");
    this.upgradeProperty("explorerConfig");
    // Re-connected while a (now-superseded) boot is still in flight: queue a
    // remount — boot()'s finally runs it once the stale mount settles.
    if (this.booting) {
      this.pendingRemount = true;
      return;
    }
    void this.boot();
  }

  disconnectedCallback(): void {
    this.detachFanout();
    // Supersede any in-flight boot (its post-await isConnected/epoch check
    // destroys the handle it was about to assign).
    this.bootEpoch++;
    // Fire-and-forget: a lifecycle callback cannot await. destroy() is
    // self-contained + idempotent, so its async tail settling after this sync
    // return is observably identical to the old synchronous teardown.
    void this.handle?.destroy();
    this.handle = null;
  }

  attributeChangedCallback(name: string, oldV: string | null, newV: string | null): void {
    if (oldV === newV) return;
    if (!this.handle) {
      // Mid-boot pre-snapshot (same tick as connect): the deferred snapshot
      // absorbs this change — no remount needed. Post-snapshot: queue one.
      // Teardown window (remount in flight, handle nulled): drop — the
      // imminent boot re-reads everything.
      if (this.booting && this.snapshotTaken) this.pendingRemount = true;
      return;
    }
    switch (name) {
      case "src":
        // Removal is a documented no-op: options.src is a mount-time src, not
        // a live reload trigger.
        if (newV) void this.handle.loadUrl(newV);
        break;
      case "theme": {
        // Removal (or an explicit "auto") falls back to a CONCRETE
        // options.theme; "auto" cannot re-resolve prefers-color-scheme without
        // a remount (setTheme only takes light|dark) → documented no-op.
        const t =
          newV === "light" || newV === "dark"
            ? newV
            : this._options?.theme === "light" || this._options?.theme === "dark"
              ? this._options.theme
              : null;
        if (t) this.handle.setTheme(t);
        break;
      }
      case "dev":
        // Removal restores the options base (NOT hard false).
        this.handle.setDev(newV !== null ? true : (this._options?.dev ?? false));
        break;
      case "controls":
        // controls toggling at runtime requires a remount (re-reads precedence).
        void this.remount();
        break;
      case "explorer":
        // Presence/absence of the panel requires a remount.
        void this.remount();
        break;
      case "local-fonts":
        // Toggling the local-font opt-in re-plugs the fonts plugin → remount.
        void this.remount();
        break;
    }
  }

  private async boot(): Promise<void> {
    if (this.handle || this.booting) return;
    this.booting = true;
    this.snapshotTaken = false;
    const epoch = ++this.bootEpoch;
    try {
      // SAME-TICK ABSORB: defer the attribute+options snapshot one microtask so
      // `container.append(el); el.options = {...}` (and framework hydration
      // patching props right after insertion) costs ONE kernel+wasm boot, not
      // two.
      await Promise.resolve();
      if (!this.isConnected || epoch !== this.bootEpoch) return; // superseded during the microtask
      this.snapshotTaken = true;
      // PRECEDENCE SNAPSHOT: options is the BASE (spread passes through every
      // attribute-less field — contextMenu, slots,
      // localFontsStorage, search, asset/worker config — BY REFERENCE);
      // present attributes override their specific fields.
      const o = this._options ?? {};
      const themeAttr = this.getAttribute("theme");
      const handle = await mountViewer(this, {
        ...o,
        src: this.getAttribute("src") ?? o.src,
        // An explicit theme="auto" attribute is PRESENT and overrides
        // options.theme back to auto.
        theme:
          themeAttr === "light" || themeAttr === "dark" || themeAttr === "auto"
            ? themeAttr
            : (o.theme ?? "auto"),
        // BOOLEAN ASYMMETRY: presence overrides to true; absence defers to
        // options. The element's bare default stays controls:false (mountViewer
        // alone defaults true) — back-compat.
        controls: this.hasAttribute("controls") ? true : (o.controls ?? false),
        dev: this.hasAttribute("dev") ? true : (o.dev ?? false),
        // ABSENT attribute + unset option must map to undefined, not false: an
        // explicit boolean always wins over the persisted one-click choice
        // (mount precedence), so a bare <parasma-viewer> would otherwise
        // silently discard the user's "Use installed fonts" grant on reload.
        useLocalFonts: this.hasAttribute("local-fonts") ? true : o.useLocalFonts,
        // Explorer exception: explorerConfig property (back-compat, most
        // specific) > options.explorer > bare-attr empty-state (the attribute
        // carries no data; the snippet ships no fixtures).
        explorer:
          this._explorerConfig ??
          o.explorer ??
          (this.hasAttribute("explorer") ? { entries: [], open: (id) => id } : undefined),
      });
      // The element may have been REMOVED (React 18 StrictMode mounts, then
      // synchronously unmounts) or this boot SUPERSEDED (a remount) while the
      // async mount ran. Assigning would leak a live kernel + wasm Viewer + the
      // window keydown listener forever — destroy instead (NEVER subscribed:
      // fan-out only attaches to a handle that is actually assigned).
      if (!this.isConnected || epoch !== this.bootEpoch) {
        void handle.destroy();
        return;
      }
      this.handle = handle;
      // Synchronous in this post-await continuation: mountViewer's src load is
      // fire-and-forget (mount.ts invariant), so this subscription lands before
      // any network-bound load settles → the initial "parasma-load" IS
      // delivered. (The initial themeChange fired DURING mountViewer and the
      // doc-switch page reset is contractually silent — deltas-after-mount.)
      this.attachFanout(handle);
    } finally {
      this.booting = false;
      if (this.pendingRemount) {
        this.pendingRemount = false;
        void this.remount();
      }
    }
  }

  private async remount(): Promise<void> {
    // Mid-boot remount: let the in-flight boot finish, then its finally re-runs
    // us (re-entering boot() while booting would silently no-op).
    if (this.booting) {
      this.pendingRemount = true;
      return;
    }
    this.bootEpoch++; // supersede any stale in-flight boot's assignment
    // Capture-null-detach: viewerHandle stops exposing a dying handle for the
    // whole teardown window; mid-window attribute/options changes route to the
    // !handle branch (harmless — the imminent boot re-reads everything).
    const h = this.handle;
    this.handle = null;
    this.detachFanout();
    // AWAIT the teardown before the successor boots (parent handoff):
    // serialize worker-terminate ahead of the new boot, removing a same-slot
    // terminate/boot overlap.
    await h?.destroy();
    if (this.isConnected) await this.boot();
  }

  /** Subscribe every SnippetEventMap event on the handle → DOM CustomEvents. */
  private attachFanout(handle: ViewerHandle): void {
    // Locally enforce "exactly one subscription set per element": a double
    // attach would otherwise orphan the previous unsub set. Unsubs are
    // Set.delete-based, so the redundant detach is always safe.
    this.detachFanout();
    this.fanoutUnsubs = (Object.keys(SNIPPET_DOM_EVENTS) as SnippetEvent[]).map((ev) => {
      const cancelable = CANCELABLE_DOM_EVENTS.has(ev);
      return handle.on(ev, (...args: unknown[]) => {
        const detail = args[0];
        const ok = this.dispatchEvent(
          new CustomEvent(SNIPPET_DOM_EVENTS[ev], {
            detail,
            bubbles: true,
            composed: true,
            cancelable,
          }),
        );
        // dispatchEvent returns false iff a listener called preventDefault().
        // Feed that back into the payload so the viewer honours a veto made
        // through the DOM event exactly as one made on the JS handle.
        if (cancelable && !ok) {
          (detail as { preventDefault?: () => void } | undefined)?.preventDefault?.();
        }
      });
    });
  }

  private detachFanout(): void {
    for (const u of this.fanoutUnsubs) u();
    this.fanoutUnsubs = [];
  }

  /** Escape hatch for imperative callers. */
  get viewerHandle(): ViewerHandle | null {
    return this.handle;
  }
}

/**
 * Register the viewer custom element (idempotent). `tag` wires
 * ViewerConfig.elementTag (coexistence) so version skew doesn't silently
 * alias `<parasma-viewer>` to the first build's class. Returns the resolved tag.
 *
 * The custom-element registry is process-global (a deliberate exception to
 * root scoping: one registry per document, mitigated by the configurable tag). `registry` is
 * injectable purely for unit testing; production uses the global customElements.
 *
 * - fresh tag → define() (as today).
 * - SAME class re-registered → silent no-op (idempotent).
 * - a DIFFERENT class already under the tag → warn + return WITHOUT redefining
 *   (customElements.define throws on a duplicate tag — two Parasma builds aliasing
 *   the same tag is a coexistence hazard we surface, not crash on).
 */
export function registerElement(
  tag = "parasma-viewer",
  registry: CustomElementRegistry | undefined =
    typeof customElements !== "undefined" ? customElements : undefined,
): string {
  if (!registry) return tag;
  const existing = registry.get(tag);
  if (existing === undefined) {
    registry.define(tag, ParasmaViewerElement);
  } else if (existing !== ParasmaViewerElement) {
    console.warn(
      `[parasma] custom element <${tag}> is already registered to a different class ` +
        `(two Parasma builds aliasing the same tag?). Skipping re-registration; ` +
        `pass a distinct ViewerConfig.elementTag to coexist.`,
    );
  }
  return tag;
}
