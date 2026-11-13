/**
 * DEV/opt-in: read the user's INSTALLED fonts via the Font Access API
 * (window.queryLocalFonts) and return raw TTF bytes for the wasm shaper.
 *
 * All activation/permission/availability complexity is sealed here; the engine
 * never sees any of it. Deps are injected so this is unit-testable without a
 * browser. Returns null on ANY failure — callers fall back to the CDN chain.
 *
 * Design (behaviour verified empirically, 2026-07-10):
 *  - a lazy SESSION CATALOG (metadata + retained FontData handles — retained
 *    handles yield blob() far outside any activation window);
 *  - the 4-tier EXACT-MATCH LADDER per declared family (alphanumeric-lowercase
 *    normalized, zero fuzz): 1 family, 2 fullName, 3 postscriptName,
 *    4 family+style — validated against Chrome's real FontData strings
 *    (Chrome reports family "Avenir" for every cut);
 *  - variant selection by style metadata with regular-SYNONYM fallback
 *    (Regular/Roman/Normal/Plain/Book — Avenir has NO literal "Regular"),
 *    NEVER positional [0] (enumeration is alphabetical by psName: Avenir's
 *    [0] is Avenir-Black);
 *  - permission flow: navigator.permissions.query({name:"local-fonts"});
 *    "granted" enables SILENT enumeration (VERIFIED) while the
 *    activation-carrying click path remains sufficient on its own.
 */
export interface LocalFaceLike {
  family: string;
  fullName: string;
  postscriptName: string;
  style: string;
  blob(): Promise<Blob>;
}

/** Which rung of the resolution ladder served the bytes (fonts.debug matchTier). */
export type MatchTier = 1 | 2 | 3 | 4 | "curated" | "dev-route";

export type LocalFontPermission = "granted" | "prompt" | "denied" | "unsupported";

/** The subset of PermissionStatus the provider needs (deps-injectable). */
export interface PermissionStatusLike {
  state: string;
  onchange?: (() => void) | null;
}

export interface LocalFontDeps {
  queryLocalFonts?: () => Promise<LocalFaceLike[]>;
  hasUserActivation?: () => boolean;
  /**
   * Dev/automation byte-source. When provided, `read()` uses THIS instead of the
   * gesture-gated Font Access path — returns the bytes for an installed family
   * (e.g. a dev-server route serving the OS font) or null. No user activation
   * required.
   */
  fetchLocalFont?: (family: string, weight: number, italic: boolean) => Promise<ArrayBuffer | null>;
  /**
   * The local-fonts permission query (deps-injectable). Default:
   * navigator.permissions.query({ name: "local-fonts" }). A missing/throwing
   * query maps to "unsupported" (Firefox/Safari TypeError).
   */
  permissionsQuery?: () => Promise<PermissionStatusLike>;
}

export interface LocalReadResult {
  bytes: ArrayBuffer;
  postscriptName: string;
  /** The candidate family that actually MATCHED an installed face — identifies
   *  the resolved face (the candidate list is just the search order). */
  family: string;
  /**
   * How the face was found. Ladder tiers 1–4 apply to the FIRST candidate (the
   * DECLARED family by the plan contract); any later candidate is the curated
   * Word-substitution hint → "curated". The injected dev byte-source reports
   * "dev-route".
   */
  matchTier: MatchTier;
}

// ─── Normalization + catalog + ladder (pure, exported for tests) ─────────────

/** Alphanumeric-lowercase normalization — the ladder's ONLY equivalence (zero fuzz). */
export function normalizeFontName(name: string): string {
  return name.toLowerCase().replace(/[^a-z0-9]/g, "");
}

/**
 * The session catalog: metadata indexes over the enumerated faces, keyed by the
 * normalized ladder strings. Handles are RETAINED — blob() works for the whole
 * session without re-querying (verified empirically).
 */
export interface LocalFontCatalog {
  faces: readonly LocalFaceLike[];
  byFamily: Map<string, LocalFaceLike[]>;
  byFullName: Map<string, LocalFaceLike[]>;
  byPostscript: Map<string, LocalFaceLike[]>;
  byFamilyStyle: Map<string, LocalFaceLike[]>;
}

export function buildLocalFontCatalog(faces: LocalFaceLike[]): LocalFontCatalog {
  const byFamily = new Map<string, LocalFaceLike[]>();
  const byFullName = new Map<string, LocalFaceLike[]>();
  const byPostscript = new Map<string, LocalFaceLike[]>();
  const byFamilyStyle = new Map<string, LocalFaceLike[]>();
  // An empty normalization (fully non-alphanumeric names, e.g. CJK) must never
  // become a key — two unrelated CJK names would otherwise "match" each other.
  const add = (m: Map<string, LocalFaceLike[]>, key: string, f: LocalFaceLike): void => {
    if (!key) return;
    const arr = m.get(key);
    if (arr) arr.push(f);
    else m.set(key, [f]);
  };
  for (const f of faces) {
    add(byFamily, normalizeFontName(f.family), f);
    add(byFullName, normalizeFontName(f.fullName), f);
    add(byPostscript, normalizeFontName(f.postscriptName), f);
    add(byFamilyStyle, normalizeFontName(f.family + f.style), f);
  }
  return { faces, byFamily, byFullName, byPostscript, byFamilyStyle };
}

/**
 * The 4-tier exact-match ladder for ONE declared family. Returns the matched
 * group (tier 1 = the whole family; tiers 2–4 = the specific face(s) the name
 * identifies) and WHICH tier hit, or null — no fuzz beyond normalization.
 */
export function matchDeclaredFamily(
  catalog: LocalFontCatalog,
  declared: string,
): { tier: 1 | 2 | 3 | 4; faces: LocalFaceLike[] } | null {
  const key = normalizeFontName(declared);
  if (!key) return null;
  const t1 = catalog.byFamily.get(key);
  if (t1?.length) return { tier: 1, faces: t1 };
  const t2 = catalog.byFullName.get(key);
  if (t2?.length) return { tier: 2, faces: t2 };
  const t3 = catalog.byPostscript.get(key);
  if (t3?.length) return { tier: 3, faces: t3 };
  const t4 = catalog.byFamilyStyle.get(key);
  if (t4?.length) return { tier: 4, faces: t4 };
  return null;
}

/**
 * Style names that MEAN the regular cut. Order = preference. Avenir's regular
 * is "Roman" (no literal "Regular"; "Book" is the lighter text weight) — the
 * case this list exists for.
 */
const REGULAR_SYNONYMS = ["regular", "roman", "normal", "plain", "book"];

/**
 * Pick the face for a weight/style request within a matched group — by style
 * metadata, falling back to the group's regular face; NEVER positional [0]
 * (verified hazard: enumeration order is alphabetical by psName, so
 * Avenir's [0] is Avenir-Black).
 */
export function selectVariantFace(
  group: LocalFaceLike[],
  wantBold: boolean,
  wantItalic: boolean,
): LocalFaceLike | null {
  const styleOf = (f: LocalFaceLike): string => normalizeFontName(f.style);
  // 1. Exact style-name match for the request.
  const exact =
    wantBold && wantItalic
      ? ["bolditalic", "boldoblique"]
      : wantBold
        ? ["bold"]
        : wantItalic
          ? ["italic", "oblique"]
          : REGULAR_SYNONYMS;
  for (const name of exact) {
    const hit = group.find((f) => styleOf(f) === name);
    if (hit) return hit;
  }
  if (wantBold || wantItalic) {
    // 2. Loose predicate ("Bold Condensed", "Bold Oblique", …). Shortest style
    //    wins the tie so "Bold" beats "Semibold" — a deterministic metadata
    //    preference, not enumeration position.
    const loose = group.filter((f) => {
      const s = f.style.toLowerCase();
      const isBold = /bold/.test(s) || /\b[6-9]00\b/.test(s);
      const isItalic = /italic|oblique/.test(s);
      return isBold === wantBold && isItalic === wantItalic;
    });
    if (loose.length > 0) {
      return loose.reduce((a, b) => (styleOf(b).length < styleOf(a).length ? b : a));
    }
    // 3. Fall back to the group's regular face (design: weight/style match,
    //    else regular — e.g. Avenir bold → Roman; browser/wasm treat the bytes
    //    as that descriptor, matching the earlier behaviour).
    for (const name of REGULAR_SYNONYMS) {
      const hit = group.find((f) => styleOf(f) === name);
      if (hit) return hit;
    }
  }
  // 4. A single-face group (a tier-2/3/4 direct hit like "Avenir Book") IS its
  //    own regular. A multi-face group with no recognizable regular is a MISS —
  //    guessing among Light/Black would be the [0] hazard in disguise.
  return group.length === 1 ? group[0]! : null;
}

// ─── Provider ────────────────────────────────────────────────────────────────

export class LocalFontProvider {
  #query?: () => Promise<LocalFaceLike[]>;
  #active: () => boolean;
  #fetchLocalFont?: (family: string, weight: number, italic: boolean) => Promise<ArrayBuffer | null>;
  #permissionsQuery?: () => Promise<PermissionStatusLike>;
  /** Session catalog (lazy; retained handles). Null until first successful build. */
  #catalog: LocalFontCatalog | null = null;

  constructor(deps: LocalFontDeps = {}) {
    const g = globalThis as {
      queryLocalFonts?: () => Promise<LocalFaceLike[]>;
      navigator?: { permissions?: { query?: (d: { name: string }) => Promise<PermissionStatusLike> } };
    };
    this.#query =
      deps.queryLocalFonts ??
      (typeof g.queryLocalFonts === "function" ? () => g.queryLocalFonts!() : undefined);
    this.#active =
      deps.hasUserActivation ??
      (() => (navigator as { userActivation?: { isActive: boolean } }).userActivation?.isActive ?? false);
    this.#fetchLocalFont = deps.fetchLocalFont;
    this.#permissionsQuery =
      deps.permissionsQuery ??
      (typeof g.navigator?.permissions?.query === "function"
        ? () => g.navigator!.permissions!.query!({ name: "local-fonts" })
        : undefined);
  }

  /** True when SOME byte-source is available — lets callers skip work entirely. */
  get supported(): boolean {
    return this.#fetchLocalFont !== undefined || this.#query !== undefined;
  }

  /**
   * The local-fonts permission state. "unsupported" covers a missing
   * Permissions API AND the Firefox/Safari TypeError for the unknown name.
   */
  async permissionState(): Promise<LocalFontPermission> {
    const q = this.#permissionsQuery;
    if (!q) return "unsupported";
    try {
      const s = (await q()).state;
      return s === "granted" || s === "prompt" || s === "denied" ? s : "unsupported";
    } catch {
      return "unsupported";
    }
  }

  /**
   * Subscribe to permission transitions (PermissionStatus.onchange). The
   * callback gets (state, previous) so the viewer can flush local cache
   * entries on a granted→denied/prompt revocation. Returns an
   * unsubscribe. Never throws; a query failure simply never fires.
   */
  onPermissionChange(cb: (state: LocalFontPermission, previous: LocalFontPermission) => void): () => void {
    let disposed = false;
    let detach: (() => void) | null = null;
    const q = this.#permissionsQuery;
    if (q) {
      void (async () => {
        let status: PermissionStatusLike;
        try {
          status = await q();
        } catch {
          return;
        }
        if (disposed) return;
        let prev = (status.state as LocalFontPermission) || "unsupported";
        const handler = (): void => {
          const next = (status.state as LocalFontPermission) || "unsupported";
          const p = prev;
          prev = next;
          if (next !== p) cb(next, p);
        };
        status.onchange = handler;
        detach = () => {
          if (status.onchange === handler) status.onchange = null;
        };
      })();
    }
    return () => {
      disposed = true;
      detach?.();
    };
  }

  /** True once the session catalog has been enumerated (fonts.debug catalogReady). */
  get catalogReady(): boolean {
    return this.#catalog !== null;
  }

  /**
   * Drop the session catalog so the next need re-enumerates. Called by
   * refreshFonts (design: catalog rebuilt on refresh) and on revocation.
   */
  invalidateCatalog(): void {
    this.#catalog = null;
  }

  /**
   * Build the session catalog now if permitted (transient activation OR the
   * "granted" state — silent enumeration is VERIFIED to work when granted).
   * The one-click flow calls this synchronously inside the click so the
   * enumeration (and the permission prompt) rides the activation window.
   * Returns whether a catalog is available. Never throws.
   */
  async ensureCatalog(): Promise<boolean> {
    if (this.#catalog) return true;
    if (!this.#query) return false;
    // The activation-carrying click path stays sufficient ON ITS OWN (the
    // design's defensive floor); "granted" additionally unlocks silent runs.
    const allowed = this.#active() || (await this.permissionState()) === "granted";
    if (!allowed) return false;
    try {
      const faces = await this.#query();
      this.#catalog = buildLocalFontCatalog(faces);
      return true;
    } catch {
      return false; // SecurityError / denied / dismissed prompt
    }
  }

  /**
   * Whether a name resolves via the ladder against the CURRENT catalog:
   * true/false with a catalog, null before one exists (unknown — callers may
   * fall back to the CSS canvas probe).
   */
  installedInCatalog(name: string): boolean | null {
    if (!this.#catalog) return null;
    return matchDeclaredFamily(this.#catalog, name) !== null;
  }

  /**
   * Return bytes for the first `candidates` family that resolves via the
   * ladder (candidate 0 = the DECLARED family → tiers 1–4; later candidates
   * are curated hints → "curated"), matching weight/style per
   * {@link selectVariantFace}. Null on any miss/denial. Never throws.
   */
  async read(candidates: string[], weight: string, style: string): Promise<LocalReadResult | null> {
    const wantBold = parseInt(weight, 10) >= 600;
    const wantItalic = style === "italic";

    // Dev/automation source: a direct byte fetch (no gesture). Used in preference to
    // the Font Access path when injected. Candidates already lead with the
    // declared family (plan contract), so an exact-name install is tried first.
    if (this.#fetchLocalFont) {
      for (const fam of candidates) {
        try {
          const bytes = await this.#fetchLocalFont(fam, wantBold ? 700 : 400, wantItalic);
          if (bytes) return { bytes, postscriptName: fam, family: fam, matchTier: "dev-route" };
        } catch {
          // try the next candidate
        }
      }
      return null;
    }

    if (!(await this.ensureCatalog())) return null;
    const catalog = this.#catalog!;
    for (let i = 0; i < candidates.length; i++) {
      const fam = candidates[i]!;
      const m = matchDeclaredFamily(catalog, fam);
      if (!m) continue;
      const face = selectVariantFace(m.faces, wantBold, wantItalic);
      if (!face) continue;
      try {
        const blob = await face.blob();
        return {
          bytes: await blob.arrayBuffer(),
          postscriptName: face.postscriptName,
          family: fam,
          matchTier: i === 0 ? m.tier : "curated",
        };
      } catch {
        return null;
      }
    }
    return null;
  }
}

// ─── Cache-API persistence ───────────────────────────────────────────────────

/** Cached-entry metadata headers: which face the bytes ARE (URL-encoded so
 *  non-ASCII family names survive the ISO-8859-1 header value restriction). */
const HDR_FAMILY = "x-parasma-family";
const HDR_POSTSCRIPT = "x-parasma-postscript-name";
const HDR_TIER = "x-parasma-match-tier";

/** The synthetic Cache-API key prefix for LOCAL-sourced entries (see keyFor). */
const LOCAL_KEY_PREFIX = "https://local-font.invalid/";

/**
 * Revocation flush: delete every LOCAL-sourced entry from the
 * shared font cache (identified by the `local-font.invalid` synthetic keys),
 * leaving CDN URL-keyed entries untouched. Returns the number deleted.
 */
export async function flushLocalFontCacheEntries(cache: Cache): Promise<number> {
  const keys = await cache.keys();
  let n = 0;
  for (const req of keys) {
    if (req.url.startsWith(LOCAL_KEY_PREFIX)) {
      await cache.delete(req);
      n++;
    }
  }
  return n;
}

/**
 * Cache-first local read: serve previously-read bytes from the shared Cache-API
 * store, else read once via the provider and persist. Keyed per RESOLVED face —
 * the MATCHED family (+ weight + style), NOT the candidate list — so two
 * different candidate lists that resolve to the same installed face share one
 * entry, and the entry identifies which face it holds (family + postscriptName
 * + matchTier metadata). Lookup probes each candidate's per-face key in order
 * (a hit means that family was previously matched + persisted). Never throws;
 * returns null when no local face is available (caller falls to the CDN).
 */
export async function readLocalFontCached(
  cache: Cache,
  provider: Pick<LocalFontProvider, "read" | "permissionState">,
  candidates: string[],
  weight: string,
  style: string,
): Promise<LocalReadResult | null> {
  // The Cache API requires an http(s) request key (it rejects custom schemes on
  // put). `.invalid` is reserved (RFC 2606) and never resolves — we only ever
  // match/put it directly, never fetch it — so it's a safe synthetic key that
  // can't collide with a real CDN font URL. Old candidate-list keys used the
  // same host with a different path shape; they live in the retired
  // "parasma-fonts" cache (see FONT_CACHE_NAME's v2 bump) and are never read.
  const keyFor = (family: string) =>
    `${LOCAL_KEY_PREFIX}face/${encodeURIComponent(family.toLowerCase())}/${weight}/${style}`;

  let grantedChecked: boolean | null = null; // memoized per call
  const granted = async (): Promise<boolean> => {
    if (grantedChecked === null) grantedChecked = (await provider.permissionState()) === "granted";
    return grantedChecked;
  };
  for (let i = 0; i < candidates.length; i++) {
    const fam = candidates[i]!;
    const key = keyFor(fam);
    const hit = await cache.match(key);
    if (!hit) continue;
    const famHdr = hit.headers?.get(HDR_FAMILY);
    const psHdr = hit.headers?.get(HDR_POSTSCRIPT);
    const tierHdr = hit.headers?.get(HDR_TIER);
    // REVOCATION-WHILE-CLOSED (fail-closed): Font-Access-derived
    // bytes (any non-"dev-route" entry) must not outlive the grant. The live
    // onchange flush only covers revocations while a page is open; here we
    // guard the next-mount path — a hit whose grant is gone is deleted and
    // skipped (the caller falls to the CDN chain). Dev-route entries have no
    // permission concept and always serve.
    if (tierHdr !== "dev-route" && !(await granted())) {
      await cache.delete(key);
      continue;
    }
    // TIER SEMANTICS ARE WRITER-RELATIVE: a numeric tier recorded by the doc
    // that WROTE the entry (its declared name matched the ladder) is only
    // honest for a reader whose DECLARED name (candidates[0]) hit this key.
    // A hit via a later (curated) candidate reports "curated" regardless of
    // what the writer stored — e.g. doc B declaring "DejaVu Sans" hitting the
    // verdana entry written tier-1 by a doc that declared "Verdana".
    const rawTier: MatchTier = tierHdr
      ? /^[1-4]$/.test(tierHdr)
        ? (Number(tierHdr) as 1 | 2 | 3 | 4)
        : (tierHdr as MatchTier)
      : "curated";
    const matchTier: MatchTier =
      typeof rawTier === "number" && i !== 0 ? "curated" : rawTier;
    return {
      bytes: await hit.arrayBuffer(),
      family: famHdr ? decodeURIComponent(famHdr) : fam,
      postscriptName: psHdr ? decodeURIComponent(psHdr) : fam,
      matchTier,
    };
  }

  const r = await provider.read(candidates, weight, style);
  if (!r) return null;
  await cache.put(
    keyFor(r.family),
    new Response(r.bytes, {
      headers: {
        [HDR_FAMILY]: encodeURIComponent(r.family),
        [HDR_POSTSCRIPT]: encodeURIComponent(r.postscriptName),
        [HDR_TIER]: String(r.matchTier),
      },
    }),
  );
  return r;
}
