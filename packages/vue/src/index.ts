/**
 * @parasma/vue — Vue-native composition over the parasma viewer.
 *
 * Composable-first: `useParasma(rootRef, options)` returns reactive state, a
 * machine-grade `status`, typed commands, and the raw handle — a LAYER over
 * `@parasma/snippet/headless`, never a gate. Module-scope-pure: importing
 * this package touches no DOM (safe under SSR/Node import; all work starts
 * at component mount).
 *
 * Format knowledge (`supportedFormats`, `sniffKind`, `UnsupportedFormatError`)
 * re-exports from @parasma/core so dispatch shells and file pickers need no
 * second import.
 */
export {
  useParasma,
  type UseParasmaOptions,
  type UseParasmaReturn,
  type ParasmaStatus,
  type ParasmaViewState,
  type ParasmaCommands,
  type ParasmaSearchState,
} from "./use-parasma";

export { ParasmaViewer, type ParasmaSource } from "./parasma-viewer";

export {
  sniffKind,
  supportedFormats,
  UnsupportedFormatError,
  type SupportedFormat,
} from "@parasma/core";

export type {
  HeadlessViewerHandle,
  HeadlessViewerConfig,
  FormulaCell,
  LinkActivateEvent,
  Theme,
  ThemeOption,
} from "@parasma/snippet/headless";
