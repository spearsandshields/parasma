# Calibration document IDs

Some layout rules in this engine were derived by opening a **real document in
Word** and measuring what Word actually did — page counts, line positions,
whether a fixed-height row grows. Those observations are the only ground truth
in the project: every WML/SML fixture in the test suite is synthetic, built
inline from XML string literals, so the suite otherwise measures engine
self-consistency rather than agreement with Word.

Comments that record such an observation cite the document by an **opaque ID**:

```rust
// push it down/out (verified against Word on calibration doc C1: a
// NormalWeb title with 1em auto-before sits at the same level as the plain
// date cells in an hRule=exact header row, not pushed below them).
```

## Why IDs and not filenames

Calibration documents are not distributed with the repository. An ID keeps the
engineering content fully in-tree — the measurement, the Word value, and the
rule it justifies — without depending on a document the reader does not have.

## The mapping

`ID → filename, sha256` lives in a local, gitignored
`.calibration/manifest.private.json`. It is a binding table only — the Word
measurements themselves live in the tracked ground-truth manifest
(`crates/parasma-wasm/src/ground_truth.rs`), which is keyed by ID. Without a
local manifest every ID reports as unresolvable, which the harness treats as
absent rather than as a failure.

The documents themselves are not in git either — they live under `fixtures/`,
which is gitignored in full.

## Rules

- **Never** put a calibration document's filename in tracked source, docs, commit
  messages, or test names. Use its ID.
- A **new** calibration document gets the next free ID and an entry in the private
  manifest at the same time. An ID with no manifest entry is worse than a
  filename, because it looks resolvable and is not.
- Public sample documents (e.g. `file-sample_1MB.docx`, from file-examples.com)
  are **not** covered by this and may be named directly. The manifest records them
  too, marked `"private": false`, so the distinction is deliberate rather than
  forgotten.
- A calibration document's filename in tracked content is a defect. Replace it
  with its ID.
