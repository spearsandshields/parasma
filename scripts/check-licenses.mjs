#!/usr/bin/env node
// check-licenses.mjs — fail if any package that ships at runtime carries a
// licence outside the allow-list.
//
// Walks the production dependency closure of the published packages
// (@parasma/core, @parasma/headless, @parasma/snippet, @parasma/vue,
// @parasma/react) through node_modules and reads each package's `license`
// field. Dev dependencies never reach a user and are not checked; neither are
// the adapters' framework peers, which the consuming application supplies. The Rust side is covered separately: cargo-about refuses to
// render packages/parasma-core/THIRD-PARTY-LICENSES.md when a crate's licence
// is not in about.toml's `accepted` list.
//
// A copyleft or source-available licence here would bind every application
// that installs Parasma, so a new one is a decision, not a dependency bump.
// Usage: node scripts/check-licenses.mjs
import { existsSync, readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const ROOT = join(dirname(fileURLToPath(import.meta.url)), "..");
const PUBLISHED = ["parasma-core", "headless", "snippet", "vue", "react"];
const ALLOWED = new Set([
  "MIT", "ISC", "Apache-2.0", "BSD-2-Clause", "BSD-3-Clause", "0BSD",
  "Zlib", "CC0-1.0", "OFL-1.1", "MPL-2.0", "Unicode-3.0", "BlueOak-1.0.0",
]);

// An SPDX expression passes when every OR-alternative set contains one fully
// allowed branch: "MIT OR GPL-3.0" passes, "MIT AND GPL-3.0" does not.
function allowed(expr) {
  const e = expr.replace(/[()]/g, " ").trim();
  return e.split(/\s+OR\s+/).some((alt) => alt.split(/\s+AND\s+/).every((id) => ALLOWED.has(id.trim())));
}

function readPkg(dir) {
  return JSON.parse(readFileSync(join(dir, "package.json"), "utf8"));
}

// Resolve a dependency the way Node does: nearest node_modules upward.
function resolveDir(name, fromDir) {
  for (let d = fromDir; ; d = dirname(d)) {
    const cand = join(d, "node_modules", name);
    if (existsSync(join(cand, "package.json"))) return cand;
    if (d === dirname(d) || d === dirname(ROOT)) return null;
  }
}

const seen = new Map();
const problems = [];
const queue = PUBLISHED.map((p) => join(ROOT, "packages", p));
while (queue.length) {
  const dir = queue.shift();
  const pkg = readPkg(dir);
  const deps = { ...(pkg.dependencies ?? {}), ...(pkg.optionalDependencies ?? {}) };
  for (const name of Object.keys(deps)) {
    if (name.startsWith("@parasma/")) continue;
    const depDir = resolveDir(name, dir);
    if (!depDir) {
      if (!(name in (pkg.optionalDependencies ?? {}))) problems.push(`${name}: not installed (run bun install)`);
      continue;
    }
    if (seen.has(depDir)) continue;
    const dep = readPkg(depDir);
    const lic = typeof dep.license === "string" ? dep.license : dep.license?.type ?? "";
    seen.set(depDir, `${dep.name}@${dep.version} ${lic || "(none)"}`);
    if (!lic || !allowed(lic)) problems.push(`${dep.name}@${dep.version}: licence "${lic || "none"}" is not allowed`);
    queue.push(depDir);
  }
}

for (const line of [...seen.values()].sort()) console.log(`  ${line}`);
if (problems.length) {
  for (const p of problems) console.error(`  ✗ ${p}`);
  process.exit(1);
}
console.log(`${seen.size} runtime dependencies, all licences allowed`);
