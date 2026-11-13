import { describe, it, expect } from "vitest";
import { createPrintPipeline, type PrintPorts } from "./print";

/**
 * Orchestration only (node env, fake ports): ranges, sequential order, the
 * canvas-release memory model, exact progress, supersede/cancel. The DOM
 * ports (toBlob, iframe presentation) are thin platform calls exercised by
 * the real-browser smoke, not unit-tested here.
 */

function makePorts(pages = 3) {
  const calls: string[] = [];
  const canvases: Array<{ width: number; height: number }> = [];
  const progress: Array<[number, number]> = [];
  let presented: Blob[] | null = null;
  let parked: (() => void) | null = null;
  let armed = false;
  const ports: PrintPorts = {
    pageCount: () => pages,
    pageSize: async (idx) => {
      calls.push(`size:${idx}`);
      return { widthPx: 800 + idx, heightPx: 1000 };
    },
    renderPage: async (idx, canvas) => {
      calls.push(`render:${idx}@${canvas.width}x${canvas.height}`);
      if (armed) {
        armed = false;
        await new Promise<void>((r) => (parked = r));
      }
    },
    createCanvas: () => {
      const c = { width: 0, height: 0, getContext: () => null } as unknown as HTMLCanvasElement;
      canvases.push(c as unknown as { width: number; height: number });
      return c;
    },
    rasterize: async (_c, type) => {
      calls.push(`blob:${type}`);
      return new Blob(["x"]);
    },
    present: async (blobs) => {
      calls.push(`present:${blobs.length}`);
      presented = blobs;
    },
    onProgress: (d, t) => progress.push([d, t]),
  };
  return { ports, calls, canvases, progress, presented: () => presented,
    holdNextRender: () => { armed = true; }, release: () => parked?.() };
}

describe("print pipeline (orchestration)", () => {
  it("full-document default: sequential order, released canvases, exact progress, one present", async () => {
    const f = makePorts(3);
    await createPrintPipeline(f.ports).print();
    expect(f.calls).toEqual([
      "size:0", "render:0@1600x2000", "blob:image/png",
      "size:1", "render:1@1602x2000", "blob:image/png",
      "size:2", "render:2@1604x2000", "blob:image/png",
      "present:3",
    ]);
    // The memory model: every working canvas backing store released.
    for (const c of f.canvases) expect([c.width, c.height]).toEqual([0, 0]);
    expect(f.progress).toEqual([[0, 3], [1, 3], [2, 3], [3, 3]]);
    expect(f.presented()!.length).toBe(3);
  });

  it("range {from,to} clamps to the document and prints the inclusive slice", async () => {
    const f = makePorts(5);
    await createPrintPipeline(f.ports).print({ from: 2, to: 99, format: "jpeg" });
    expect(f.calls.filter((c) => c.startsWith("size:"))).toEqual([
      "size:1", "size:2", "size:3", "size:4",
    ]);
    expect(f.calls.filter((c) => c.startsWith("blob:"))).toEqual(
      Array(4).fill("blob:image/jpeg"),
    );
    expect(f.progress.at(-1)).toEqual([4, 4]);
  });

  it("supersede: a second print() aborts the first — no present, no further progress", async () => {
    const f = makePorts(3);
    const pipe = createPrintPipeline(f.ports);
    f.holdNextRender(); // park run #1 inside page 0's render
    const first = pipe.print();
    await Promise.resolve();
    const second = pipe.print({ to: 1 }); // supersedes
    f.release(); // run #1 resumes, sees itself superseded, bails
    await first;
    await second;
    // Run #1 presented nothing; run #2 presented its single page.
    expect(f.calls.filter((c) => c.startsWith("present:"))).toEqual(["present:1"]);
  });

  it("cancel() aborts without presenting; empty document rejects", async () => {
    const f = makePorts(2);
    const pipe = createPrintPipeline(f.ports);
    f.holdNextRender();
    const run = pipe.print();
    await Promise.resolve();
    pipe.cancel();
    f.release();
    await run;
    expect(f.calls.some((c) => c.startsWith("present:"))).toBe(false);

    const empty = makePorts(0);
    await expect(createPrintPipeline(empty.ports).print()).rejects.toThrow(/no pages/);
  });
});
