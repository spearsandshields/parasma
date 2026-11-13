#!/usr/bin/env bun
/**
 * Populates the demo's fixtures directory from two sources:
 *   1. OOXML test fixtures from the parser fork (spearsandshields/parasma-ooxml)
 *   2. Generated test fixtures (scripts/generate-test-fixtures.ts)
 *
 * Rebuilds manifest.json from scratch each run.
 *
 * Usage:
 *   bun run scripts/fetch-fixtures.ts [path-to-parser-fork-checkout]
 *
 * Without an argument, uses a sibling checkout at ../pterror-fork or ../pterror
 * when one exists, and otherwise fetches the fork's fixtures/ directory alone
 * into a temporary sparse clone.
 */

import { join, relative, dirname } from "path";
import {
  existsSync,
  mkdirSync,
  cpSync,
  readdirSync,
  statSync,
  writeFileSync,
} from "fs";
import { execSync } from "child_process";
import { mkdtempSync, rmSync } from "fs";
import { tmpdir } from "os";

const REPO_ROOT = join(import.meta.dir, "..");
const FORK_URL = "https://github.com/spearsandshields/parasma-ooxml";
const DEST = join(REPO_ROOT, "fixtures");

let tempClone: string | null = null;
function locateFork(): string {
  if (process.argv[2]) return process.argv[2];
  for (const sibling of ["../pterror-fork", "../pterror"]) {
    const p = join(REPO_ROOT, sibling);
    if (existsSync(join(p, "fixtures/wml"))) return p;
  }
  tempClone = mkdtempSync(join(tmpdir(), "parasma-ooxml-fixtures-"));
  console.log(`no local parser-fork checkout; fetching fixtures/ from ${FORK_URL}`);
  execSync(`git clone --quiet --depth 1 --filter=blob:none --sparse ${FORK_URL} "${tempClone}"`, {
    stdio: "inherit",
  });
  execSync(`git -C "${tempClone}" sparse-checkout set fixtures`, { stdio: "inherit" });
  return tempClone;
}
const FORK = locateFork();
const SRC = join(FORK, "fixtures/wml");

// ─── Step 1: Copy the parser fork's fixtures ───────────────────────────────

if (existsSync(SRC)) {
  let count = 0;
  function walkAndCopy(dir: string): void {
    for (const entry of readdirSync(dir, { withFileTypes: true })) {
      const full = join(dir, entry.name);
      if (entry.isDirectory()) {
        walkAndCopy(full);
      } else if (entry.name.endsWith(".docx")) {
        const rel = relative(SRC, full);
        const dest = join(DEST, rel);
        mkdirSync(dirname(dest), { recursive: true });
        cpSync(full, dest);
        count++;
      }
    }
  }
  walkAndCopy(SRC);
  console.log(`copied ${count} parser-fork fixtures`);
} else {
  console.log(
    `warning: parser-fork fixtures not found at ${SRC} — skipping\n` +
      `         run: bun run scripts/fetch-fixtures.ts [path-to-parser-fork-checkout]`,
  );
}
if (tempClone) rmSync(tempClone, { recursive: true, force: true });

// ─── Step 2: Generate our test fixtures ────────────────────────────────────

await import("./generate-test-fixtures.ts");

// ─── Step 3: Rebuild manifest.json ─────────────────────────────────────────

interface ManifestEntry {
  path: string;
  category: string;
  name: string;
}

const entries: ManifestEntry[] = [];

function scanFixtures(dir: string): void {
  if (!existsSync(dir)) return;
  for (const entry of readdirSync(dir, { withFileTypes: true })) {
    const full = join(dir, entry.name);
    // statSync, not entry.isDirectory(): a subdirectory may be a symlink to a
    // directory of local documents, and a Dirent reports a symlink as
    // not-a-directory.
    if (statSync(full).isDirectory()) {
      scanFixtures(full);
    } else if (/\.(docx|xlsx|csv|pptx)$/.test(entry.name)) {
      const rel = relative(DEST, full);
      const category = rel.split("/")[0];
      const name = entry.name.replace(/\.(docx|xlsx|csv|pptx)$/, "");
      entries.push({ path: `fixtures/${rel}`, category, name });
    }
  }
}

scanFixtures(DEST);
entries.sort((a, b) => a.path.localeCompare(b.path));

writeFileSync(join(DEST, "manifest.json"), JSON.stringify(entries, null, 2) + "\n");
console.log(`manifest.json: ${entries.length} entries`);
