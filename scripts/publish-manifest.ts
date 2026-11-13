#!/usr/bin/env bun
/**
 * Apply / restore `publishConfig` field overrides around packing.
 *
 * bun (this repo's publish tool) does NOT yet apply `publishConfig` field
 * overrides the way pnpm/yarn do — it's an open feature request
 * (oven-sh/bun#19205). So we do explicitly what pnpm does implicitly: on the
 * `prepack` lifecycle hook (which bun/npm/pnpm/yarn all run), splice the
 * package's `publishConfig.{main,module,types,exports,bin,browser}` into the
 * top level so the packed tarball points at `dist/*`; on `postpack`, restore
 * the original `src/*.ts` manifest so `bun run dev` keeps consuming source.
 *
 * `publishConfig` stays the single source of truth for the dist mapping. When
 * bun implements #19205, the swap goes away; the licence copy below stays.
 *
 * The same hooks copy the root LICENSE, NOTICE and THIRD-PARTY-NOTICES.md into
 * the package so every tarball carries them (Apache-2.0 §4 asks for the
 * licence and the NOTICE file with every redistribution), and remove the
 * copies afterwards. The copies are gitignored; the root files are the only
 * source.
 *
 * Usage (cwd = the package being packed):
 *   bun ../../scripts/publish-manifest.ts apply     # prepack
 *   bun ../../scripts/publish-manifest.ts restore   # postpack
 */
import { readFileSync, writeFileSync, existsSync, rmSync, copyFileSync } from "node:fs";

const mode = process.argv[2];
const PKG = "package.json";
// Exact-bytes backup of the pre-swap manifest. Gitignored; a lingering copy
// means a pack aborted between prepack and postpack (see the guard below).
const BACKUP = "package.json.prepack-orig";
// Only these fields are meaningful as publish-time entry overrides.
const OVERRIDE_KEYS = ["main", "module", "types", "exports", "bin", "browser"] as const;
// Root files every tarball must carry; copied in on apply, removed on restore.
const ROOT_FILES = ["LICENSE", "NOTICE", "THIRD-PARTY-NOTICES.md"] as const;
const ROOT = "../..";

function fail(msg: string): never {
  console.error(`[publish-manifest] ${msg}`);
  process.exit(1);
}

if (mode === "apply") {
  if (existsSync(BACKUP)) {
    fail(
      `stale ${BACKUP} found — a previous pack aborted mid-swap. Restore with ` +
        `\`mv ${BACKUP} ${PKG}\` then retry.`,
    );
  }
  for (const f of ROOT_FILES) copyFileSync(`${ROOT}/${f}`, f);
  const raw = readFileSync(PKG, "utf8");
  const pkg = JSON.parse(raw);
  const pc = pkg.publishConfig;
  if (!pc || typeof pc !== "object") process.exit(0); // nothing to swap
  writeFileSync(BACKUP, raw); // preserve the exact original for restore
  for (const key of OVERRIDE_KEYS) {
    if (key in pc) pkg[key] = pc[key];
  }
  writeFileSync(PKG, `${JSON.stringify(pkg, null, 2)}\n`);
  console.log(`[publish-manifest] ${pkg.name}: applied publishConfig → dist paths`);
} else if (mode === "restore") {
  for (const f of ROOT_FILES) rmSync(f, { force: true });
  if (existsSync(BACKUP)) {
    const orig = readFileSync(BACKUP, "utf8");
    writeFileSync(PKG, orig);
    rmSync(BACKUP);
    let name = "";
    try {
      name = `${JSON.parse(orig).name}: `;
    } catch {
      /* keep unattributed if the backup is somehow unparseable */
    }
    console.log(`[publish-manifest] ${name}restored src paths`);
  }
} else {
  fail("usage: publish-manifest.ts apply|restore");
}
