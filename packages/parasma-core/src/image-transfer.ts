/**
 * The Transferable return-list contract for the render
 * worker's generic dispatch (worker.ts). Extracted as a PURE, side-effect-free
 * module so worker.ts imports it AND unit tests exercise it without running
 * worker.ts's top-level `self.onmessage=` boot.
 *
 * It decides which ArrayBuffers ride a worker→main `res` message zero-copy. It
 * subsumes BOTH transfer branches, with the opcodes branch FIRST + duck-typed so
 * the display-list path (renderPageDisplayList → `{ opcodes: Float64Array }`) stays
 * byte-identical.
 *
 * The `imageRefs` branch lists ONE distinct ArrayBuffer per image (the Rust
 * `image_refs` mints a FRESH whole-buffer `Uint8Array` per image via
 * `new_with_length` + `copy_from`, byteOffset 0 — NOT wasm linear memory, never a
 * shared/aliased buffer), so transferring one image's buffer never neuters a
 * sibling and never touches linear memory. A `seen` Set dedups defensively: a
 * duplicate buffer in the transfer list throws `DataCloneError`, so even a
 * hypothetical future shared buffer is listed exactly once.
 *
 * NB: transferring OUT of the worker neuters the worker's OWN `value` bytes — fine
 * (the worker discards them and does NOT paint). The imageRefs branch is
 * `method`-gated so no future worker method that keeps its array is corrupted.
 */
export function collectTransferables(method: string, value: unknown): Transferable[] {
  // Opcodes branch FIRST — duck-typed, unchanged, protects renderPageDisplayList.
  const enc = value as { opcodes?: Float64Array } | null;
  if (enc && enc.opcodes instanceof Float64Array) return [enc.opcodes.buffer];

  if (method === "imageRefs" && Array.isArray(value)) {
    const seen = new Set<ArrayBufferLike>();
    const out: Transferable[] = [];
    for (const r of value as Array<{ bytes?: unknown } | null>) {
      const b = r?.bytes;
      if (b instanceof Uint8Array && b.buffer instanceof ArrayBuffer && !seen.has(b.buffer)) {
        seen.add(b.buffer);
        out.push(b.buffer);
      }
    }
    return out;
  }

  return [];
}
