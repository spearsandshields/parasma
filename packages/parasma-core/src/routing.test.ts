/**
 * worker:"auto" sniff-and-route.
 *
 * `create({worker:"auto"})` constructs the Viewer IN-PROCESS (never eager-boots a
 * worker), then on EACH load() ensures the worker engine: ALL sniffed formats —
 * docx, pptx, AND xlsx/csv (grid + overlays record off-thread) —
 * ATTACH a worker so parse+layout+paint run off-thread. The route is IDEMPOTENT
 * (no-op when already correct), so a docx↔xlsx switch stays on the SAME worker (no
 * detach/re-attach). A worker-boot failure degrades SILENTLY to in-process
 * (batteries-included) — the doc still renders.
 *
 * We drive the REAL create() path (so the create() truthy-gate bug is exercised) by
 * mocking (a) the wasm module — a fake local `WasmViewer` recording the
 * in-process load entries into a shared log; and (b) `createWorkerTransport` — a
 * spy returning a MockRemoteTransport that routes RPCs to an in-process Backend
 * (records the WORKER-side load entries + dispose ordering). Observability: the
 * transport-spy call count = attach count; the backend log = worker-side loads;
 * the local log = in-process loads; the shared wire log = abort/dispose ordering.
 */
import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import type { Transport, TransportEvent } from "./transport.js";

// ── shared, hoisted state (reset per test) ──────────────────────────────────
const H = vi.hoisted(() => ({
  initSpy: vi.fn((..._a: unknown[]) => Promise.resolve({})),
  localLog: [] as string[], // the in-process #wasm's load entries
  createWorkerTransport: vi.fn(),
}));

// Mock the wasm module: a fake local `WasmViewer` recording its (in-process) load
// entries into H.localLog. Empty font/image lists keep #doLoad*'s async setup fast.
vi.mock("./wasm/parasma_wasm.js", () => {
  class Viewer {
    loadDocx(): void {
      H.localLog.push("local:loadDocx");
    }
    loadXlsx(): void {
      H.localLog.push("local:loadXlsx");
    }
    loadPptx(): void {
      H.localLog.push("local:loadPptx");
    }
    abortLayout(): void {
      H.localLog.push("local:abort");
    }
    runLayout(cb: unknown): Promise<number> {
      if (typeof cb === "function") (cb as (i: number) => void)(0);
      return Promise.resolve(1);
    }
    setNow(): void {}
    pageSize(): { widthPx: number; heightPx: number } {
      return { widthPx: 612, heightPx: 792 };
    }
    fontFamiliesUsed(): string[] {
      return [];
    }
    imageRefs(): unknown[] {
      return [];
    }
    clearImageBytes(): void {}
    free(): void {}
    debugLayout(): void {}
  }
  return { default: H.initSpy, Viewer, parserInfo: () => ({}) };
});

// Partial-mock transport.js: keep the REAL exports (WorkerSelector etc.), replace
// createWorkerTransport with the controllable spy.
vi.mock("./transport.js", async (orig) => {
  const actual = await orig<typeof import("./transport.js")>();
  return { ...actual, createWorkerTransport: H.createWorkerTransport };
});

import { Viewer } from "./index";

// ── the worker-side engine stand-in (dispatched by RPC method name) ─────────
class Backend {
  constructor(public log: string[]) {}
  #rec(m: string): void {
    this.log.push(m);
  }
  loadDocx(): void {
    this.#rec("loadDocx");
  }
  loadXlsx(): void {
    this.#rec("loadXlsx");
  }
  loadPptx(): void {
    this.#rec("loadPptx");
  }
  abortLayout(): void {
    this.#rec("abortLayout");
  }
  setNow(): void {}
  pageSize(): { widthPx: number; heightPx: number } {
    return { widthPx: 612, heightPx: 792 };
  }
  fontFamiliesUsed(): string[] {
    return [];
  }
  imageRefs(): unknown[] {
    return [];
  }
  runLayout(_gen: number): number {
    return 1;
  }
}

/** A Transport routing to an in-process Backend (structured-clone on both sides,
 *  like postMessage). Records dispose into the shared wire log for ordering. */
class MockRemoteTransport implements Transport {
  disposeCount = 0;
  #backend: Record<string, ((...a: unknown[]) => unknown) | undefined>;
  #log: string[];
  constructor(backend: object, log: string[]) {
    this.#backend = backend as Record<string, ((...a: unknown[]) => unknown) | undefined>;
    this.#log = log;
  }
  request(method: string, args: unknown[]): Promise<unknown> {
    const fn = this.#backend[method];
    if (typeof fn !== "function") return Promise.reject(new Error(`unknown worker method: ${method}`));
    const clonedArgs = args.map((a) => structuredClone(a));
    const value = fn.apply(this.#backend, clonedArgs);
    return Promise.resolve(structuredClone(value));
  }
  notify(method: string, args: unknown[]): void {
    const fn = this.#backend[method];
    if (typeof fn === "function") fn.apply(this.#backend, args.map((a) => structuredClone(a)));
  }
  onEvent(_h: (e: TransportEvent) => void): () => void {
    return () => {};
  }
  onDead(_h: (e: Error) => void): () => void {
    return () => {};
  }
  dispose(): void {
    this.disposeCount++;
    this.#log.push("dispose");
  }
}

// A stored (method 0) ZIP naming `partName` in its local-file header — the byte
// substring sniff classifies it without a zip lib (load-kind.test idiom).
function zipNaming(partName: string): Uint8Array {
  const name = new TextEncoder().encode(partName);
  const h = new Uint8Array(30 + name.length);
  h[0] = 0x50;
  h[1] = 0x4b;
  h[2] = 0x03;
  h[3] = 0x04;
  h[26] = name.length & 0xff;
  h[27] = (name.length >> 8) & 0xff;
  h.set(name, 30);
  return h;
}
const docx = (): Uint8Array => zipNaming("word/document.xml");
const xlsx = (): Uint8Array => zipNaming("xl/workbook.xml");
const pptx = (): Uint8Array => zipNaming("ppt/presentation.xml");

// The shared wire log threaded into every transport + backend the spy mints, so
// the switch tests can assert abort-before-dispose ordering.
let wireLog: string[];
let backend: Backend;
const transports: MockRemoteTransport[] = [];

function stubWorkerOk(): void {
  H.createWorkerTransport.mockImplementation(async () => {
    const t = new MockRemoteTransport(backend, wireLog);
    transports.push(t);
    return t;
  });
}
function stubWorkerFail(err = new Error("boot fail")): void {
  H.createWorkerTransport.mockImplementation(async () => {
    throw err;
  });
}
const attachCount = (): number => H.createWorkerTransport.mock.calls.length;
const disposeTotal = (): number => transports.reduce((n, t) => n + t.disposeCount, 0);

const realFetch = globalThis.fetch;
beforeEach(() => {
  H.localLog.length = 0;
  H.initSpy.mockClear();
  H.createWorkerTransport.mockReset();
  transports.length = 0;
  wireLog = [];
  backend = new Backend(wireLog);
  // Font fetches (none here — fontFamiliesUsed=[]) never fire; guard anyway.
  globalThis.fetch = vi.fn(() => Promise.reject(new Error("network disabled"))) as unknown as typeof fetch;
});
afterEach(() => {
  globalThis.fetch = realFetch;
  vi.restoreAllMocks();
});

describe("worker:\"auto\" sniff-and-route", () => {
  it("constructs IN-PROCESS — no eager worker boot (RED: the create() truthy gate)", async () => {
    stubWorkerOk();
    await Viewer.create({ worker: "auto" });
    // The construction did NOT boot a worker; the boot is deferred to load().
    expect(attachCount()).toBe(0);
  });

  it("docx under auto → ATTACH a worker (loadDocx routes off-thread, not local)", async () => {
    stubWorkerOk();
    const v = await Viewer.create({ worker: "auto" });
    const info = await v.load(docx());
    expect(info.documentKind).toBe("word");
    expect(attachCount()).toBe(1); // RED: without the route step, no attach
    expect(backend.log).toContain("loadDocx"); // ran in the WORKER
    expect(H.localLog).not.toContain("local:loadDocx"); // NOT in-process
  });

  it("xlsx under auto → ATTACH a worker (grid + overlays record off-thread, not local)", async () => {
    stubWorkerOk();
    const v = await Viewer.create({ worker: "auto" });
    const info = await v.load(xlsx());
    expect(info.documentKind).toBe("excel");
    expect(attachCount()).toBe(1); // RED before the overlay fix: xlsx used to stay in-process
    expect(backend.log).toContain("loadXlsx"); // ran in the WORKER
    expect(H.localLog).not.toContain("local:loadXlsx"); // NOT in-process
  });

  it("pptx under auto → ATTACH a worker (slides record off-thread, not local)", async () => {
    stubWorkerOk();
    const v = await Viewer.create({ worker: "auto" });
    const info = await v.load(pptx());
    expect(info.documentKind).toBe("ppt");
    expect(attachCount()).toBe(1); // RED before pptx routing: pptx used to stay in-process
    expect(backend.log).toContain("loadPptx"); // ran in the WORKER
    expect(H.localLog).not.toContain("local:loadPptx"); // NOT in-process
  });

  it("IDEMPOTENT: docx→docx attaches EXACTLY once (RED: a naive route re-attaches)", async () => {
    stubWorkerOk();
    const v = await Viewer.create({ worker: "auto" });
    await v.load(docx());
    await v.load(docx());
    expect(attachCount()).toBe(1); // no second attach
    expect(backend.log.filter((m) => m === "loadDocx")).toHaveLength(2); // both in the worker
  });

  it("IDEMPOTENT: xlsx→xlsx attaches EXACTLY once", async () => {
    stubWorkerOk();
    const v = await Viewer.create({ worker: "auto" });
    await v.load(xlsx());
    await v.load(xlsx());
    expect(attachCount()).toBe(1); // one worker attach, reused for the 2nd load
    expect(disposeTotal()).toBe(0);
    expect(backend.log.filter((m) => m === "loadXlsx")).toHaveLength(2); // both in the worker
  });

  it("SWITCH: docx→xlsx→docx stay on ONE worker (no detach); each load drains prior layout", async () => {
    stubWorkerOk();
    const v = await Viewer.create({ worker: "auto" });
    await v.load(docx()); // attach #1
    await v.load(xlsx()); // SAME worker (xlsx now records off-thread) — no detach
    await v.load(docx()); // still the SAME worker

    expect(attachCount()).toBe(1); // one worker for the whole session — never detaches
    expect(disposeTotal()).toBe(0); // no dispose under auto anymore
    expect(backend.log.filter((m) => m === "loadDocx")).toHaveLength(2);
    expect(backend.log).toContain("loadXlsx"); // xlsx ran in the WORKER too
    expect(H.localLog).not.toContain("local:loadDocx"); // nothing in-process
    expect(H.localLog).not.toContain("local:loadXlsx");
    // DRAIN: each load aborts the prior in-flight layout on the (same) engine.
    // (The abort-before-DISPOSE ordering that used to live here is retired — no
    // dispose runs under auto now; that ordering only fires on worker-death.)
    expect(wireLog).toContain("abortLayout");
  });

  it("FALLBACK: a worker-boot failure degrades SILENTLY to in-process; docx still renders", async () => {
    stubWorkerFail();
    const warn = vi.spyOn(console, "warn").mockImplementation(() => {});
    const v = await Viewer.create({ worker: "auto" });
    const info = await v.load(docx()); // must NOT reject
    expect(info.documentKind).toBe("word");
    expect(H.localLog).toContain("local:loadDocx"); // rendered in-process
    expect(disposeTotal()).toBe(0);
    expect(warn).toHaveBeenCalled(); // warned, did not throw
    warn.mockRestore();
  });

  it("GIVE-UP LATCH: after a boot failure, a later docx load does NOT re-attempt the boot", async () => {
    stubWorkerFail();
    const warn = vi.spyOn(console, "warn").mockImplementation(() => {});
    const v = await Viewer.create({ worker: "auto" });
    await v.load(docx()); // boot attempt #1 (fails → latched)
    await v.load(docx()); // must NOT re-attempt the doomed boot
    expect(attachCount()).toBe(1); // RED without the latch: a 2nd boot attempt = 2
    expect(H.localLog.filter((e) => e === "local:loadDocx").length).toBe(2); // both rendered in-process
    warn.mockRestore();
  });
});

describe("precedence + in-process default (worker !== \"auto\")", () => {
  it("worker:true still EAGER-boots at construction; auto route never runs", async () => {
    stubWorkerOk();
    const v = await Viewer.create({ worker: true });
    expect(attachCount()).toBe(1); // booted at construction (createWorkerBacked)
    // #autoRoute is false → loading docx then xlsx adds NO extra attach/detach.
    await v.load(docx());
    await v.load(xlsx());
    expect(attachCount()).toBe(1);
    expect(disposeTotal()).toBe(0);
    expect(backend.log).toContain("loadDocx");
    expect(backend.log).toContain("loadXlsx"); // stayed on the explicit worker
  });

  it("worker:false → in-process, no worker, no route", async () => {
    stubWorkerOk();
    const v = await Viewer.create({ worker: false });
    await v.load(docx());
    expect(attachCount()).toBe(0);
    expect(H.localLog).toContain("local:loadDocx");
  });

  it("worker absent → in-process (byte-identical default), no worker", async () => {
    stubWorkerOk();
    const v = await Viewer.create();
    await v.load(docx());
    await v.load(xlsx());
    expect(attachCount()).toBe(0);
    // Both loads ran in-process, in order (the docx→xlsx switch drains the prior
    // layout via the EXISTING inline abort — unchanged by auto).
    expect(H.localLog.filter((m) => m.startsWith("local:load"))).toEqual(["local:loadDocx", "local:loadXlsx"]);
  });
});
