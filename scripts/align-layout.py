#!/usr/bin/env python3
"""align-layout — document-order alignment of Parasma's debug-probe layout against
Word's PDF ground truth (pdftotext -bbox). Robust to DUPLICATE content and shifted
page breaks (naive per-page text-prefix matching fails on both): uses difflib
sequence alignment on the line texts, computes per-matched-line GLOBAL-Y delta
(page*pageHeight + y), and reports the GAPS — lines present in one render but not
the other = missing/extra elements (e.g. a dropped chart).

Inputs (JSON):
  word    : [[page, idx, yTop_pt, x0_pt, text], ...]   (pdftotext -bbox, → /tmp/word_lines.json)
  parasma : [{page, y(px@96dpi), h, preview}, ...]      (window.__pv.debugLayout per page)

Usage: scripts/align-layout.py /tmp/word_lines.json /tmp/parasma_fix.json [--full] [--page-h 841.89]
"""
import sys, json, re, difflib, argparse

def norm(s): return re.sub(r"[^a-z0-9]", "", s.lower())[:24]

def load_word(path, ph):
    out = []
    for pg, idx, y, x, t in json.load(open(path)):
        if not t.strip(): continue
        out.append({"gy": (pg - 1) * ph + y, "text": t, "page": pg, "key": norm(t)})
    out.sort(key=lambda r: r["gy"])
    return out

def load_parasma(path, ph):
    out = []
    for r in json.load(open(path)):
        t = (r.get("preview") or "").strip()
        if not t: continue
        out.append({"gy": (r["page"] - 1) * ph + r["y"] * 0.75, "text": t, "page": r["page"], "key": norm(t)})
    out.sort(key=lambda r: r["gy"])
    return out

def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("word"); ap.add_argument("parasma")
    ap.add_argument("--full", action="store_true", help="print every matched line's delta")
    ap.add_argument("--page-h", type=float, default=841.89, help="page height in pt (A4 default)")
    args = ap.parse_args()

    W = load_word(args.word, args.page_h)
    P = load_parasma(args.parasma, args.page_h)
    sm = difflib.SequenceMatcher(None, [r["key"] for r in W], [r["key"] for r in P], autojunk=False)
    matched, miss, extra = [], [], []
    for tag, i1, i2, j1, j2 in sm.get_opcodes():
        if tag == "equal":
            for k in range(i2 - i1):
                w, p = W[i1 + k], P[j1 + k]
                matched.append((w, p, p["gy"] - w["gy"]))
        else:
            miss += W[i1:i2]
            extra += P[j1:j2]

    print(f"matched {len(matched)}/{len(W)} word lines | {len(miss)} missing in Parasma | "
          f"{len(extra)} extra in Parasma")
    print(f"\n{'pg':>2} {'n':>3} {'delta range (pt)':>20} {'median':>7}  drift(last−first)")
    for pg in range(1, max(r["page"] for r in W) + 1):
        rows = [d for (w, _, d) in matched if w["page"] == pg]
        if not rows:
            print(f"{pg:>2}  (none matched)"); continue
        sd = sorted(rows)
        print(f"{pg:>2} {len(rows):>3} {min(rows):>9.2f}..{max(rows):<9.2f} {sd[len(sd)//2]:>7.2f}  {rows[-1]-rows[0]:+.2f}")

    if miss:
        print(f"\n=== MISSING in Parasma (Word renders, Parasma doesn't) ===")
        for r in miss[:10]:
            print(f"  p{r['page']} gy={r['gy']:7.0f}  {r['text'][:52]!r}")
    if extra:
        print(f"\n=== EXTRA in Parasma (Parasma renders, Word doesn't) ===")
        for r in extra[:10]:
            print(f"  p{r['page']} gy={r['gy']:7.0f}  {r['text'][:52]!r}")
    if args.full:
        print("\n=== every matched line (global-Y delta, document order) ===")
        for w, p, d in matched:
            print(f"  W:p{w['page']}→P:p{p['page']}  Δ={d:+8.2f}  {w['text'][:42]}")

if __name__ == "__main__":
    main()
