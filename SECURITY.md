# Security policy

Parasma opens documents that its user did not write. A document is untrusted
input, and the project treats it that way.

## Reporting a vulnerability

Please report privately, not in a public issue. Use GitHub's private reporting:
the **Security** tab of this repository, then **Report a vulnerability**.

Include the smallest document that reproduces the problem. **Do not attach a
confidential or third-party document.** Reduce it to a synthetic file first, or
describe the structure that triggers the problem and we will build one.

Reports are acknowledged on a best-effort basis, normally within a week. There is no bug bounty.

## Supported versions

Only the latest release and the `main` branch receive fixes.

## What is in scope

- **Script or markup injection from document content**: anything in a document
  that reaches the host page as executable script, an unsanitised link target,
  or active clipboard HTML.
- **Escaping the viewer**: document content that reads or writes outside the
  viewer's own state, makes network requests the integrator did not configure,
  or leaks one document's content into another viewer instance.
- **Denial of service**: a small document that hangs the tab, exhausts memory,
  or aborts the engine. These are bugs and we want them, with one caveat below.

## What to expect from the design

- The engine is Rust compiled to WebAssembly and runs inside the browser's wasm
  sandbox. A memory-safety failure in the engine traps; it does not corrupt the
  host page.
- A package whose parts expand far beyond its compressed size is refused
  before parsing (the limits are in the README). The check counts the bytes
  actually inflated, not the sizes the zip directory declares.
- Parsing and layout are synchronous inside the engine, and the engine cannot
  be interrupted from outside. In **worker mode**, a worker that sends nothing
  for 30 seconds while a request is outstanding is terminated: that load fails
  with a "worker stalled" error and the next load starts a fresh worker. The
  limit is `workerStallTimeoutMs` in the core viewer config. Layout reports
  each finished page, so a long document is not mistaken for a stalled one.
- **In-process mode has no such protection.** A hostile document can hang or
  crash the tab that renders it. Use worker mode for documents from untrusted
  sources; `mountViewer` and `createHeadlessViewer` use it by default.
- The library makes no network requests for a document's content. Its only
  default third-party requests are font files; see the Privacy section of the
  README for how to self-host them.

## Out of scope

- Rendering differences from Microsoft Office. Those are fidelity bugs; open a
  normal issue.
- Problems that require the integrator to have disabled a documented safeguard.
