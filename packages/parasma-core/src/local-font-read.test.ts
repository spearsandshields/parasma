import { describe, it, expect, vi } from "vitest";
import { readLocalFontCached } from "./local-font-provider";

/**
 * Cache fake with header round-trip: `put` receives a real `Response` (the
 * implementation constructs one), so `arrayBuffer()` + `headers` behave
 * natively; `match` hands back the stored pair the way the Cache API would.
 */
function fakeCache() {
  const store = new Map<string, { buf: ArrayBuffer; headers: Headers }>();
  return {
    store,
    match: vi.fn(async (k: string) => {
      const e = store.get(k);
      return e ? { arrayBuffer: async () => e.buf, headers: e.headers } : undefined;
    }),
    put: vi.fn(async (k: string, resp: Response) => {
      store.set(k, { buf: await resp.arrayBuffer(), headers: resp.headers });
    }),
    delete: vi.fn(async (k: string) => store.delete(k)),
  };
}

const result = (byte: number, family: string, ps = `${family}-PS`) => ({
  bytes: new Uint8Array([byte]).buffer,
  postscriptName: ps,
  family,
  matchTier: 1 as const, // the ladder tier rides the result + cache entry
});

describe("readLocalFontCached", () => {
  it("reads via provider on miss, then serves from cache on hit", async () => {
    const cache = fakeCache();
    const provider = { permissionState: async () => "granted" as const, read: vi.fn(async () => result(9, "Verdana")) };
    const a = await readLocalFontCached(cache as any, provider, ["Verdana"], "400", "normal");
    expect(new Uint8Array(a!.bytes)).toEqual(new Uint8Array([9]));
    expect(cache.put).toHaveBeenCalledTimes(1);

    const b = await readLocalFontCached(cache as any, provider, ["Verdana"], "400", "normal");
    expect(new Uint8Array(b!.bytes)).toEqual(new Uint8Array([9]));
    expect(provider.read).toHaveBeenCalledTimes(1); // not called again — cache hit
  });

  it("returns null when the provider returns null", async () => {
    const cache = fakeCache();
    const provider = { permissionState: async () => "granted" as const, read: vi.fn(async () => null) };
    expect(await readLocalFontCached(cache as any, provider, ["Verdana"], "400", "normal")).toBeNull();
  });

  it("keys the cache by face weight + style (bold ≠ regular)", async () => {
    const cache = fakeCache();
    const provider = {
      permissionState: async () => "granted" as const,
      read: vi.fn(async (_c: string[], w: string) =>
        result(parseInt(w, 10) >= 600 ? 7 : 3, "Verdana")),
    };
    const reg = await readLocalFontCached(cache as any, provider, ["Verdana"], "400", "normal");
    const bold = await readLocalFontCached(cache as any, provider, ["Verdana"], "700", "normal");
    expect(new Uint8Array(reg!.bytes)).toEqual(new Uint8Array([3]));
    expect(new Uint8Array(bold!.bytes)).toEqual(new Uint8Array([7])); // distinct key, not the cached regular
    expect(provider.read).toHaveBeenCalledTimes(2);
  });

  // RED (old code): the key was the WHOLE candidate list, so a different list
  // that resolves to the same installed face missed the cache (provider read
  // twice) and the entry never said WHICH family matched.
  it("keys by the MATCHED family, not the candidate list", async () => {
    const cache = fakeCache();
    const provider = { permissionState: async () => "granted" as const, read: vi.fn(async () => result(5, "Verdana")) };

    const a = await readLocalFontCached(
      cache as any, provider, ["DejaVu Sans", "Verdana"], "400", "normal");
    expect(a!.family).toBe("Verdana");

    // Different candidate list, same matched face → must HIT the per-face entry.
    const b = await readLocalFontCached(cache as any, provider, ["Verdana"], "400", "normal");
    expect(provider.read).toHaveBeenCalledTimes(1);
    expect(new Uint8Array(b!.bytes)).toEqual(new Uint8Array([5]));
    // Metadata round-trip: matched family + postscriptName come back from the
    // cached entry's headers, not just from a live provider read.
    expect(b!.family).toBe("Verdana");
    expect(b!.postscriptName).toBe("Verdana-PS");
  });

  it("probes candidates in order and hits an earlier candidate's cached face", async () => {
    const cache = fakeCache();
    const provider = { permissionState: async () => "granted" as const, read: vi.fn(async () => result(6, "Segoe UI", "SegoeUI")) };

    await readLocalFontCached(cache as any, provider, ["Segoe UI"], "400", "normal");
    // "Segoe UI" appears among the candidates → served from its per-face entry.
    const hit = await readLocalFontCached(
      cache as any, provider, ["Not Installed", "Segoe UI"], "400", "normal");
    expect(provider.read).toHaveBeenCalledTimes(1);
    expect(hit!.family).toBe("Segoe UI");
    expect(hit!.postscriptName).toBe("SegoeUI");
  });
});

describe("readLocalFontCached — revocation and tier semantics", () => {
  const result4 = (byte: number, family: string, tier: 1 | "dev-route") => ({
    bytes: new Uint8Array([byte]).buffer,
    postscriptName: `${family}-PS`,
    family,
    matchTier: tier,
  });

  it("revocation-while-closed: Font-Access entries are deleted+skipped when the grant is gone", async () => {
    const cache = fakeCache();
    // Written under a granted era (numeric tier → Font-Access-derived).
    const writeEra = {
      permissionState: async () => "granted" as const,
      read: vi.fn(async () => result4(7, "Verdana", 1)),
    };
    await readLocalFontCached(cache as any, writeEra, ["Verdana"], "400", "normal");
    expect(cache.put).toHaveBeenCalledTimes(1);

    // Next mount: permission revoked while the page was closed.
    const revoked = {
      permissionState: async () => "prompt" as const,
      read: vi.fn(async () => null),
    };
    const out = await readLocalFontCached(cache as any, revoked, ["Verdana"], "400", "normal");
    expect(out).toBeNull(); // NOT served from the stale grant-era entry
    expect(cache.delete).toHaveBeenCalledTimes(1); // and flushed, fail-closed
  });

  it("dev-route entries serve without a grant (no permission concept)", async () => {
    const cache = fakeCache();
    const dev = {
      permissionState: async () => "prompt" as const,
      read: vi.fn(async () => result4(8, "Times New Roman", "dev-route")),
    };
    await readLocalFontCached(cache as any, dev, ["Times New Roman"], "400", "normal");
    const hit = await readLocalFontCached(cache as any, dev, ["Times New Roman"], "400", "normal");
    expect(new Uint8Array(hit!.bytes)).toEqual(new Uint8Array([8]));
    expect(dev.read).toHaveBeenCalledTimes(1); // second call was a cache hit
  });

  it("tier is WRITER-relative: a hit via a later (curated) candidate reports 'curated', not the stored numeric tier", async () => {
    const cache = fakeCache();
    const granted = {
      permissionState: async () => "granted" as const,
      read: vi.fn(async () => result4(9, "Verdana", 1)),
    };
    // Doc A declares "Verdana": writes the verdana entry with tier 1.
    const a = await readLocalFontCached(cache as any, granted, ["Verdana"], "400", "normal");
    expect(a!.matchTier).toBe(1);
    // Doc B declares "DejaVu Sans" with Verdana as the CURATED candidate:
    // the same entry serves, but tier 1 would misreport an exact name match.
    const b = await readLocalFontCached(
      cache as any,
      granted,
      ["DejaVu Sans", "Verdana"],
      "400",
      "normal",
    );
    expect(new Uint8Array(b!.bytes)).toEqual(new Uint8Array([9]));
    expect(b!.matchTier).toBe("curated");
  });
});
