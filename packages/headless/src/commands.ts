/**
 * Command bus. Plugins reach each other only via `kernel.exec('id.command')`
 * — never by import. A command is a namespaced async function;
 * `exec` looks it up and invokes it.
 *
 * TYPED: {@link CommandMap} is an OPEN interface each plugin
 * declaration-merges (co-located with its register block, targeting `../commands`
 * — the same module the barrel re-exports, so the augmentation is program-wide).
 * A registered id + its handler signature live in the map, so `exec("id", …)` is
 * type-checked: a command-id TYPO, a wrong ARG, or a wrong RETURN is a COMPILE
 * error at the call site. Every registered id MUST be declared — an undeclared id
 * matches NEITHER register overload (a hard error at the register site), which is
 * what keeps the map complete + symmetric.
 *
 * DYNAMIC HATCH: a genuinely-runtime id opts in via {@link asDynamic}. The brand
 * is REQUIRED, so a raw string literal is NOT assignable to `DynamicCommandId` —
 * a typo can't launder through the fallback overload; there is deliberately NO
 * permissive `(id: string, …)` overload (that would swallow typos, nullifying
 * the type check).
 */

/**
 * The registry of command ids → handler signatures. EMPTY in the kernel (the
 * kernel owns NO command semantics); plugins augment it via
 * `declare module "../commands" { interface CommandMap { … } }`.
 */
// eslint-disable-next-line @typescript-eslint/no-empty-interface
export interface CommandMap {}

/** The union of all declared command ids. */
export type CommandId = keyof CommandMap & string;

// The brand carrier for a genuinely-dynamic id (never constructed).
declare const DYNAMIC: unique symbol;
/**
 * A branded string for a runtime-built command id (the escape hatch). REQUIRED
 * brand ⇒ a raw string literal is NOT assignable, so a typo can't fall through
 * the dynamic overload — callers opt in explicitly with {@link asDynamic}.
 */
export type DynamicCommandId = string & { readonly [DYNAMIC]: true };

/** Opt a runtime-built string into the dynamic-id escape hatch. */
export const asDynamic = (id: string): DynamicCommandId => id as DynamicCommandId;

/** The fallback handler shape (dynamic ids + the factory internals). */
export type Command = (...args: any[]) => unknown | Promise<unknown>;

export interface CommandBus {
  /**
   * Register a command under a declared id (typed handler), returning an
   * unregister thunk (mirrors `bus.subscribe`): push it onto the plugin's
   * `unsubs` and let destroy() splice-and-call it, instead of hand-maintaining a
   * parallel list of `unregister(id)` calls (removes a drift hazard).
   */
  register<Id extends CommandId>(id: Id, command: CommandMap[Id]): () => void;
  /** Register a genuinely-dynamic id (the branded escape hatch). */
  register(id: DynamicCommandId, command: Command): () => void;
  /** Remove a command (used on plugin destroy). Lookup — stays string-keyed. */
  unregister(id: string): void;
  /** True if a command id is registered. Lookup — stays string-keyed. */
  has(id: string): boolean;
  /** Invoke a declared command by id: args + return are checked against the map. */
  exec<Id extends CommandId>(
    id: Id,
    ...args: Parameters<CommandMap[Id]>
  ): Promise<Awaited<ReturnType<CommandMap[Id]>>>;
  /** Invoke a genuinely-dynamic id (the branded escape hatch). */
  exec<R = unknown>(id: DynamicCommandId, ...args: unknown[]): Promise<R>;
}

export function createCommandBus(): CommandBus {
  const commands = new Map<string, Command>();
  const bus: CommandBus = {
    register(id: string, command: Command): () => void {
      if (commands.has(id)) {
        throw new Error(`[parasma:headless] command already registered: ${id}`);
      }
      commands.set(id, command);
      // The teardown thunk — idempotent (a second call is a no-op delete).
      return () => {
        commands.delete(id);
      };
    },
    unregister(id) {
      commands.delete(id);
    },
    has(id) {
      return commands.has(id);
    },
    async exec(id: string, ...args: unknown[]): Promise<never> {
      const command = commands.get(id);
      if (!command) {
        throw new Error(`[parasma:headless] unknown command: ${id}`);
      }
      return (await command(...args)) as never;
    },
  };
  return bus;
}
