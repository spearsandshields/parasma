#!/usr/bin/env node
// Regenerate src/snippet-styles.generated.ts from src/snippet.css.
//
// snippet.css stays the AUTHORING source (real CSS tooling, the exported
// `@parasma/snippet/snippet.css` subpath for consumers who deliver styles
// themselves). The generated string module is what the RUNTIME adopts into
// Document/ShadowRoot via adoptedStyleSheets — bundler-agnostic (no ?inline
// magic, no css-loader requirement on consumers). A gate test asserts the two
// never drift; when it fails, run:  node scripts/gen-snippet-styles.mjs
import { readFileSync, writeFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const css = readFileSync(join(root, "packages/snippet/src/snippet.css"), "utf8");
const out = `// GENERATED from snippet.css by scripts/gen-snippet-styles.mjs — DO NOT EDIT.
// The gate's agreement check (this script, --check) fails if this drifts.
export const SNIPPET_CSS: string = ${JSON.stringify(css)};
`;
const target = join(root, "packages/snippet/src/snippet-styles.generated.ts");
if (process.argv.includes("--check")) {
  // Gate mode: byte-compare instead of writing. (A vitest agreement case was
  // tried first and reverted: vitest's css pipeline swallows `.css?raw` into
  // an empty string, making the comparison vacuously red — the check lives
  // where no bundler pipeline can touch it.)
  let current = "";
  try {
    current = readFileSync(target, "utf8");
  } catch {
    /* missing = drifted */
  }
  if (current !== out) {
    console.error("snippet-styles.generated.ts drifted from snippet.css — run: node scripts/gen-snippet-styles.mjs");
    process.exit(1);
  }
  console.log("snippet styles in sync");
} else {
  writeFileSync(target, out);
  console.log(`generated (${css.length} bytes of css)`);
}
