/**
 * @parasma/snippet/chrome — the Tier-C chrome-factory barrel.
 *
 * @advanced Tier C of the customization ladder (A: attributes and options →
 * B: slots → C: THIS → full headless). These are the SAME primitives mountViewer composes into
 * the default chrome, exported so a BYO-chrome consumer on
 * `@parasma/snippet/headless` can keep individual pieces while replacing others
 * — e.g. keep {@link createSearchBox} and {@link installSlots}' overlay while
 * authoring your own toolbar.
 *
 * SIDE-EFFECT-FREE (the load-bearing invariant, mirroring headless-viewer.ts's
 * contract): importing this subpath registers NO custom element and touches NO
 * global. The barrel reaches ONLY chrome/*.ts + ../slots.ts + ../dom.ts (via the
 * factories) + type-only ../types / ../root-scope / @parasma/headless /
 * @parasma/core. It must NOT import ../element, ../index, ../mount,
 * ../headless-viewer, any .css, or @fontsource (grep-verified: no
 * `customElements.define` reachable from here; pinned by chrome-barrel.test.ts).
 *
 * Contract notes:
 *  1. Each factory returns a handle exposing `root` (YOU insert it) and, where
 *     stateful, `destroy()` — call it before tearing down the kernel, in the
 *     reverse of the creation order mountViewer uses.
 *  2. The Deps interfaces are the exact seams mount.ts wires — `kernel`/`scope`
 *     come from a HeadlessViewerHandle's escape hatches (`handle.kernel`,
 *     `handle.scope`), computeFit/scroll callbacks from `handle.dom`.
 *  3. The ps-* classnames are styled by the stylesheet the `.` / `./headless`
 *     entries ship — these factories assume a Parasma viewer is mounted on the
 *     page. Importing CSS here would itself be a side effect, so this barrel
 *     deliberately imports none.
 *  4. Types not re-exported here (e.g. RootScope) are erasure-safe to
 *     `import type {...} from "@parasma/snippet"` — type-only imports never
 *     execute the side-effectful root barrel.
 *  5. STABILITY: the Deps shapes mirror the default chrome's internal
 *     composition and may widen in minor versions while the package is pre-1.0.
 */

export { createToolbar, type ToolbarDeps, type ToolbarHandle } from "./toolbar";
export { createSearchBox, type SearchBoxDeps, type SearchBoxHandle } from "./search-box";
export { createExplorer, type CreateExplorerDeps, type ExplorerHandle } from "./explorer";
export { createSheetTabs, type SheetTabsDeps, type SheetTabsHandle } from "./sheet-tabs";
// FormulaCell's actual HOME is formula-bar.ts — ../types itself imports it from here.
export { createFormulaBar, type FormulaBarHandle, type FormulaCell } from "./formula-bar";
export { createCommentsPanel, type CommentsPanelDeps, type CommentsPanelHandle } from "./comments-panel";
export { icon, type IconName } from "./icons";
export { installSlots, type InstallSlotsDeps, type InstalledSlots } from "../slots";
// Type homes, so a chrome-only consumer never needs the side-effectful root
// barrel: ExplorerConfig/Entry/Format feed CreateExplorerDeps; ViewerHandle +
// MountOptions are referenced by InstallSlotsDeps (same standalone-use
// rationale). Type-only — erased at runtime, Object.keys surface unchanged.
export type {
  ExplorerConfig,
  ExplorerEntry,
  ExplorerFormat,
  ViewerHandle,
  MountOptions,
} from "../types";
