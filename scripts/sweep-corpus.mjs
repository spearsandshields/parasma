#!/usr/bin/env node
// The `.corpus/` robustness sweep, made reproducible.
//
// 1,122 third-party documents found the bugs that `crates/parasma-wasm/src/repair/`
// exists to fix. This is the driver that runs them through the engine, so the
// sweep can be repeated and its result compared.
//
// It is NOT part of `bun run ci` and must never be: it sweeps third-party documents
// that are deliberately malformed (19 of them are not valid zips at all), it needs a
// corpus that is not in the repo, and a "failure" here is a finding to triage, not a
// broken build. Run it on demand.
//
//   node scripts/sweep-corpus.mjs                  # whole corpus
//   node scripts/sweep-corpus.mjs --dir .corpus/poi
//   node scripts/sweep-corpus.mjs --jobs 4 --json /tmp/sweep.json
//
// Getting the corpus: the OOXML test documents of the Apache POI and LibreOffice
// repositories (see the usage message below); it is not, and should not be, committed.
//
// ── What "OK" does and does not mean ────────────────────────────────────────────
// This is a CRASH SCREEN, not a fidelity oracle. `PROBE_OK` proves the loader did
// not reject the file and the layout did not panic. It proves nothing about what
// was rendered: the probe reports only a page count, and `page_count()` has a
// documented min-one-page floor (`empty_body_yields_one_page`), so a regression
// that dropped an entire document body would still report `PROBE_OK pages=1`. A
// pass rate from this script is "did not reject and did not crash" — do not quote
// it as "renders correctly". Turning it into a regression oracle needs content
// counters on the PROBE_OK line (lines/items) plus a per-file baseline; that is a
// change to the probe and is deliberately not made here.
//
// ── Why process-per-file, and why the raw test binary ───────────────────────────
// PROCESS ISOLATION IS LOAD-BEARING, not a convenience:
//   * The single most valuable historical finding (`deep-table-cell.docx`, 5000
//     nesting levels, fixed by repair RULE 7) presented as a STACK OVERFLOW — a
//     guard-page SIGSEGV. `catch_unwind` cannot contain that, nor OOM, nor a hang.
//     An in-process loop dies on such a file and loses the rest of the run.
//   * Five `thread_local!` blocks carry layout state across documents (FIELD_DATES,
//     HYPERLINK_RELS, RESERVE_TRAILING_BLANKS, OPEN_COMMENT_IDS, EMF ACTIVE /
//     PAINT_REQUESTS). They are republished per `run_layout`, but the layout restartability
//     self-test already caught a real OPEN_COMMENT_IDS replay bug in exactly this class. A
//     shared process makes any anomaly ambiguous between engine bug and driver bleed.
// The cost of that isolation is ~4-5 ms/file of process + Viewer + font setup, i.e.
// a few seconds across the whole corpus — and zero in wall-clock terms, because
// forking is what makes the parallelism free.
//
// We fork the RAW TEST BINARY rather than `cargo test` per file: measured 6.8 ms vs
// 59.9 ms per file (8.8x), and — the reason that matters here — `cargo test` takes
// the cargo build lock on every invocation, so a per-file sweep serialises against
// any concurrent build in the tree.
//
// ── Exit code contract ──────────────────────────────────────────────────────────
// Exit 0 whatever the documents do. Non-zero ONLY when the driver itself could not
// run (no test binary, unreadable corpus). An absent corpus is exit 0 with
// instructions for fetching it — silence and a crash are both wrong there.

import { execFile, execFileSync } from "node:child_process";
import { existsSync, mkdirSync, readdirSync, statSync, writeFileSync } from "node:fs";
import { cpus } from "node:os";
import { dirname, join, resolve, sep } from "node:path";

const ROOT = resolve(new URL("..", import.meta.url).pathname);

// ── args ────────────────────────────────────────────────────────────────────────

const args = process.argv.slice(2);
function opt(name, fallback) {
  const i = args.indexOf(name);
  return i === -1 ? fallback : args[i + 1];
}
const CORPUS = resolve(ROOT, opt("--dir", ".corpus"));
const JOBS = Math.max(1, Number(opt("--jobs", String(Math.min(8, cpus().length)))) || 1);
// ⚠️ WHAT THIS DOES NOT MEASURE — read before citing it as evidence.
//
// This sweep measures LOADABILITY: did the document parse and lay out without
// rejecting, panicking or hanging. It does NOT measure layout correctness, and
// it is blind to almost every rendering change.
//
// Measured 2026-07-28: disabling the per-page float fixpoint ENTIRELY (forcing the
// rollback branch to ACCEPT) leaves this sweep byte-identical at 1102/20 =
// 98.2%. It cannot distinguish a working engine from one with its float
// fixpoint removed. Using "the corpus sweep held" as an acceptance gate for a
// layout refactor is a category error.
//
// Use it for: robustness against hostile/malformed input, crash regressions,
// parser coverage. Do NOT use it for: behavioural preservation, layout
// fidelity, or anything a golden pin is the right instrument for.

// The tail is long and real: on the 1,122-document corpus the slowest document
// measured 19.2 s, the next two 13.4 s and 8.8 s, and six exceeded 2 s.
// A timeout under ~30 s reports engine slowness as a hang.
const TIMEOUT_MS = (Number(opt("--timeout", "120")) || 120) * 1000;
const LIMIT = Number(opt("--limit", "0")) || 0;
const FILTER = opt("--filter", null);
const JSON_OUT = opt("--json", null);
const QUIET = args.includes("--quiet");

if (args.includes("--help") || args.includes("-h")) {
  process.stdout.write(
    `sweep-corpus — throw a corpus of .docx at the engine and report what survives\n\n` +
      `  --dir <path>      corpus root (default .corpus)\n` +
      `  --jobs <n>        parallel processes (default min(8, ncpu))\n` +
      `  --timeout <sec>   per-file timeout (default 120; slowest known file is 18.5 s)\n` +
      `  --limit <n>       sweep only the first n files (per whole run, after sorting)\n` +
      `  --filter <substr> only files whose path contains substr\n` +
      `  --json <path>     write the full per-file result set here\n` +
      `  --quiet           no progress output\n`,
  );
  process.exit(0);
}

// ── corpus discovery ────────────────────────────────────────────────────────────

/** Every `.docx` under `dir`, recursively. */
function walk(dir, out = []) {
  for (const name of readdirSync(dir)) {
    const p = join(dir, name);
    const st = statSync(p);
    if (st.isDirectory()) walk(p, out);
    else if (name.toLowerCase().endsWith(".docx")) out.push(p);
  }
  return out;
}

function missingCorpus(reason) {
  process.stdout.write(
    `sweep-corpus: ${reason}\n\n` +
      `  looked in: ${CORPUS}\n\n` +
      `The corpus is third-party and deliberately NOT committed. Fetch it with a\n` +
      `blobless partial clone + sparse checkout (the contents API truncates):\n\n` +
      `  git clone --filter=blob:none --no-checkout https://github.com/apache/poi\n` +
      `  git -C poi sparse-checkout set test-data/document\n` +
      `  git clone --filter=blob:none --no-checkout https://github.com/LibreOffice/core\n` +
      `  git -C core sparse-checkout set sw/qa/extras/ooxmlimport/data sw/qa/extras/ooxmlexport/data\n\n` +
      `Nothing was swept.\n`,
  );
  process.exit(0);
}

if (!existsSync(CORPUS)) missingCorpus("no corpus directory.");
let files;
try {
  files = walk(CORPUS);
} catch (e) {
  process.stderr.write(`sweep-corpus: cannot read corpus at ${CORPUS}: ${e.message}\n`);
  process.exit(1);
}
if (FILTER) files = files.filter((f) => f.includes(FILTER));
files.sort();
if (LIMIT) files = files.slice(0, LIMIT);
if (files.length === 0) missingCorpus("corpus directory contains no .docx files.");

/** Sub-corpus = the path segment directly under the corpus root ("" if loose). */
function subcorpusOf(file) {
  const rel = file.slice(CORPUS.length + 1);
  const i = rel.indexOf(sep);
  return i === -1 ? "(root)" : rel.slice(0, i);
}

// ── test binary ─────────────────────────────────────────────────────────────────

/**
 * Build (if needed) and locate the `parasma-wasm` test binary. `--no-run` is one
 * cargo invocation for the whole sweep — the per-file forks then touch no lock.
 */
function resolveTestBinary() {
  if (!QUIET) process.stderr.write("sweep-corpus: building test binary…\n");
  let out;
  try {
    out = execFileSync(
      "cargo",
      [
        "test", "-p", "parasma-wasm",
        "--features", "sml,pml,emf",
        "--locked", "--no-run",
        "--message-format=json",
      ],
      { cwd: ROOT, encoding: "utf8", maxBuffer: 256 * 1024 * 1024, stdio: ["ignore", "pipe", "inherit"] },
    );
  } catch (e) {
    process.stderr.write("sweep-corpus: `cargo test --no-run` failed; nothing to sweep with.\n");
    process.exit(1);
  }
  const exes = [];
  for (const line of out.split("\n")) {
    if (!line.startsWith("{")) continue;
    let m;
    try {
      m = JSON.parse(line);
    } catch {
      continue;
    }
    if (m.reason === "compiler-artifact" && m.executable && m.target?.test) exes.push(m.executable);
  }
  if (exes.length !== 1) {
    process.stderr.write(
      `sweep-corpus: expected exactly one parasma-wasm test executable, got ${exes.length}:\n` +
        exes.map((e) => `  ${e}\n`).join(""),
    );
    process.exit(1);
  }
  return exes[0];
}

const BIN = resolveTestBinary();
const PROBE = "probe_open_arbitrary_docx";

/**
 * Fail loudly if the filter selects anything other than exactly the probe.
 * Without this, a renamed or unmatched test makes libtest print "running 0
 * tests" and exit 0 — which every file would then report as its own anomaly,
 * turning one driver bug into 1,122 phantom findings. (Note the filter is a
 * SUBSTRING match, deliberately: libtest's `--exact` wants the full
 * `oracle_tests::probe_open_arbitrary_docx` path, which would break the moment
 * the probe moved module.)
 */
function assertProbeSelected() {
  let listing;
  try {
    listing = execFileSync(BIN, ["--ignored", "--list", PROBE], { encoding: "utf8" });
  } catch (e) {
    process.stderr.write(`sweep-corpus: cannot list tests in ${BIN}: ${e.message}\n`);
    process.exit(1);
  }
  const hits = listing.split("\n").filter((l) => l.endsWith(": test"));
  if (hits.length !== 1) {
    process.stderr.write(
      `sweep-corpus: filter '${PROBE}' selected ${hits.length} tests, expected exactly 1.\n` +
        hits.map((h) => `  ${h}\n`).join(""),
    );
    process.exit(1);
  }
}
assertProbeSelected();

// ── classification ──────────────────────────────────────────────────────────────
//
// STDOUT MARKERS FIRST, exit code only as the fallback bucket. The probe reports a
// rejected document by PRINTING and returning, so a correct rejection exits 0; a
// panic prints nothing and exits 101; an abort (stack overflow / OOM) prints
// nothing and dies on a signal. Reading the exit code first would invert the two
// most interesting categories.

const CATEGORIES = ["ok", "parse_err", "layout_err", "crash", "timeout", "unknown"];

function classify(stdout, err) {
  if (err?.killed) return { category: "timeout", detail: `no result in ${TIMEOUT_MS / 1000}s` };
  for (const line of stdout.split("\n")) {
    if (line.startsWith("PROBE_OK ")) {
      const pages = Number(/pages=(\d+)/.exec(line)?.[1] ?? -1);
      const ms = Number(/ms=(\d+)/.exec(line)?.[1] ?? -1);
      return { category: "ok", pages, ms };
    }
    if (line.startsWith("PROBE_PARSE_ERR ")) {
      return { category: "parse_err", detail: line.slice("PROBE_PARSE_ERR ".length).trim() };
    }
    if (line.startsWith("PROBE_LAYOUT_ERR ")) {
      // Note: run_layout_native flattens every layout error to one constant
      // string, so this detail carries no diagnosis — it says only "layout said
      // Err". Triaging one needs a second, single-file run.
      return { category: "layout_err", detail: line.slice("PROBE_LAYOUT_ERR ".length).trim() };
    }
  }
  // No marker. Panic (rc 101) or uncatchable abort (signal) — the categories this
  // driver exists to survive.
  if (err) {
    const sig = err.signal ? ` signal=${err.signal}` : "";
    const code = err.code === undefined ? "" : ` rc=${err.code}`;
    // Rust's panic banner is two lines and carries a thread id between the thread
    // name and "panicked at", so the message is on the line AFTER the location:
    //   thread 'oracle_tests::probe…' (4383764) panicked at src/…rs:7421:57:
    //   read /path: Permission denied (os error 13)
    const m = /thread '[^']*'[^\n]*panicked at ([^\n]*)\n([^\n]*)/.exec(err.stderr ?? "");
    const overflow = /stack overflow/i.test(err.stderr ?? "");
    const what = m
      ? `panic: ${m[2].trim()} (at ${m[1].replace(/:$/, "")})`
      : overflow
        ? "stack overflow (abort)"
        : "no marker, no panic text";
    return { category: "crash", detail: what + code + sig };
  }
  return { category: "unknown", detail: "exited 0 with no PROBE_ marker" };
}

function runOne(file) {
  return new Promise((done) => {
    const t0 = Date.now();
    execFile(
      BIN,
      ["--ignored", "--nocapture", PROBE],
      {
        cwd: ROOT,
        env: { ...process.env, PROBE_DOCX: file, RUST_BACKTRACE: "0" },
        timeout: TIMEOUT_MS,
        killSignal: "SIGKILL",
        maxBuffer: 32 * 1024 * 1024,
        encoding: "utf8",
      },
      (err, stdout, stderr) => {
        if (err) {
          err.stdout = stdout;
          err.stderr = stderr;
        }
        done({
          file,
          sub: subcorpusOf(file),
          wall_ms: Date.now() - t0,
          ...classify(stdout ?? "", err),
        });
      },
    );
  });
}

// ── run ─────────────────────────────────────────────────────────────────────────

const results = [];
let next = 0;
const started = Date.now();

async function worker() {
  while (next < files.length) {
    const i = next++;
    results.push(await runOne(files[i]));
    if (!QUIET && results.length % 50 === 0) {
      process.stderr.write(`  ${results.length}/${files.length}…\n`);
    }
  }
}

if (!QUIET) {
  process.stderr.write(
    `sweep-corpus: ${files.length} files, ${JOBS} jobs, ${TIMEOUT_MS / 1000}s timeout\n`,
  );
}
await Promise.all(Array.from({ length: Math.min(JOBS, files.length) }, worker));
const wall = (Date.now() - started) / 1000;

// ── report ──────────────────────────────────────────────────────────────────────

results.sort((a, b) => a.file.localeCompare(b.file));

const subs = [...new Set(results.map((r) => r.sub))].sort();
const tally = (rows) => {
  const t = Object.fromEntries(CATEGORIES.map((c) => [c, 0]));
  for (const r of rows) t[r.category]++;
  return t;
};

const W = Math.max(12, ...subs.map((s) => s.length));
const head = ["sub-corpus".padEnd(W), "files", "  OK", " rej", " lay", " crash", " t/o", "  unk", "     %"];
const out = [];
out.push("");
out.push(`sweep-corpus — ${files.length} documents, ${JOBS} jobs, ${wall.toFixed(1)}s wall`);
out.push(`corpus: ${CORPUS}`);
out.push(`binary: ${BIN}`);
out.push("");
out.push(head.join(" "));
out.push("-".repeat(head.join(" ").length));

function row(label, rows) {
  const t = tally(rows);
  const pct = rows.length ? (100 * t.ok) / rows.length : 0;
  return [
    label.padEnd(W),
    String(rows.length).padStart(5),
    String(t.ok).padStart(4),
    String(t.parse_err).padStart(4),
    String(t.layout_err).padStart(4),
    String(t.crash).padStart(6),
    String(t.timeout).padStart(4),
    String(t.unknown).padStart(5),
    `${pct.toFixed(1)}%`.padStart(6),
  ].join(" ");
}

for (const s of subs) out.push(row(s, results.filter((r) => r.sub === s)));
out.push("-".repeat(head.join(" ").length));
out.push(row("TOTAL", results));
out.push("");
out.push(
  "  OK = did not reject and did not crash. NOT a fidelity claim — see the header",
  "  of this script: the probe reports only a page count, and one page is a floor.",
);

// Per-failure detail: filename + the actual error. This is the part a triager
// needs, so it is never elided.
for (const cat of ["crash", "timeout", "layout_err", "unknown", "parse_err"]) {
  const rows = results.filter((r) => r.category === cat);
  if (!rows.length) continue;
  out.push("");
  out.push(`── ${cat} (${rows.length}) ${"─".repeat(Math.max(0, 60 - cat.length))}`);
  for (const r of rows) {
    out.push(`  ${r.file.slice(CORPUS.length + 1)}`);
    out.push(`      ${(r.detail ?? "").split("\n")[0].slice(0, 400)}`);
  }
}

const slow = results.filter((r) => r.category === "ok").sort((a, b) => b.wall_ms - a.wall_ms).slice(0, 5);
if (slow.length) {
  out.push("");
  out.push("── slowest (ok) ──────────────────────────────────────────────────");
  for (const r of slow) {
    out.push(`  ${(r.wall_ms / 1000).toFixed(2)}s  ${r.file.slice(CORPUS.length + 1)}`);
  }
}

const zero = results.filter((r) => r.category === "ok" && r.pages === 0);
if (zero.length) {
  out.push("");
  out.push(`── pages==0 (${zero.length}) — should be impossible under the min-one-page floor`);
  for (const r of zero) out.push(`  ${r.file.slice(CORPUS.length + 1)}`);
}

out.push("");
process.stdout.write(out.join("\n") + "\n");

if (JSON_OUT) {
  // Relative to the CWD, like every other CLI. Resolving against the repo root
  // surprises the caller and can drop a report — which lists every swept
  // document by name — somewhere they did not choose.
  const p = resolve(process.cwd(), JSON_OUT);
  mkdirSync(dirname(p), { recursive: true });
  writeFileSync(
    p,
    JSON.stringify(
      {
        corpus: CORPUS,
        binary: BIN,
        jobs: JOBS,
        timeout_s: TIMEOUT_MS / 1000,
        wall_s: Number(wall.toFixed(2)),
        total: tally(results),
        by_subcorpus: Object.fromEntries(
          subs.map((s) => [s, tally(results.filter((r) => r.sub === s))]),
        ),
        results: results.map((r) => ({ ...r, file: r.file.slice(CORPUS.length + 1) })),
      },
      null,
      2,
    ),
  );
  process.stderr.write(`sweep-corpus: wrote ${p}\n`);
}

// Diagnostic tool: the documents' behaviour never sets the exit code.
process.exit(0);
