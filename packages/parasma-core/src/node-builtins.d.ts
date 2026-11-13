/**
 * Hand-rolled typings for the sliver of Node's stdlib the TEST suite touches.
 *
 * @parasma/core ships to the browser and has no Node dependency; pulling in
 * `@types/node` to typecheck one test would put Node's entire global surface
 * (Buffer, process, setImmediate, a Node-flavoured `fetch`) in scope for every
 * source file in the package and quietly permit exactly the leakage this
 * declaration exists to avoid. So: declare what is used, nothing more.
 *
 * Sole consumer: `font-pinning.test.ts`, which reads
 * `scripts/fetch-test-fonts.sh` to prove the browser and native font pins agree.
 * If a second consumer appears, that is the moment to reconsider.
 */
declare module "node:fs" {
  export function readFileSync(path: string | URL, encoding: "utf8"): string;
}
