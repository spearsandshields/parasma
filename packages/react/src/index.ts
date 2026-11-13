/**
 * @parasma/react — React bindings over the parasma viewer.
 *
 * Hook-first: `useParasma(options)` returns the viewer state, typed commands
 * and the raw handle — a LAYER over `@parasma/snippet/headless`, never a
 * gate. `<ParasmaViewer>` is sugar built on it. Importing this package
 * touches no DOM; all work starts once a stage element is attached.
 *
 * Format knowledge (`supportedFormats`, `sniffKind`, `UnsupportedFormatError`)
 * re-exports from @parasma/core so file pickers need no second import.
 */
export { useParasma, type UseParasmaOptions, type UseParasmaResult } from "./use-parasma";

export { ParasmaViewer, type ParasmaViewerProps, type ParasmaSource } from "./parasma-viewer";

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
  ParasmaStatus,
  ParasmaViewState,
  ParasmaSearchState,
  ParasmaCommands,
  ViewerSnapshot,
} from "@parasma/snippet/headless";
