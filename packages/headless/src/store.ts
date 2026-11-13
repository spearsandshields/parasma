/**
 * Per-plugin state-slice helper (@xstate/store-shaped).
 *
 * The original design called for a "slice-per-plugin shared store" the kernel would compose
 * into one immutable snapshot. That composition was NEVER wired: the kernel
 * builds no store, and every stateful plugin instead owns its OWN single-slice
 * store, reading only its own state. The cross-plugin read-model is the bus's
 * replayable sticky events, not a composed snapshot. So this is honestly a
 * per-plugin SINGLE-slice helper — `createSliceStore(slice)` — not a kernel
 * composer. We wrap `@xstate/store` so the surface is `get`/`dispatch`/`watch`
 * and never leaks the library shape to consumers.
 */

import { createStore } from "@xstate/store";

/** A plugin's store slice: its initial state + the events that mutate it. */
export interface StoreSlice<State extends Record<string, unknown>> {
  /** Initial state for this slice. */
  state: State;
  /**
   * Pure transitions, @xstate/store-shaped: each returns the NEXT state
   * (immutably) from the current state + event payload.
   */
  on: {
    [event: string]: (context: State, payload: any) => State;
  };
}

/** A single-slice store: one plugin's state + the events that mutate it. */
export interface Store<State extends Record<string, unknown>> {
  /** Current immutable state. */
  get(): State;
  /** Dispatch an event to a transition (by its bare `on` key). */
  dispatch(event: string, payload?: unknown): void;
  /** Subscribe to state changes. Returns an unsubscribe. */
  watch(listener: (state: State) => void): () => void;
}

/**
 * Build a plugin's single-slice store. The slice's `on` keys are the raw event
 * types; `dispatch(event, payload)` routes to the owning transition, and `get()`
 * returns the raw state object (there is no per-slice wrapper — one plugin, one
 * slice).
 */
export function createSliceStore<State extends Record<string, unknown>>(
  slice: StoreSlice<State>,
): Store<State> {
  const on: Record<string, (ctx: State, ev: { payload: unknown }) => State> = {};
  for (const event of Object.keys(slice.on)) {
    const transition = slice.on[event]!;
    on[event] = (ctx, ev) => transition(ctx, ev.payload);
  }

  const store = createStore({ context: slice.state, on });

  return {
    get() {
      return store.getSnapshot().context as State;
    },
    dispatch(event, payload) {
      store.send({ type: event, payload } as never);
    },
    watch(listener) {
      const sub = store.subscribe((snapshot) => {
        listener(snapshot.context as State);
      });
      return () => sub.unsubscribe();
    },
  };
}
