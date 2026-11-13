/**
 * DEV-SERVER ONLY (apply: "serve") — serves the machine's OS-installed fonts at
 * `GET /__local-fonts/<family>?w=<weight>&i=<0|1>`, so the local-font path gets
 * Word-fidelity bytes WITHOUT the Font Access gesture (headless browser
 * automation + local dev). Reuses scripts/debug-fonts.py --system for the cross-platform OS lookup,
 * caching faces in the gitignored .debug-fonts/.
 *
 * Never part of any built bundle (it's a config-time plugin, and apply:"serve"
 * skips it during `vite build`). Serves only the user's own fonts on localhost —
 * same legitimacy as the Font Access read; not redistribution.
 */
import type { Plugin } from "vite";
import { execFile } from "node:child_process";
import { promisify } from "node:util";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const execFileP = promisify(execFile);
const REPO = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../..");
const DEBUG_FONTS = path.join(REPO, ".debug-fonts");
const SCRIPT = path.join(REPO, "scripts", "debug-fonts.py");
// Family names only — blocks path traversal AND shell/argv injection.
const FAMILY_RE = /^[A-Za-z0-9 ]{1,64}$/;

function styleFor(weight: number, italic: boolean): string {
  const bold = weight >= 600;
  if (bold && italic) return "boldItalic";
  if (bold) return "bold";
  if (italic) return "italic";
  return "regular";
}

/** Locate a populated face by the script's `<FamilyNoSpaces>-<style>.{ttf,otf}` naming. */
function findFace(family: string, style: string): string | null {
  const base = family.replace(/ /g, "");
  for (const ext of [".ttf", ".otf"]) {
    const p = path.join(DEBUG_FONTS, `${base}-${style}${ext}`);
    if (fs.existsSync(p) && fs.statSync(p).size > 0) return p;
  }
  return null;
}

export function localFontsDevPlugin(): Plugin {
  // Per-family populate dedup: the 4 variant requests (R/B/I/BI) arrive together
  // on a cold family; share ONE debug-fonts.py run rather than spawning four.
  const inflight = new Map<string, Promise<void>>();

  return {
    name: "parasma-local-fonts-dev",
    apply: "serve", // dev server ONLY — never runs during `vite build`
    configureServer(server) {
      server.middlewares.use("/__local-fonts/", async (req, res) => {
        try {
          const url = new URL(req.url ?? "", "http://localhost");
          const family = decodeURIComponent(url.pathname.replace(/^\//, "").split("/")[0] ?? "");
          if (!FAMILY_RE.test(family)) {
            res.statusCode = 400;
            return res.end("bad family");
          }
          const weight = parseInt(url.searchParams.get("w") ?? "400", 10) || 400;
          const italic = url.searchParams.get("i") === "1";
          const style = styleFor(weight, italic);

          let face = findFace(family, style);
          if (!face) {
            let job = inflight.get(family);
            if (!job) {
              // Populate from the OS-installed font (reuses the script's --system
              // lookup). argv array — never a shell string — and family is regex-
              // validated above, so no injection.
              job = execFileP("python3", [SCRIPT, "--system", "--all-styles", "--families", family], {
                cwd: REPO,
                timeout: 30000,
              })
                .then(() => undefined)
                .catch((e) => {
                  server.config.logger.warn(`[local-fonts] populate failed for "${family}": ${e}`);
                })
                .finally(() => inflight.delete(family));
              inflight.set(family, job);
            }
            await job;
            face = findFace(family, style);
          }
          if (!face) {
            res.statusCode = 404;
            return res.end("not installed");
          }

          res.setHeader("content-type", face.endsWith(".otf") ? "font/otf" : "font/ttf");
          res.setHeader("cache-control", "no-store");
          res.end(fs.readFileSync(face));
        } catch (e) {
          res.statusCode = 500;
          res.end(String(e));
        }
      });
    },
  };
}
