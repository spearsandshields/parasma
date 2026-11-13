#!/usr/bin/env bun
/**
 * Populate the snippet demo's fixtures from the repo-root /fixtures corpus
 * (the already-fetched SAFE public set) — the FULL catalog so the demo, our dev
 * tool, can browse every fixture. Binaries are NEVER committed (demo/fixtures is
 * gitignored — same policy as the /fixtures corpus, see .gitignore +
 * scripts/fetch-fixtures.ts). Run automatically via the predev/prebuild hooks.
 *
 * Copies everything under /fixtures EXCEPT the `user/` subtree, which is a
 * local place for documents that are not redistributable. The top-level `user`
 * directory is skipped outright.
 *
 * The reduced manifest ships with the demo, so it excludes every fixtures/user/
 * entry as well: neither the files nor their names reach the demo.
 *
 * CONTENT DEDUP: the upstream catalog ships the SAME file content under several
 * category paths (e.g. text/arabic.docx == text/unicode/arabic.docx — identical
 * bytes), which surfaces as DOUBLED leaves in the explorer. We dedup BY CONTENT:
 * hash every safe non-user file, copy each unique content ONCE under its
 * canonical path (shortest path, alphabetical tie-break).
 *
 * NAME DEDUP: beyond content-dups, the catalog also ships the SAME name under
 * different category folders with DIFFERENT bytes (e.g. comment/comment-basic
 * vs comments/comment-basic) — visually a duplicate leaf. The reduced manifest
 * keeps each NAME exactly once (first occurrence = shortest/canonical path), so
 * the explorer shows no two leaves with the same name.
 *
 *   bun run scripts/copy-demo-fixtures.ts
 */
import {
  existsSync,
  mkdirSync,
  copyFileSync,
  readdirSync,
  readFileSync,
  writeFileSync,
  rmSync,
} from "fs";
import { createHash } from "crypto";
import { join, relative, dirname } from "path";

const PKG = join(import.meta.dir, "..");
const SRC = join(PKG, "../../fixtures");
const DEST = join(PKG, "demo/fixtures");
const MANIFEST_NAME = "manifest.json";

// No corpus (a clean checkout, CI): the demo still builds, with an empty
// catalog. The fetch is a developer step, not a build prerequisite.
if (!existsSync(SRC)) {
  console.warn(
    `[copy-demo-fixtures] no fixtures at ${SRC}; writing an empty catalog.\n` +
      `To browse fixtures in the demo, run: bun run scripts/fetch-fixtures.ts`,
  );
  rmSync(DEST, { recursive: true, force: true });
  mkdirSync(DEST, { recursive: true });
  writeFileSync(join(DEST, MANIFEST_NAME), "[]\n");
  process.exit(0);
}

// Start fresh so removed upstream fixtures don't linger.
rmSync(DEST, { recursive: true, force: true });
mkdirSync(DEST, { recursive: true });

// Pass 1: gather every SAFE non-user file (rel path is relative to SRC, so the
// manifest path is `fixtures/` + rel). The manifest itself is metadata, not a
// fixture — skip it here and write the reduced one at the end.
const files: string[] = []; // rel paths
let skippedUser = 0;
function gather(dir: string): void {
  for (const entry of readdirSync(dir, { withFileTypes: true })) {
    const full = join(dir, entry.name);
    const rel = relative(SRC, full);
    // Never copy the local user/ subtree.
    if (rel.split("/")[0] === "user") {
      skippedUser++;
      continue;
    }
    if (entry.isDirectory()) {
      gather(full);
      continue;
    }
    if (rel === MANIFEST_NAME) continue; // handled separately (reduced copy)
    files.push(rel);
  }
}
gather(SRC);

// Hash each file's content and pick the canonical rel path per content: the
// shortest path wins, ties broken alphabetically. This keeps the plain
// `text/arabic.docx` form and drops the longer `text/unicode/arabic.docx`.
function isMoreCanonical(candidate: string, current: string): boolean {
  if (candidate.length !== current.length) return candidate.length < current.length;
  return candidate < current;
}
const canonicalByHash = new Map<string, string>(); // hash -> canonical rel
const hashByRel = new Map<string, string>(); // rel -> hash
for (const rel of files) {
  const buf = readFileSync(join(SRC, rel));
  const hash = createHash("sha256").update(buf).digest("hex");
  hashByRel.set(rel, hash);
  const cur = canonicalByHash.get(hash);
  if (cur === undefined || isMoreCanonical(rel, cur)) {
    canonicalByHash.set(hash, rel);
  }
}

// Copy ONLY canonical files; count skipped duplicates.
const canonicalRels = new Set(canonicalByHash.values());
let copied = 0;
let skippedDup = 0;
for (const rel of files) {
  if (!canonicalRels.has(rel)) {
    skippedDup++;
    continue;
  }
  const dest = join(DEST, rel);
  mkdirSync(dirname(dest), { recursive: true });
  copyFileSync(join(SRC, rel), dest);
  copied++;
}

// Write the REDUCED manifest. The source manifest is the truth for entry
// metadata (category/name); keep an entry only when its path is the canonical
// path for its content. User entries are retained unchanged (they were never
// hashed/copied — the demo filters them out at render time anyway).
interface ManifestItem {
  path: string;
  category: string;
  name: string;
}
const srcManifestPath = join(SRC, MANIFEST_NAME);
let writtenEntries = 0;
if (existsSync(srcManifestPath)) {
  const srcManifest = JSON.parse(readFileSync(srcManifestPath, "utf8")) as ManifestItem[];
  const seenName = new Set<string>();
  const reduced = srcManifest.filter((m) => {
    // Drop user/ entries: the files aren't copied, so their names stay out of
    // the demo's manifest too.
    if (m.path.startsWith("fixtures/user/")) return false;
    const rel = m.path.replace(/^fixtures\//, "");
    const hash = hashByRel.get(rel);
    if (hash !== undefined && canonicalByHash.get(hash) !== rel) return false; // non-canonical content-dup
    // Dedup by CATEGORY+NAME so the explorer shows no two leaves with the same
    // name within a group (e.g. comment/comment-basic vs comments/comment-basic).
    // Cross-CATEGORY same names are DISTINCT fixtures — a bare-name dedup
    // silently dropped spreadsheet/chart because ppt/chart came first.
    // Keep the first occurrence — the source manifest is already ordered by
    // shortest/canonical path.
    const nameKey = `${m.category}/${m.name}`;
    if (seenName.has(nameKey)) return false;
    seenName.add(nameKey);
    return true;
  });
  writtenEntries = reduced.length;
  writeFileSync(join(DEST, MANIFEST_NAME), JSON.stringify(reduced, null, 2) + "\n");
}

console.log(
  `[copy-demo-fixtures] copied ${copied} unique-content files → demo/fixtures ` +
    `(gitignored); skipped ${skippedDup} duplicate-content files; ` +
    `skipped user/ paths: ${skippedUser}; reduced manifest entries: ${writtenEntries}.`,
);
