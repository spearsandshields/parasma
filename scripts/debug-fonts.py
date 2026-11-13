#!/usr/bin/env python3
"""debug-fonts — download the REAL fonts a document declares and dump their
metrics, for layout/line-height investigation. Optionally emit a loader so the
dev debug-probe viewer renders with the real faces (font-substitution tests).

The downloaded fonts land in a gitignored `.debug-fonts/` at the repo root —
they are an investigation aid ONLY and must never be committed or reach the
@parasma/core package. URL sources are read out of the repo's own
font-resolver.ts by evaluating it (needs `bun`), so the tool resolves families
exactly as core does — including its pinned upstream commits.

Usage:
  scripts/debug-fonts.py <doc.docx|xlsx|pptx>          # families from the doc
  scripts/debug-fonts.py --families "DejaVu Sans,Open Sans"
  scripts/debug-fonts.py <doc> --pt 10.5 --mult 1.2    # tune the pitch column
  scripts/debug-fonts.py <doc> --load                  # also write a viewer loader
  scripts/debug-fonts.py <doc> --all-styles            # download bold/italic too
"""
import sys, os, re, json, struct, zipfile, argparse, subprocess, urllib.request

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.dirname(HERE)
CORE_PKG = os.path.join(REPO, "packages/parasma-core")
OUT = os.path.join(REPO, ".debug-fonts")

# Proprietary font families can't be downloaded; the open metric-compatible
# clone matches their metrics by design, so substitute for investigation purposes.
CLONE = {
    "Arial": "Arimo", "Helvetica": "Arimo", "Liberation Sans": "Arimo",
    "Times New Roman": "Tinos", "Times": "Tinos", "Liberation Serif": "Tinos",
    "Calibri": "Carlito", "Cambria": "Caladea",
    "Courier New": "Cousine", "Courier": "Cousine", "Liberation Mono": "Cousine",
}

SYSTEM_DIRS = ["/System/Library/Fonts", "/Library/Fonts",
               os.path.expanduser("~/Library/Fonts")]
# Office bundles cloud fonts (Segoe UI, Aptos, …) OUTSIDE the OS font dirs, with
# HASHED filenames — exactly the faces Word substitutes to (Open Sans → Segoe UI).
# These need name-table matching, not filename matching.
import glob as _glob
OFFICE_DIRS = _glob.glob(os.path.expanduser(
    "~/Library/Group Containers/*.Office/FontCache"))
STYLE_NORM = {"regular": "", "bold": "bold", "italic": "italic", "boldItalic": "bolditalic"}

def _name_table_match(path, want, want_bold, want_italic):
    """True if the font at `path` is family `want` (normalized) in the requested
    bold/italic style, per its OpenType name/OS2 tables. For hashed Office files."""
    try:
        from fontTools.ttLib import TTFont
        t = TTFont(path, lazy=True, fontNumber=0)
        fam = re.sub(r"[^a-z0-9]", "", (t["name"].getDebugName(1) or "").lower())
        sub = (t["name"].getDebugName(2) or "").lower()
        fs = t["OS/2"].fsSelection if "OS/2" in t else 0
        is_bold = bool(fs & 0x20) or "bold" in sub
        is_italic = bool(fs & 0x01) or "italic" in sub or "oblique" in sub
        return fam == want and is_bold == want_bold and is_italic == want_italic
    except Exception:
        return False

def find_system_font(family, style):
    """Find a locally-installed face by family+style (the EXACT fonts the OS/Word
    use). OS dirs match by filename; Office dirs match by name table (hashed names)."""
    want = re.sub(r"[^a-z0-9]", "", family.lower())
    target = want + STYLE_NORM[style]
    for d in SYSTEM_DIRS:
        if not os.path.isdir(d):
            continue
        for root, _, files in os.walk(d):
            for fn in files:
                if not fn.lower().endswith((".ttf", ".otf")):
                    continue
                base = re.sub(r"\.(ttf|otf)$", "", fn, flags=re.I)
                if re.sub(r"[^a-z0-9]", "", base.lower()) == target:
                    return os.path.join(root, fn)
    # Office cloud fonts: hashed filenames, so match the name table instead.
    want_bold = "bold" in style.lower()
    want_italic = "italic" in style.lower()
    for d in OFFICE_DIRS:
        if not os.path.isdir(d):
            continue
        for root, _, files in os.walk(d):
            for fn in files:
                if not fn.lower().endswith((".ttf", ".otf")):
                    continue
                p = os.path.join(root, fn)
                if _name_table_match(p, want, want_bold, want_italic):
                    return p
    return None

def load_font_urls():
    """Family -> {regular,bold,italic,boldItalic} exactly as @parasma/core resolves it.

    EVALUATES the module rather than scraping it. This used to regex out
    `regular: "https://…"` literals; once the URLs were built from a
    pinned-commit table there were no literals left and the tool silently reported
    "no source" for every family. Asking the module cannot drift that way.
    """
    out = subprocess.run(
        ["bun", "-e",
         'import { BUNDLED_FONT_FAMILIES } from "./src/font-resolver";'
         " console.log(JSON.stringify(BUNDLED_FONT_FAMILIES))"],
        cwd=CORE_PKG, capture_output=True, text=True)
    if out.returncode != 0:
        sys.exit("could not read font URLs from @parasma/core (is bun installed?):\n"
                 + out.stderr.strip())
    urls = json.loads(out.stdout)
    # DejaVuSans key (unquoted) is the doc family "DejaVu Sans"
    if "DejaVuSans" in urls and "DejaVu Sans" not in urls:
        urls["DejaVu Sans"] = urls["DejaVuSans"]
    return urls

def resolve(family, urls):
    """family -> (source_family, kind, {style: url}) or None."""
    if family in urls:
        return (family, "real" if family in ("DejaVu Sans", "Open Sans") or family.startswith("Noto") else "clone", urls[family])
    clone = CLONE.get(family)
    if clone and clone in urls:
        return (clone, f"clone-of:{family}", urls[clone])
    return None

def extract_families(docpath):
    fams = set()
    def add(v):
        for f in v.split(";"):
            f = f.strip()
            # skip theme refs (+mn-lt) and language tags (w:lang also has an
            # eastAsia attr — e.g. "zh-CN"/"zxx"/"en-US" — which is NOT a font)
            if f and not f.startswith("+") and not re.fullmatch(r"[a-z]{2,3}(-[A-Z]{2,4})?", f):
                fams.add(f)
    with zipfile.ZipFile(docpath) as z:
        for name in z.namelist():
            if not name.endswith(".xml"):
                continue
            try:
                data = z.read(name).decode("utf-8", "replace")
            except Exception:
                continue
            # WML/SML: only the font attrs INSIDE a <w:rFonts ...> element
            for rf in re.findall(r"<w:rFonts\b[^>]*>", data):
                for attr in re.findall(r'w:(?:ascii|hAnsi|cs|eastAsia)="([^"]+)"', rf):
                    add(attr)
            # DrawingML / PPTX: <a:latin|ea|cs typeface="...">
            for tf in re.findall(r"<a:(?:latin|ea|cs)\b[^>]*typeface=\"([^\"]+)\"", data):
                add(tf)
    return sorted(fams)

# ── TTF/OTF (sfnt) metric parsing — no dependencies ──────────────────────────
def read_metrics(path):
    data = open(path, "rb").read()
    if data[:4] == b"ttcf":
        raise ValueError("TrueType Collection not supported")
    num = struct.unpack(">H", data[4:6])[0]
    tabs, off = {}, 12
    for _ in range(num):
        tag = data[off:off+4].decode("latin1")
        toff = struct.unpack(">I", data[off+8:off+12])[0]
        tabs[tag] = toff
        off += 16
    s16 = lambda b: struct.unpack(">h", b)[0]
    u16 = lambda b: struct.unpack(">H", b)[0]
    h = tabs["head"]; upem = u16(data[h+18:h+20])
    a = tabs["hhea"]
    m = {"upem": upem, "hheaAsc": s16(data[a+4:a+6]),
         "hheaDesc": s16(data[a+6:a+8]), "hheaGap": s16(data[a+8:a+10])}
    if "OS/2" in tabs:
        o = tabs["OS/2"]
        m["fsSel"] = u16(data[o+62:o+64])
        m["typoAsc"] = s16(data[o+68:o+70]); m["typoDesc"] = s16(data[o+70:o+72])
        m["typoGap"] = s16(data[o+72:o+74])
        m["winAsc"] = u16(data[o+74:o+76]); m["winDesc"] = u16(data[o+76:o+78])
    return m

def em_ratios(m):
    upem = m["upem"]
    r = {"hhea": (m["hheaAsc"] - m["hheaDesc"]) / upem,
         "hhea+gap": (m["hheaAsc"] - m["hheaDesc"] + m["hheaGap"]) / upem}
    if "typoAsc" in m:
        r["typo"] = (m["typoAsc"] - m["typoDesc"]) / upem
        r["typo+gap"] = (m["typoAsc"] - m["typoDesc"] + m["typoGap"]) / upem
        r["win"] = (m["winAsc"] + m["winDesc"]) / upem
        r["USE_TYPO"] = bool(m.get("fsSel", 0) & 0x80)
    return r

def download(src, dest):
    """src is a URL or a local file path; copy/fetch into dest."""
    if os.path.exists(dest) and os.path.getsize(dest) > 0:
        return "cached"
    if os.path.exists(src):  # local system font → copy
        import shutil
        shutil.copyfile(src, dest)
        return f"{os.path.getsize(dest)//1024}KB (local)"
    req = urllib.request.Request(src, headers={"User-Agent": "parasma-debug-fonts"})
    with urllib.request.urlopen(req, timeout=30) as resp:
        data = resp.read()
    open(dest, "wb").write(data)
    return f"{len(data)//1024}KB"

def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("doc", nargs="?", help="docx/xlsx/pptx path")
    ap.add_argument("--families", help="comma-separated families instead of a doc")
    ap.add_argument("--pt", type=float, default=10.5, help="font size for the pitch column")
    ap.add_argument("--mult", type=float, default=1.2, help="lineRule auto multiplier (line/240)")
    ap.add_argument("--all-styles", action="store_true", help="also download bold/italic/boldItalic")
    ap.add_argument("--load", action="store_true", help="write a viewer loader manifest")
    ap.add_argument("--remap", action="append", default=[],
                    help='substitution test: render family A with face B, e.g. --remap "DejaVu Sans=Open Sans" (repeatable)')
    ap.add_argument("--system", action="store_true",
                    help="prefer locally-installed system fonts (the EXACT faces Word/the OS used)")
    ap.add_argument("--url", action="append", default=[],
                    help='explicit source for any font: --url "Verdana=https://...ttf" (repeatable)')
    ap.add_argument("--out", default=OUT)
    args = ap.parse_args()
    remaps = dict(r.split("=", 1) for r in args.remap)
    url_overrides = dict(u.split("=", 1) for u in args.url)

    urls = load_font_urls()
    if args.families:
        fams = [f.strip() for f in args.families.split(",") if f.strip()]
    elif args.doc:
        fams = extract_families(args.doc)
    else:
        ap.error("provide a doc path or --families")

    os.makedirs(args.out, exist_ok=True)
    styles = ["regular", "bold", "italic", "boldItalic"] if args.all_styles else ["regular"]

    print(f"families declared: {fams}\n")

    cache = {}
    def fetch_family(fam):
        """Download a family's faces → {style: localpath, _kind}. Source priority:
        explicit --url > --system (local) > FONT_URLS/clone. None if no source."""
        if fam in cache:
            return cache[fam]
        if fam in url_overrides:
            src_name, kind, srcs = fam, "url", {"regular": url_overrides[fam]}
        elif args.system and find_system_font(fam, "regular"):
            srcs = {st: p for st in styles if (p := find_system_font(fam, st))}
            src_name, kind = fam, "system"
        else:
            res = resolve(fam, urls)
            if not res:
                cache[fam] = None
                return None
            src_name, kind, srcs = res
        got = {"_kind": kind}
        for st in styles:
            src = srcs.get(st)
            if not src:
                continue
            ext = ".otf" if src.lower().endswith(".otf") else ".ttf"
            dest = os.path.join(args.out, f"{src_name.replace(' ', '')}-{st}{ext}")
            try:
                download(src, dest)
                got[st] = dest
            except Exception as e:
                print(f"  download {fam} {st} FAILED: {e}")
        cache[fam] = got
        return got

    for tgt in remaps.values():
        fetch_family(tgt.strip())

    hdr = (f"{'family':22s} {'src':16s} {'upem':>5s} {'hhea':>7s} {'h+gap':>7s} "
           f"{'typo':>7s} {'t+gap':>7s} {'win':>7s} {'TYPO':>4s} | pitch@{args.pt}pt×{args.mult}")
    print(hdr); print("-" * len(hdr))
    for fam in fams:
        got = fetch_family(fam)
        if not got:
            print(f"{fam:22s} {'(no source)':16s}  — add to CLONE map / FONT_URLS / --remap")
            continue
        reg = got.get("regular")
        if not reg:
            continue
        try:
            m = read_metrics(reg); r = em_ratios(m)
            pitch = r["hhea"] * args.pt * args.mult  # em-ratio × pt × multiplier
            g = lambda k: f"{r[k]:.4f}" if k in r else "  -  "
            typo = "yes" if r.get("USE_TYPO") else ("no" if "USE_TYPO" in r else "-")
            print(f"{fam:22s} {got['_kind']:16s} {m['upem']:5d} {g('hhea'):>7s} {g('hhea+gap'):>7s} "
                  f"{g('typo'):>7s} {g('typo+gap'):>7s} {g('win'):>7s} {typo:>4s} | hhea={pitch:.2f}pt")
        except Exception as e:
            print(f"{fam:22s} {got['_kind']:16s}  metrics FAILED: {e}")

    if remaps:
        print("\nremaps (render family A with B's face):")
        for a, b in remaps.items():
            print(f"  {a!r} → {b.strip()!r}")

    if args.load:
        # manifest maps the URL core fetches for family A → the LOCAL face to serve
        # (B's face when A is remapped, else A's own real face).
        manifest = []
        for fam in fams:
            res = resolve(fam, urls)  # core's ACTUAL fetch URL is the match key
            core = res[2] if res else {}
            serve = fetch_family(remaps.get(fam, fam).strip())
            if not serve:
                continue
            for st in styles:
                core_url = core.get(st)
                local = serve.get(st) or serve.get("regular")
                if core_url and local:
                    manifest.append({"family": fam, "style": st, "url": core_url, "path": local})
        mpath = os.path.join(args.out, "manifest.json")
        json.dump(manifest, open(mpath, "w"), indent=1)
        print(f"\nmanifest → {mpath} ({len(manifest)} entries)")
        print(f"load in viewer:  http://localhost:5173/?debugfonts={args.out}")
    print(f"\nfonts in {args.out}/  (gitignored — never commit)")

if __name__ == "__main__":
    main()
