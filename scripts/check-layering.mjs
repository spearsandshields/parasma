#!/usr/bin/env node
// The layering rules, made able to fail. (RULE 4 joined later: engine/
// membership is computed from a reachability census, not asserted.)
//
// Three rules were WRITTEN DOWN and enforced by nothing. The root-scoping rule
// originally named its mechanism (an eslint no-restricted-globals ban on bare
// window/document in the adapters dir) — but this repo has no eslint config at
// all, so that rule never ran. A rule that matters must be able to fail; until it can, it is
// a comment.
//
// Deliberately dependency-free, matching the framework-ban check
// already in `scripts/ci-local.sh`. Pulling in eslint + typescript-eslint to
// enforce one banned identifier would add a toolchain to police a grep.
//
// Comments and string literals are stripped before matching — every historical
// false positive here was prose in a docstring ("inside the visible window").

import { readFileSync, readdirSync, statSync } from "node:fs";
import { join, relative } from "node:path";

const ROOT = new URL("..", import.meta.url).pathname.replace(/\/$/, "");
let failures = [];

/** All files under `dir` matching `exts`, recursively. */
function walk(dir, exts, out = []) {
  let entries;
  try {
    entries = readdirSync(dir);
  } catch {
    return out; // an optional tree (e.g. a crate not present) is not a failure
  }
  for (const name of entries) {
    if (name === "node_modules" || name === "dist" || name === "target") continue;
    const p = join(dir, name);
    if (statSync(p).isDirectory()) walk(p, exts, out);
    else if (exts.some((e) => p.endsWith(e))) out.push(p);
  }
  return out;
}

/**
 * Blank out comments — and, when `strings` is set, string/template literals too
 * — PRESERVING byte offsets and newlines so reported line numbers stay true to
 * the original file.
 *
 * The two rule families need different treatment, which an injection test
 * caught. Identifier rules (`window.`) MUST blank strings, or the
 * literal "window.baz" reads as a violation. Import rules must NOT: an import
 * specifier *is* a string literal, so blanking strings made rule 2 silently
 * unable to fail.
 */
function strip(src, { strings, singleQuotes = true }) {
  let out = "";
  let i = 0;
  const keep = (ch) => (ch === "\n" ? "\n" : " ");
  while (i < src.length) {
    const two = src.slice(i, i + 2);
    if (two === "//") {
      while (i < src.length && src[i] !== "\n") out += keep(src[i++]);
    } else if (two === "/*") {
      const end = src.indexOf("*/", i + 2);
      const stop = end === -1 ? src.length : end + 2;
      while (i < stop) out += keep(src[i++]);
    } else if (strings && (src[i] === '"' || (singleQuotes && src[i] === "'") || src[i] === "`")) {
      const q = src[i];
      out += " ";
      i++;
      while (i < src.length) {
        if (src[i] === "\\") {
          out += "  ";
          i += 2;
          continue;
        }
        if (src[i] === q) {
          out += " ";
          i++;
          break;
        }
        out += keep(src[i++]);
      }
    } else {
      out += src[i++];
    }
  }
  return out;
}

function lineOf(src, index) {
  return src.slice(0, index).split("\n").length;
}

function check(rule, files, patterns, { strings = true } = {}) {
  // In Rust a bare ' is a LIFETIME (&'a str), not a string opener. Treating it
  // as one blanked everything after the first lifetime tick, which made RULE 3
  // blind to engine→format imports in exactly the files that use lifetimes —
  // proven by injection (an added `use crate::wml::…` in engine/fragment.rs
  // passed). Char literals go unstripped as a result; none can contain a
  // module path, so that trade is free.
  const sq = (f) => !f.endsWith(".rs");
  if (!files.length) {
    failures.push(`${rule}: scanned ZERO files — a path moved and this rule silently stopped checking`);
    return;
  }
  for (const file of files) {
    const src = readFileSync(file, "utf8");
    const code = strip(src, { strings, singleQuotes: sq(file) });
    for (const { re, what } of patterns) {
      re.lastIndex = 0;
      let m;
      while ((m = re.exec(code)) !== null) {
        failures.push(
          `${relative(ROOT, file)}:${lineOf(code, m.index)}  ${rule}: ${what} (\`${m[0].trim()}\`)`,
        );
      }
    }
  }
}

// ── RULE 1 — no snippet adapter reads a PAGE-GLOBAL ──────────────────────────
// Every DOM concern derives from ONE RootScope threaded to every adapter, so a
// viewer in an iframe, a shadow root, or a second instance on the page talks to
// the right window. A bare `window.` silently targets the wrong one.
check(
  "root-scoping",
  walk(join(ROOT, "packages/snippet/src/adapters"), [".ts"]).filter(
    (f) => !f.endsWith(".test.ts"),
  ),
  [
    { re: /(?<![.\w$])window\s*\./g, what: "reads the page-global `window` — use `scope.win`" },
    {
      re: /(?<![.\w$])document\s*\./g,
      what: "reads the page-global `document` — use `scope.doc`",
    },
    {
      re: /instanceof\s+HTML\w*/g,
      what: "`instanceof HTML*` is realm-bound — compare against `scope.win.HTML*`",
    },
  ],
);

// ── RULE 2 — import direction: core ← headless ← snippet, never upward ───────
// The dependency spine is one-way. An upward import makes the lower layer
// unusable standalone, which is the entire point of the split.
check(
  "import-direction",
  walk(join(ROOT, "packages/parasma-core/src"), [".ts"]),
  [{ re: /from\s+["']@parasma\/(headless|snippet)/g, what: "core must not import upward" }],
  { strings: false },
);
check(
  "import-direction",
  walk(join(ROOT, "packages/headless/src"), [".ts"]),
  [{ re: /from\s+["']@parasma\/snippet/g, what: "headless must not import snippet" }],
  { strings: false },
);

// ── RULE 3 — `engine/` membership ────────────────────────────────────────────
// `engine/` holds what is COMMON to every format (fonts, shaping, geometry).
// The moment it reaches into `wml`/`sml`/`pml` it stops being shared and the
// format crates can no longer be reasoned about independently.
check(
  "engine-membership",
  walk(join(ROOT, "crates/parasma-wasm/src/engine"), [".rs"]),
  [
    {
      re: /(?:use\s+(?:crate::|super::)?|crate::|super::)(?:wml|sml|pml)::/g,
      what: "engine/ must not depend on a format module",
    },
  ],
);

// ── RULE 4 — engine/ membership is EARNED: reached from ≥2 roots ─────────────
// The dual of RULE 3. RULE 3 stops engine/ reaching DOWN into a format; this
// rule stops single-format code hiding UNDER the "shared" label — 40% of
// engine/ (geom.rs + pattern.rs, 2,475 LOC) was PML-only and nothing said so.
//
// A root is wml/, sml/, pml/, or the crate SHELL (lib.rs + everything at the
// crate root outside the format dirs: chart.rs, emf/, repair/, …). Each root
// counts once — shell counting lets worker/FFI infra like displaylist.rs pass
// legitimately (wml + shell) without an exception. Deliberately
// narrower than "transitively reachable": test files do not count (sml/pml
// tests import fonts/displaylist ubiquitously — counting them makes the rule
// vacuous), and engine-internal imports do not count (paint.rs naming
// pattern.rs's tile types must not self-justify pattern's membership for every
// format that reaches paint). A structural pin like that is exactly what a
// declared exception is for. Like RULES 1–3 the census reads text, not build
// config — cfg/feature gates are the compiler's job, knowledge topology is
// this rule's.
//
// A module reached from <2 roots must carry a grep-able exception in
// engine/mod.rs — the membership manifest — spelled on one line:
//   LAYERING-EXCEPTION(<mod>): <reason>
// The rule also fails the OTHER way when an exception goes stale (module now
// reached from ≥2 roots), so the list self-deletes as gates widen.
{
  const CRATE = join(ROOT, "crates/parasma-wasm/src");
  const ENGINE = join(CRATE, "engine");
  const isTest = (f) => f.endsWith("tests.rs") || f.includes("/tests/");
  const engineMods = readdirSync(ENGINE)
    .filter((f) => f.endsWith(".rs") && f !== "mod.rs")
    .map((f) => f.replace(/\.rs$/, ""));
  const formatDirs = ["wml", "sml", "pml"].map((d) => join(CRATE, d));
  const roots = {
    wml: walk(formatDirs[0], [".rs"]),
    sml: walk(formatDirs[1], [".rs"]),
    pml: walk(formatDirs[2], [".rs"]),
    shell: walk(CRATE, [".rs"]).filter(
      (f) => ![...formatDirs, ENGINE].some((d) => f.startsWith(d + "/")),
    ),
  };
  if (!engineMods.length)
    failures.push("engine-earned: found ZERO engine modules — a path moved and this rule silently stopped checking");
  for (const [name, files] of Object.entries(roots))
    if (!files.length)
      failures.push(`engine-earned: root '${name}' has ZERO files — a path moved and this rule silently stopped checking`);
  const stripped = Object.fromEntries(
    Object.entries(roots).map(([name, files]) => [
      name,
      // singleQuotes:false — Rust lifetimes, same reason as check()'s sq().
      files.filter((f) => !isTest(f)).map((f) => strip(readFileSync(f, "utf8"), { strings: true, singleQuotes: false })),
    ]),
  );
  const manifest = readFileSync(join(ENGINE, "mod.rs"), "utf8");
  for (const mod of engineMods) {
    // Two spellings reach a module: a path (`engine::fragment::X`) and a
    // brace-group import (`use crate::engine::{fragment, opc}`); the plain
    // path regex missed the second, so a legitimate grouped-import refactor
    // would have turned this gate spuriously red.
    const re = new RegExp(`\\bengine::${mod}\\b|\\bengine::\\{[^}]*\\b${mod}\\b`);
    const reached = Object.keys(roots).filter((name) => stripped[name].some((code) => re.test(code)));
    // [^\S\n] not \s: the reason must sit ON the marker line, or an empty
    // marker inherits the next line's prose as its "reason".
    const exception = new RegExp(`LAYERING-EXCEPTION\\(${mod}\\):[^\\S\\n]*\\S`).test(manifest);
    if (reached.length < 2 && !exception)
      failures.push(
        `crates/parasma-wasm/src/engine/${mod}.rs  engine-earned: reached only from [${reached}] — shared code needs ≥2 of {wml,sml,pml,shell}, or a declared \`LAYERING-EXCEPTION(${mod}): <reason>\` in engine/mod.rs`,
      );
    if (reached.length >= 2 && exception)
      failures.push(
        `crates/parasma-wasm/src/engine/mod.rs  engine-earned: LAYERING-EXCEPTION(${mod}) is STALE — now reached from [${reached}]; delete it`,
      );
  }
}

if (failures.length) {
  console.error(`layering violations (${failures.length}):`);
  for (const f of failures) console.error(`  ${f}`);
  process.exit(1);
}
console.log(
  "layering rules hold (root-scoping, import-direction, engine-membership, engine-earned)",
);
