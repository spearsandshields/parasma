/**
 * Coverage detector (local-font fidelity): classify ONE
 * declared family's local-font situation from provenance + availability facts.
 * PURE — callers (Viewer.fontProvenance, the dev FONTS tab) merge the catalog
 * and/or the CSS canvas probe into `candidateInstalled` themselves.
 */
import { normalizeFontName, type MatchTier } from "./local-font-provider";

export type FontCoverage =
  /** Ladder hit — the DECLARED family's own local bytes are rendering. */
  | "exact-local"
  /** The curated Word-substitute's local bytes are rendering (e.g. Verdana for DejaVu Sans). */
  | "substituted-local"
  /** A ladder/curated candidate IS installed but NOT rendering — the benefit case. */
  | "substituted-local-available"
  /** The CDN substitute is rendering; nothing local detected (or unknown). */
  | "substituted"
  /** Engine default (no substitute / failed load); nothing local detected. */
  | "missing"
  /** CJK-routed → bundled Noto; EXCLUDED from coverage counting v1 (follow-on). */
  | "cjk (bundled Noto)";

export interface FontCoverageInput {
  /** The family name as the document declares it. */
  declared: string;
  /** The plan's substitute family, or null (engine default). */
  resolved: string | null;
  /** Regular-face byte-source provenance. */
  origin: "local" | "cdn" | "failed" | null;
  /** The ladder tier that served the local bytes, when origin is local. */
  matchTier: MatchTier | null;
  /** The candidate family the local bytes matched (null when not local). */
  matchedFamily: string | null;
  /** CJK-routed family (plan resolves to the bundled Noto CJK). */
  isCjk: boolean;
  /**
   * Whether ANY ladder/curated candidate is installed locally (session catalog
   * and/or the CSS canvas probe); null = unknown (no catalog, no probe) —
   * treated as not-detected, never claimed as available.
   */
  candidateInstalled: boolean | null;
}

export function computeFontCoverage(f: FontCoverageInput): FontCoverage {
  if (f.isCjk) return "cjk (bundled Noto)";
  if (f.origin === "local") {
    // "Exact" = the declared name itself matched: any ladder tier (1–4 all
    // identify the declared string), or the dev route serving the declared
    // candidate. "curated" (and the dev route serving a curated candidate) is
    // Word's substitution rendered from local bytes.
    const exact =
      typeof f.matchTier === "number" ||
      (f.matchTier === "dev-route" &&
        f.matchedFamily !== null &&
        normalizeFontName(f.matchedFamily) === normalizeFontName(f.declared));
    return exact ? "exact-local" : "substituted-local";
  }
  // Not rendering locally: an installed candidate is the actionable case,
  // whether the fallback is a CDN substitute or the engine default.
  if (f.candidateInstalled === true) return "substituted-local-available";
  if (f.resolved === null || f.origin === "failed") return "missing";
  return "substituted";
}
