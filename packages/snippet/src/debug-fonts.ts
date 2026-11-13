/**
 * DEV-ONLY font override for layout investigation. When the page is opened with
 * `?debugfonts=<absolute .debug-fonts dir>`, this reads the manifest written by
 * `scripts/debug-fonts.py` and monkeypatches `window.fetch` so the CDN font URLs
 * that core's `buildFontPlan` requests are served from locally-downloaded REAL
 * faces (vite `/@fs`). Combined with the script's `--remap`, it lets the
 * debug-probe viewer render a document family with a different physical face —
 * font-substitution and line-height experiments without touching core.
 *
 * It never reaches production: it is only wired in under `dev`, no-ops without
 * the query param, and the fonts live in a gitignored `.debug-fonts/`.
 */
type Entry = { family: string; style: string; url: string; path: string };

export async function installDebugFonts(): Promise<void> {
  const dir = new URLSearchParams(location.search).get("debugfonts");
  if (!dir) return;
  try {
    // core's fetchWithCache serves font bytes from the Cache API without
    // hitting window.fetch on a hit — so the redirect below would be bypassed.
    // Evict it so the next load re-fetches through the patched fetch. The live
    // store is "parasma-fonts-v2" (per-face keys); the retired v1 name
    // is deleted too in case an old orphan still lingers in this profile.
    if (typeof caches !== "undefined") {
      await caches.delete("parasma-fonts-v2");
      await caches.delete("parasma-fonts");
    }
    const manifest: Entry[] = await (await fetch(`/@fs${dir}/manifest.json`)).json();
    const map = new Map(manifest.map((e) => [e.url, `/@fs${e.path}`]));
    const orig = window.fetch.bind(window);
    window.fetch = (input: RequestInfo | URL, init?: RequestInit) => {
      const url =
        typeof input === "string" ? input : input instanceof URL ? input.href : input.url;
      if (/\.(ttf|otf|woff2?)(\?|$)/i.test(url)) {
        const local = map.get(url);
        // eslint-disable-next-line no-console
        console.log(`[debugfonts] FONT FETCH ${url}  ${local ? "→ " + local : "(no match)"}`);
        if (local) return orig(local, init);
      }
      return orig(input, init);
    };
    // eslint-disable-next-line no-console
    console.log(`[debugfonts] active — ${manifest.length} faces redirected from ${dir}`);
  } catch (e) {
    // eslint-disable-next-line no-console
    console.error("[debugfonts] failed to install:", e);
  }
}
