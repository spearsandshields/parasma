# Memory architecture — constraints, layers, and the debts we chose

Status: living document, created 2026-07-28 out of a memory investigation and
the layout backpressure work. This is the ledger of *deliberate* memory
decisions so they read as choices with reasons, not accidents.

## The physics we live under

- **wasm32 linear memory has a hard 4 GiB ceiling** (65,536 × 64 KiB pages).
  This is architectural, not configurable. Growth beyond it is impossible;
  when `memory.grow` fails, Rust's allocator aborts and the instance is dead.
- **Memory only ever grows.** `memory.grow` has no inverse, so the browser
  sees the session's PEAK as resident forever. Freed Rust values ARE reused
  internally (dlmalloc, not GC) — the peak matters for hitting the ceiling,
  not as ongoing waste. The only true release is discarding the instance.
- **The browser can kill the whole tab before wasm hits any limit.** Its OOM
  judgment counts the entire tab (JS heap, canvases, decoded images, GPU,
  wasm). Nothing inside wasm can prevent that; the layers below only shrink
  our contribution to it.
- **Measured cost of our layout: ~2.31 MB retained per page** (13→1,063-page
  fit, rsd 0.3–1.4%). Derived ceilings: ~1,850 pages at the 4 GiB wall,
  ~925 at a 2 GB practical cap. Layout is a COPY of the document, not a view
  over it — items hold their text and glyph geometry. That is a deliberate
  trade (see "Why pages are fat").

## Why wasm32 and not wasm64 (memory64) — revisit trigger included

memory64 shipped in Chrome 133 / Firefox 134 (early 2025). We stay on wasm32
for four reasons, one of which is a performance fact worth recording:

1. **The guard-page trick.** On wasm32, engines skip per-access bounds checks
   entirely: they reserve 4 GiB of virtual address space plus guard pages, and
   an out-of-bounds access becomes a hardware fault. memory64 breaks this —
   every memory access gains an explicit bounds check, with measured
   low-double-digit-percent slowdowns. wasm32 is not just a size choice; it is
   where our paint/layout speed partly comes from.
2. Safari has not shipped memory64; an embeddable component cannot require
   the one missing engine, and dual artifacts double the build/golden matrix.
3. `wasm64-unknown-unknown` is Rust Tier 3 and wasm-bindgen support is
   experimental — our shipping pipeline has no supported path.
4. Windowed retention (below) makes the wall stop mattering: windowed retention makes
   memory O(viewport), and 4 GiB of address space is then plenty.

**Revisit when BOTH hold: Safari ships memory64 AND Rust/wasm-bindgen support
it as a supported tier.** Even then, weigh the bounds-check tax against what
windowed retention has already bought.

## The four layers (outermost failure first)

| Layer | Role | State |
|---|---|---|
| **Windowed retention** | *The solve.* Evict laid-out pages outside the viewport; re-lay from the layout's per-page checkpoints on demand (the Word architecture). Memory becomes O(window). | Direction decided, parked; gated on the census measurements below |
| **Soft budget** | *The brakes.* Layout checks total linear memory at the three page-boundary yield points; over budget → stop gracefully, keep pages laid so far, stamp `layoutTruncatedAt` ("showing first N pages"). Default 2 GiB, provisional pending browser measurement; `setMemoryBudget` overrides, 0 disables. | Shipped |
| **Instance recreation** | *The reset.* The only mechanism that returns memory. Worker mode: on document switch, a worker whose linear memory ≥ 1 GiB is disposed and rebuilt via a recreate recipe (font bytes re-push; `#sessionFaces` skips the re-decode). Also fixes the stale-peak interaction. **In-process has no equivalent** — module-level memory is shared by every handle on the page; only a page-level teardown returns it. That asymmetry is one more reason worker mode is the eventual default. | Shipped (worker mode) |
| **Decompression fence** | *The door.* Before any parse, every loader meters each package part's real inflated size (`src/package_limits.rs`): refused past 100× its compressed size once over 4 MiB, or past 1 GiB in total. The total stays inside the headroom below. Sits outside the ordering contract: it rejects input rather than containing work. | Shipped |
| **Containment cap** | *The airbag — DECLINED.* A below-wall `--max-memory` was built, verified, and dropped: the wall traps identically without it; a cap only buys deterministic, attributable failure. Judged not worth a build knob. Revisit if wild OOMs prove hard to attribute. | Declined by choice |

The ordering is the contract: **if an inner layer fires, the layer above it
failed.** An instance death at the wall in the wild is a bug report against
the budget or the recreation logic, not a platform fact to shrug at.

### The budget-headroom invariant

Documented at `setMemoryBudget`: keep **budget ≥ ~1.5 GiB below the 4 GiB
wall**. The margin is legitimate transient headroom (document bytes, unzip
staging, image bytes before `clearImageBytes` frees them) that must fit
above where layout stops. An embedder raising the budget toward ~2.5 GiB is
spending that margin and should know it.

## Why pages are fat (and why we don't fight it head-on)

Canvas-native paint retains glyph-level geometry per page — that is where
paint speed comes from, and it forecloses the thin-node strategy (full
materialized layout of *thin* nodes). Instead of thinning pages, windowed
retention stops *retaining* them. **A memory census (2026-07-29) measured what
gates that plan** — debug build, ratios are the finding:

| fixture | pages | pages walked | glyphs | text (dup) | ckpt med/max | transient (caches) |
|---|---|---|---|---|---|---|
| text-2k | 267 | 129.6 MB | 29.1 | 0.9 (0.9) | 0.8 / 0.8 KB | 150.6 MB |
| text-10k | 1,334 | 648.1 MB | 145.3 | 4.5 (4.5) | 0.8 / 0.8 KB | 756.0 MB |
| text-45k | 6,000 | 2,916.6 MB | 653.7 | 20.4 (20.4) | 0.8 / 0.8 KB | 3,398.1 MB |

Per-page cost is constant: **0.486 MB walked** across a 22× range. Findings:

- **Checkpoints are free** (0.8 KB median and max) — eviction bookkeeping
  costs nothing. Untested class: a giant single paragraph would fatten the
  ParaItems tail.
- **Caches ≈ pages** — evicting pages alone buys ~2×; windowed retention must window
  `block_caches` too (the browser's 529 MB-before-page-1 datapoint agrees).
- **Text interning is not worth it** (0.7% of page bytes) — declined on its
  own pre-agreed criterion.
- **~77% of page bytes is fixed struct overhead**: `size_of::<Item>` =
  624 B (RunStyle 184 B inline, seven cold `Option`s inline) vs ~5 B mean
  text payload. Glyphs are 22%. The intra-page lever is slimming `Item`,
  worth potentially 2–3× on top of windowing.

Instruments: the cfg(test) counting global allocator (`alloc_census`,
lib.rs) + payload walkers (`memory_census`, oracle_tests.rs) — both
`#[ignore]`d, run alone, never in the gate. The raw `retained` column
double-counts pages under cfg(test) (`push_records` clones every page);
discount it when reading raw output.

## Known blind spots and debts (the honest register)

- **JS-side font memory is invisible to the wasm budget.** Decoded FontFaces
  (`#sessionFaces`) persist for the viewer's lifetime with no eviction below
  `destroy()`; bounded by the font catalog (only CJK/Arabic families are
  heavy), so hygiene rather than a leak — but it pressures the same tab the
  budget defends and neither system sees the other. Deferred whole to a future
  font-strategy design.
- **The budget only samples at page boundaries during WML layout.** A single
  huge allocation between checks (e.g. a giant embedded image) sails past it
  straight toward the wall — and with the cap declined, nothing else stands
  in between. SML/PML have no budget check, RECORDED as acceptable
  (code-verified): the grid default is chunk-LRU-bounded; SML pages view
  materializes on demand and grows only as far as the user scrolls; PML
  materializes the whole deck at parse, before any loop a check could live
  in. Re-open on evidence, not speculation.
- **An OOM trap does not fire `enginePanic`.** `handle_alloc_error` bypasses
  the panic hook, so an allocation-failure death surfaces as a `RuntimeError`
  at the JS call site, not the formatted terminal event. Routing it into the
  same path is tied to the instance-recreation work.
- ~~The real truncation path has never fired in a browser~~ — VERIFIED LIVE
  2026-07-29: the `memory_size` arm fired against a 64 MB budget on
  a 1,324-page document (stamp `Some(1)`, clean stop, instance alive), and
  the same document completes untruncated under the 2 GiB default (renderer
  RSS 1.31 GB). Bonus measurement: linear memory read **529 MB at the first
  page boundary** — parse + the up-front cache build dominate before any
  page is retained, pre-confirming the hypothesis that `block_caches`
  dominate, which the census above quantifies.
- **Truncated views skip the post-layout passes** (declared at the yield
  sites): a NUMPAGES field shows its cached value in a truncated view.
