/**
 * The Transferable return-list contract for the worker's
 * generic dispatch. `collectTransferables` decides which ArrayBuffers ride a
 * worker→main response zero-copy. The two crux invariants under test:
 *
 *  1. the display-list path (renderPageDisplayList → `{ opcodes: Float64Array }`)
 *     keeps transferring EXACTLY `[opcodes.buffer]` and nothing else;
 *  2. `imageRefs` transfers ONE distinct buffer PER image with NO cross-neuter
 *     and NO DataCloneError on an aliased/duplicated buffer.
 *
 * Each test states its RED condition.
 */
import { describe, it, expect } from "vitest";
import { collectTransferables } from "./image-transfer";

type Ref = { relId: string; mime: string; bytes: Uint8Array };
const ref = (relId: string, mime: string, bytes: Uint8Array): Ref => ({ relId, mime, bytes });

describe("collectTransferables — imageRefs branch", () => {
  it("T1 per-image: lists exactly the distinct buffers, one per image", () => {
    const a = ref("rId1", "image/png", new Uint8Array([1, 2, 3, 4]));
    const b = ref("rId2", "image/jpeg", new Uint8Array([9, 9]));
    const out = collectTransferables("imageRefs", [a, b]);
    expect(out).toEqual([a.bytes.buffer, b.bytes.buffer]);
    expect(new Set(out).size).toBe(2); // RED: a dup/miss would collapse or omit
  });

  it("T2 dedup: two entries sharing ONE buffer are listed once (guards DataCloneError)", () => {
    const shared = new Uint8Array([1, 2, 3, 4, 5, 6]);
    const v0 = shared.subarray(0, 3);
    const v1 = shared.subarray(3, 6);
    const out = collectTransferables("imageRefs", [ref("a", "image/png", v0), ref("b", "image/png", v1)]);
    // Same underlying ArrayBuffer → listed exactly once (a repeat throws DataCloneError).
    expect(out).toEqual([shared.buffer]);
  });

  it("T3 opcodes-intact + no spurious transfer for non-image values", () => {
    const opcodes = new Float64Array([1, 2, 3]);
    // opcodes branch FIRST + duck-typed — protects the display-list path.
    expect(collectTransferables("renderPageDisplayList", { opcodes, strings: [] })).toEqual([opcodes.buffer]);
    // a scalar / a non-image array (linkRects/comments shape) → no transfer.
    expect(collectTransferables("linkRects", [{ x: 0, y: 0, w: 1, h: 1, target: "u" }])).toEqual([]);
    expect(collectTransferables("pageSize", { widthPx: 612, heightPx: 792 })).toEqual([]);
    expect(collectTransferables("imageRefs", null)).toEqual([]);
    // RED: an over-broad branch would ship an unrelated buffer (regressing the display-list path).
  });

  it("T4 transfer-faithful round-trip: bytes/meta preserved, sources neutered, no cross-neuter", () => {
    const a = ref("rId1", "image/png", new Uint8Array([1, 2, 3, 4]));
    const b = ref("rId2", "image/jpeg", new Uint8Array([7, 8]));
    const value = [a, b];
    const received = structuredClone(value, { transfer: collectTransferables("imageRefs", value) }) as Ref[];
    const r0 = received[0]!;
    const r1 = received[1]!;

    // meta + bytes contents preserved on the receiving side
    expect(r0.relId).toBe("rId1");
    expect(r0.mime).toBe("image/png");
    expect(Array.from(r0.bytes)).toEqual([1, 2, 3, 4]);
    expect(r1.relId).toBe("rId2");
    expect(Array.from(r1.bytes)).toEqual([7, 8]);

    // REAL transfer, not a copy — both source buffers are now neutered (0-length).
    expect(a.bytes.byteLength).toBe(0);
    expect(b.bytes.byteLength).toBe(0);
    // …and the receiving buffers are intact + independent (no cross-neuter).
    expect(r0.bytes.byteLength).toBe(4);
    expect(r1.bytes.byteLength).toBe(2);
    expect(r0.bytes.buffer).not.toBe(r1.bytes.buffer);
  });

  it("T5 repeated calls on FRESH backing buffers: no DataCloneError, correct bytes twice", () => {
    const mint = (): Ref[] => [ref("rId1", "image/png", new Uint8Array([1, 2])), ref("rId2", "image/png", new Uint8Array([3]))];
    for (let i = 0; i < 2; i++) {
      const value = mint();
      const received = structuredClone(value, { transfer: collectTransferables("imageRefs", value) }) as Ref[];
      expect(Array.from(received[0]!.bytes)).toEqual([1, 2]);
      expect(Array.from(received[1]!.bytes)).toEqual([3]);
    }
  });

  it("skips odd/scalar entries in the imageRefs array (defensive instanceof guards)", () => {
    const good = ref("rId1", "image/png", new Uint8Array([1]));
    // a null entry, a scalar, and an entry without a Uint8Array bytes field
    const out = collectTransferables("imageRefs", [null, 5, { relId: "x", mime: "y", bytes: "nope" }, good]);
    expect(out).toEqual([good.bytes.buffer]);
  });
});
