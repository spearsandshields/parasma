/**
 * useParasma — the React binding over the framework-neutral viewer session.
 *
 * Hook-first, like the Vue package: state is the session snapshot read
 * through useSyncExternalStore, so any component can render it and a
 * BYO toolbar needs nothing else. `<ParasmaViewer>` is sugar built ON this;
 * nothing is reachable only through it, and `handle` is always exposed.
 *
 * Lifecycle: the viewer boots once the stage element is attached (pass
 * `stageRef` as the `ref` of an EMPTY element — React must never render
 * children inside it, the engine owns that subtree) and is destroyed on
 * unmount. Boots and teardowns into one element are serialised, so React
 * StrictMode's mount → unmount → mount never runs two viewers in the same
 * element. Options are read when the viewer boots; change them by remounting
 * (a new `key`).
 */
import { useCallback, useEffect, useMemo, useRef, useState, useSyncExternalStore } from "react";
import {
  createHeadlessViewer,
  createViewerSession,
  type HeadlessViewerConfig,
  type HeadlessViewerHandle,
  type ParasmaCommands,
  type ParasmaViewState,
  type ViewerSession,
  type ViewerSnapshot,
} from "@parasma/snippet/headless";

export interface UseParasmaOptions extends Omit<HeadlessViewerConfig, "root"> {
  /** Applied after each successful load. Storage stays yours. */
  initialViewState?: Partial<ParasmaViewState>;
}

export interface UseParasmaResult extends ViewerSnapshot {
  /** Callback ref for the stage: an empty element the viewer renders into. */
  stageRef: (el: HTMLElement | null) => void;
  load(bytes: ArrayBuffer | Uint8Array, name?: string): Promise<void>;
  unload(): Promise<void>;
  commands: ParasmaCommands;
  viewState(): ParasmaViewState;
  applyViewState(s: Partial<ParasmaViewState>): void;
  // ── the escape hatch (the layer-not-gate guarantee) ──
  /** null until the stage is attached and the viewer has booted. */
  handle: HeadlessViewerHandle | null;
  /** Resolves when the handle exists. */
  ready: Promise<HeadlessViewerHandle>;
  isReady: boolean;
}

export function useParasma(options: UseParasmaOptions = {}): UseParasmaResult {
  // Boot-time options: captured on first render, like the session itself.
  const [boot] = useState(() => {
    const { initialViewState, ...config } = options;
    return { config, session: createViewerSession({ initialViewState }) as ViewerSession };
  });
  const { session, config } = boot;
  const snap = useSyncExternalStore(session.subscribe, session.getSnapshot, session.getSnapshot);
  const [stage, setStage] = useState<HTMLElement | null>(null);
  const [handle, setHandle] = useState<HeadlessViewerHandle | null>(null);
  /** Settles when the previous viewer in this hook is fully destroyed. */
  const settled = useRef<Promise<unknown>>(Promise.resolve());

  useEffect(() => {
    if (!stage) return;
    let disposed = false;
    const booted = settled.current.then(async () => {
      if (disposed) return null;
      try {
        const h = await createHeadlessViewer({ ...config, root: stage });
        if (disposed) {
          // Unmounted while booting: nothing was wired — reclaim and stop.
          await h.destroy();
          return null;
        }
        session.connect(h, stage);
        setHandle(h);
        return h;
      } catch (e) {
        if (!disposed) session.fail(e);
        return null;
      }
    });
    return () => {
      disposed = true;
      // Teardown: unhook while alive, THEN destroy.
      session.disconnect();
      setHandle(null);
      settled.current = booted.then((h) => h?.destroy()).catch(() => {});
    };
  }, [stage, session, config]);

  const stageRef = useCallback((el: HTMLElement | null) => setStage(el), []);

  // A new result only when the snapshot or the handle changes: `<ParasmaViewer>`
  // hands it to its ref, and a ref that stores it in state must not re-render
  // forever.
  return useMemo(
    () => ({
      ...snap,
      stageRef,
      load: session.load,
      unload: session.unload,
      commands: session.commands,
      viewState: session.viewState,
      applyViewState: session.applyViewState,
      handle,
      // A getter: the session re-arms `ready` on disconnect.
      get ready() {
        return session.ready;
      },
      isReady: handle !== null,
    }),
    [snap, stageRef, session, handle],
  );
}
