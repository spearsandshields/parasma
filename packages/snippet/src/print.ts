/**
 * Print pipeline (word/pptx; spreadsheets rejected upstream — Excel print
 * ranges are deferred work).
 *
 * MEMORY MODEL (the whole point): pages are
 * rasterized SEQUENTIALLY through ONE working canvas whose backing store is
 * released after each `toBlob`, so live uncompressed memory is ~one page
 * regardless of document length. Rasters live browser-side (canvas/Blob) —
 * wasm's share per page is the transient display list, nothing
 * accumulates in linear memory. Compressed pages are Blobs: browser-managed,
 * disk-spillable.
 *
 * PRESENTATION: a hidden same-origin iframe owns the print document, so the
 * consumer's app needs NO `@media print` rules and their chrome never fights
 * the spool — the isolation an embeddable library owes its host page.
 *
 * The pipeline is expressed over injected PORTS so orchestration (ranges,
 * sequencing, supersede, progress) unit-tests in node; the DOM
 * implementations live in {@link domPrintPorts}.
 */

export interface PrintOptions {
  /** 1-based inclusive page range; defaults to the whole document. Ranges are
   *  the pressure valve for huge documents (browser print spoolers struggle
   *  near 1k pages no matter how the content was prepared). */
  from?: number;
  to?: number;
  /** Raster scale over CSS px. Default 2 (≈192dpi) — crisp print at a quarter
   *  of 300dpi's bytes; pass 3–4 for photo-grade output. */
  scale?: number;
  /** "png" (default: lossless text, and text dominates print) or "jpeg" for
   *  photo-heavy documents. */
  format?: "png" | "jpeg";
  /** JPEG quality (0–1, default 0.92); ignored for png. */
  quality?: number;
}

export interface PrintPorts {
  pageCount(): number;
  pageSize(idx: number): Promise<{ widthPx: number; heightPx: number }>;
  renderPage(idx: number, canvas: HTMLCanvasElement): Promise<void>;
  createCanvas(): HTMLCanvasElement;
  rasterize(canvas: HTMLCanvasElement, type: string, quality?: number): Promise<Blob>;
  present(pages: Blob[], firstSize: { widthPx: number; heightPx: number }): Promise<void>;
  onProgress(done: number, total: number): void;
}

export interface PrintPipeline {
  print(opts?: PrintOptions): Promise<void>;
  /** Aborts the in-flight run (also implied by a new print() — supersede). */
  cancel(): void;
}

export function createPrintPipeline(ports: PrintPorts): PrintPipeline {
  let seq = 0;

  async function print(opts: PrintOptions = {}): Promise<void> {
    const mySeq = ++seq;
    const total = ports.pageCount();
    if (total <= 0) throw new Error("print: no pages to print");
    const from = Math.max(1, Math.min(opts.from ?? 1, total));
    const to = Math.max(from, Math.min(opts.to ?? total, total));
    const scale = opts.scale ?? 2;
    const type = opts.format === "jpeg" ? "image/jpeg" : "image/png";
    const quality = opts.format === "jpeg" ? (opts.quality ?? 0.92) : undefined;

    const count = to - from + 1;
    const pages: Blob[] = [];
    let firstSize: { widthPx: number; heightPx: number } | null = null;
    ports.onProgress(0, count);
    for (let idx = from - 1; idx < to; idx++) {
      const size = await ports.pageSize(idx);
      firstSize ??= size;
      const canvas = ports.createCanvas();
      canvas.width = Math.max(1, Math.round(size.widthPx * scale));
      canvas.height = Math.max(1, Math.round(size.heightPx * scale));
      const ctx = canvas.getContext("2d", { alpha: false });
      if (ctx) {
        ctx.fillStyle = "#ffffff";
        ctx.fillRect(0, 0, canvas.width, canvas.height);
        // renderPage draws in layout px; pre-scale so the full page fills the
        // print-resolution backing (the base-layer adapter's exact pattern).
        ctx.scale(canvas.width / size.widthPx, canvas.height / size.heightPx);
      }
      await ports.renderPage(idx, canvas);
      if (seq !== mySeq) return; // superseded/cancelled: no present, no spool
      pages.push(await ports.rasterize(canvas, type, quality));
      // Release the raster NOW — this line is the memory model.
      canvas.width = 0;
      canvas.height = 0;
      if (seq !== mySeq) return;
      ports.onProgress(idx - (from - 1) + 1, count);
    }
    await ports.present(pages, firstSize!);
  }

  return { print, cancel: () => void seq++ };
}

/** The real DOM ports (browser only). `win`/`doc` come from the RootScope so
 *  shadow/iframe embeds print from the correct realm. */
export function domPrintPorts(
  win: Pick<Window, "setTimeout">,
  doc: Document,
): Pick<PrintPorts, "createCanvas" | "rasterize" | "present"> {
  return {
    createCanvas: () => doc.createElement("canvas"),
    rasterize: (canvas, type, quality) =>
      new Promise<Blob>((res, rej) =>
        canvas.toBlob(
          (b) => (b ? res(b) : rej(new Error("print: rasterization produced no blob"))),
          type,
          quality,
        ),
      ),
    present: async (pages, firstSize) => {
      const urls = pages.map((b) => URL.createObjectURL(b));
      const iframe = doc.createElement("iframe");
      iframe.setAttribute("aria-hidden", "true");
      iframe.style.cssText =
        "position:fixed;right:0;bottom:0;width:1px;height:1px;border:0;visibility:hidden;";
      doc.body.appendChild(iframe);
      const cleanup = (): void => {
        iframe.remove();
        for (const u of urls) URL.revokeObjectURL(u);
      };
      try {
        const idoc = iframe.contentDocument!;
        idoc.open();
        // @page sized to the first page (mixed page sizes print scaled-to-fit
        // — a recorded floor, rare in practice). break-after paginates.
        // Write the FULL skeleton and close before touching `body`: a write of
        // head-only content leaves `idoc.body` null until the parser sees body
        // content (measured live — appendChild on null).
        idoc.write(
          `<!doctype html><style>@page{size:${firstSize.widthPx}px ${firstSize.heightPx}px;margin:0}` +
            `html,body{margin:0}img{display:block;width:100%;break-after:page}` +
            `img:last-child{break-after:auto}</style><body></body>`,
        );
        idoc.close();
        for (const u of urls) {
          const img = idoc.createElement("img");
          img.src = u;
          idoc.body.appendChild(img);
        }
        await Promise.all(
          [...idoc.images].map((img) => img.decode().catch(() => {})),
        );
        const iwin = iframe.contentWindow!;
        iwin.addEventListener("afterprint", cleanup, { once: true });
        // Fallback: environments that never fire afterprint (headless, some
        // print-to-PDF flows). The blobs stay alive until then — bounded.
        win.setTimeout(cleanup, 60_000);
        iwin.focus();
        iwin.print();
      } catch (e) {
        cleanup();
        throw e;
      }
    },
  };
}
