/**
 * The stall watchdog: a worker that goes silent mid-request is terminated and
 * the transport dies, while a worker that is slow but keeps streaming progress
 * is left alone. A fake Worker is injected; time is faked.
 */
import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import { WorkerTransport } from "./transport";

/** Never replies on its own; the test drives every message. */
class SilentWorker {
  onmessage: ((e: MessageEvent) => void) | null = null;
  onerror: ((e: unknown) => void) | null = null;
  terminated = false;
  lastId = 0;
  postMessage(msg: { id: number }): void {
    this.lastId = msg.id;
  }
  terminate(): void {
    this.terminated = true;
  }
  say(data: unknown): void {
    this.onmessage?.({ data } as MessageEvent);
  }
  progress(): void {
    this.say({ kind: "evt", event: "pageReady", gen: 1, payload: 0 });
  }
  reply(value: unknown): void {
    this.say({ kind: "res", id: this.lastId, ok: true, value });
  }
}

function setup(stallTimeoutMs?: number) {
  const worker = new SilentWorker();
  const transport = new WorkerTransport(worker as unknown as Worker, { stallTimeoutMs });
  const deaths: Error[] = [];
  transport.onDead((e) => deaths.push(e));
  return { worker, transport, deaths };
}

beforeEach(() => {
  vi.useFakeTimers();
});
afterEach(() => {
  vi.useRealTimers();
});

describe("WorkerTransport stall watchdog", () => {
  it("terminates a worker that says nothing for the stall timeout and rejects the request", async () => {
    const { worker, transport, deaths } = setup(1000);
    const req = transport.request("loadDocx", []);
    const settled = expect(req).rejects.toThrow(/worker stalled/);
    await vi.advanceTimersByTimeAsync(999);
    expect(worker.terminated).toBe(false);
    await vi.advanceTimersByTimeAsync(300);
    await settled;
    expect(worker.terminated).toBe(true);
    expect(deaths).toHaveLength(1);
    // Dead means dead: a new request fails immediately instead of hanging.
    await expect(transport.request("pageCount", [])).rejects.toThrow(/unavailable/);
  });

  it("leaves a slow worker alone while it keeps streaming progress", async () => {
    const { worker, transport, deaths } = setup(1000);
    const req = transport.request("runLayout", []);
    for (let i = 0; i < 10; i++) {
      await vi.advanceTimersByTimeAsync(800);
      worker.progress();
    }
    worker.reply(42);
    await expect(req).resolves.toBe(42);
    expect(worker.terminated).toBe(false);
    expect(deaths).toHaveLength(0);
  });

  it("does not count idle time before a request against the worker", async () => {
    const { worker, transport } = setup(1000);
    const first = transport.request("a", []);
    worker.reply(1);
    await first;
    await vi.advanceTimersByTimeAsync(10_000); // idle, no request outstanding
    const req = transport.request("b", []);
    await vi.advanceTimersByTimeAsync(500);
    worker.reply(2);
    await expect(req).resolves.toBe(2);
    expect(worker.terminated).toBe(false);
  });

  it("does not blame the worker for time the main thread itself was frozen", async () => {
    const { worker, transport } = setup(1000);
    const req = transport.request("loadDocx", []);
    await vi.advanceTimersByTimeAsync(250);
    // The tab was frozen for a minute: the clock jumps, no timer ran.
    vi.setSystemTime(Date.now() + 60_000);
    await vi.advanceTimersByTimeAsync(250);
    expect(worker.terminated).toBe(false);
    worker.reply("ok");
    await expect(req).resolves.toBe("ok");
  });

  it("stallTimeoutMs 0 disables the watchdog", async () => {
    const { worker, transport } = setup(0);
    const req = transport.request("loadDocx", []);
    await vi.advanceTimersByTimeAsync(10 * 60_000);
    expect(worker.terminated).toBe(false);
    worker.reply("late");
    await expect(req).resolves.toBe("late");
  });

  it("defaults to 30 seconds", async () => {
    const { worker, transport } = setup();
    const req = transport.request("loadDocx", []);
    const settled = expect(req).rejects.toThrow(/30000 ms/);
    await vi.advanceTimersByTimeAsync(29_000);
    expect(worker.terminated).toBe(false);
    await vi.advanceTimersByTimeAsync(2_000);
    await settled;
    expect(worker.terminated).toBe(true);
  });
});
