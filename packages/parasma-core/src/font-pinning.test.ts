/**
 * The bundled font sources must be IMMUTABLE, and must agree with the
 * native harness.
 *
 * Byte-identity is the safety net this repo leans on, and it was capped by the
 * font sources: `FONT_URLS` fetched eight families from a branch ref (`@main` /
 * `@master`), so an upstream rebuild changed the metrics the engine measures
 * with no commit of ours and nothing going red. It had already fired — Cousine's
 * 2026-06-09 rebuild held `glyf`/`hmtx` byte-identical but moved OS/2
 * sTypoAscender 1255→1705 and sTypoDescender −386→−615, taking the natural line
 * height of every monospace line from 0.8013em to 1.1328em.
 *
 * These are STATIC checks — they need no network, so they can live in the gate.
 * They read the SHIPPING values out of the module rather than grepping the
 * source for `@main`, deliberately: several suites feed `…/carlito@main/…` to
 * the origin-rewriting resolvers as a fixture, and those are inputs to a string
 * replace, not URLs anything fetches. A text grep would either flag them or
 * teach people to work around it.
 *
 * What they cover:
 *   - no bundled URL may name a mutable ref (branch, tag, npm range);
 *   - a family fetched by BOTH the browser and `scripts/fetch-test-fonts.sh`
 *     must name the same version in both, so the browser renders against the
 *     bytes every pinned baseline was calibrated on.
 *
 * What they CANNOT cover: whether the CDN still serves the same bytes for a
 * given commit (upstream force-push, jsDelivr tamper). No offline test can, and
 * a networked test would make the gate flaky. The procedure for that, and for
 * bumping a pin, is `scripts/verify-font-pins.sh` — run it when you change a
 * commit in either file, and re-run the native baselines behind any change it
 * reports.
 */

import { readFileSync } from "node:fs";
import { describe, expect, it } from "vitest";
import { BUNDLED_FONT_URLS } from "./font-resolver";

/** Repo root is three levels up from packages/parasma-core/src/. */
const NATIVE_FETCH_SCRIPT = new URL(
  "../../../scripts/fetch-test-fonts.sh",
  import.meta.url,
);

/** `https://cdn.jsdelivr.net/gh/<owner>/<repo>@<ref>/<path>` */
const GH_URL = /^https:\/\/cdn\.jsdelivr\.net\/gh\/[^/@]+\/[^/@]+@([^/]+)\/(.+)$/;
/** `https://cdn.jsdelivr.net/npm/<pkg>@<version>/<path>` */
const NPM_URL = /^https:\/\/cdn\.jsdelivr\.net\/npm\/[^/@]+@([^/]+)\/(.+)$/;

const COMMIT_SHA = /^[0-9a-f]{40}$/;
/** Exact version only — `^1.2.3`, `~1.2.3`, `latest` and `1.2` are all mutable. */
const EXACT_SEMVER = /^\d+\.\d+\.\d+$/;

/** The ref a bundled URL pins to, and the file it fetches. */
function pinOf(url: string): { ref: string; file: string } {
  const m = GH_URL.exec(url) ?? NPM_URL.exec(url);
  const [, ref, path] = m ?? [];
  if (ref === undefined || path === undefined) {
    throw new Error(`unrecognised bundled font URL shape: ${url}`);
  }
  return { ref, file: decodeURIComponent(path.split("/").pop()!) };
}

describe("bundled font URLs are pinned", () => {
  it("has a URL set to check (guards against a vacuous pass)", () => {
    expect(BUNDLED_FONT_URLS.length).toBeGreaterThanOrEqual(20);
  });

  it("names no mutable ref — every source is a commit sha or an exact version", () => {
    const mutable = BUNDLED_FONT_URLS.filter((u) => {
      if (GH_URL.test(u)) return !COMMIT_SHA.test(pinOf(u).ref);
      if (NPM_URL.test(u)) return !EXACT_SEMVER.test(pinOf(u).ref);
      return true; // an unrecognised shape is unreviewed, so treat it as unpinned
    });
    expect(mutable).toEqual([]);
  });
});

describe("bundled font pins agree with the native harness", () => {
  const script = readFileSync(NATIVE_FETCH_SCRIPT, "utf8");

  // Membership is derived from the script rather than listed here, so a family
  // added to the native harness gains this coverage without anyone remembering.
  const shared = BUNDLED_FONT_URLS.filter((u) => script.includes(pinOf(u).file));

  it("covers the families the native harness actually loads", () => {
    expect(shared.map((u) => pinOf(u).file)).toEqual(
      expect.arrayContaining([
        "Arimo-Regular.ttf", // the oracle basis (load_test_fonts)
        "XITSMath-Regular.otf", // ditto, for the OMML pins
        "Carlito-Regular.ttf", // ground-truth basis
        "Tinos-Regular.ttf",
        "OpenSans-Regular.ttf",
        "DejaVuSans.ttf",
      ]),
    );
  });

  it("fetches every shared family from the version the script sha256-verifies", () => {
    // PER-FAMILY, not substring-anywhere. The first spelling of this test was
    // `script.includes(pinOf(u).ref)`, which accepts ANY sha present anywhere in
    // the script — including a DIFFERENT family's commit. That is the single
    // most likely hand-editing mistake (copy a pin line, forget to change the
    // sha), and it passed green. Bind the family to its own commit variable.
    const scriptCommits = new Map(
      [...script.matchAll(/^([A-Z_]+)_COMMIT="([0-9a-f]{40})"/gm)].map((m) => [
        (m[1] ?? "").toLowerCase(),
        m[2] ?? "",
      ]),
    );
    expect(scriptCommits.size, "parsed no *_COMMIT vars — the script's format changed").
      toBeGreaterThanOrEqual(4);

    const mismatched = shared
      .map((u) => {
        const { ref, file } = pinOf(u);
        // "Carlito-Bold.ttf" → "carlito"; "DejaVuSans.ttf" → npm, skipped below.
        const family = ((file.split("/").pop() ?? "").split(/[-.]/)[0] ?? "").toLowerCase();
        const want = scriptCommits.get(family) ?? scriptCommits.get(family.replace(/sans$/, ""));
        return want && want !== ref ? `${family}: browser ${ref} vs script ${want}` : null;
      })
      .filter(Boolean);
    expect(mismatched).toEqual([]);
  });

  it("keeps the script's own commits immutable too", () => {
    const commits = [...script.matchAll(/^([A-Z_]+_COMMIT)="([^"]*)"/gm)];
    expect(commits.length).toBeGreaterThanOrEqual(4);
    const notShas = commits
      .map(([, name, sha]) => `${name}=${sha}`)
      .filter((assignment) => !COMMIT_SHA.test(assignment.split("=")[1] ?? ""));
    expect(notShas).toEqual([]);
    // Belt and braces: nothing in the script may fall back to a branch ref.
    expect(script).not.toMatch(/@(main|master|HEAD|latest)\b/);
  });
});
