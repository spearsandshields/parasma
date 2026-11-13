# Contributing to Parasma

Thanks for taking an interest. Taking part means following the
[code of conduct](CODE_OF_CONDUCT.md).

## What is welcome right now

Bug reports, reduced reproductions, rendering comparisons against Word, Excel
or PowerPoint, and design suggestions. Open an issue.

Pull requests are not merged at the moment. Fixes are written within the
project so that the code base keeps a single copyright holder; this keeps every
licensing option open. If an issue comes with a clear
reproduction, that is the fastest route to a fix. This may change later.

## Working on the code yourself

Run the gate. It must print `GATE PASSED`:

```bash
bun install
bun run build:wasm:full   # once, and again after engine changes
bun run ci
```

The gate runs the layering rules, `clippy -D warnings`, the native tests, the
TypeScript type-checks and the TypeScript tests, all at the feature set that
ships. The conventions below are what the tree follows; a fork that keeps
them stays easy to compare against upstream.

## Conventions

- **Conventional commits with a body that explains why.** The diff shows what
  changed; the message is for the reason.
- **Spec citations are the docstring.** A rule derived from ECMA-376 cites its
  section (`§17.3.1.33`). The standard itself is not in this repository; it is
  free to download from Ecma International.
- **Declare deviations.** If the implementation departs from the standard, say
  so at the site, with the section it departs from.
- **Format on touch.** The tree is not rustfmt-clean and is not bulk-formatted.
  Run `cargo fmt` only on files you are already rewriting.
- **Fixtures are synthetic.** Tests build their documents inline from XML.

## Never attach or name a real document

Do not attach, commit, quote from, or name a document you did not create and do
not have the right to publish, in an issue, a pull request, a test or a
comment. If a real document shows a bug, reduce it to a synthetic file that
shows the same bug. If you cannot, describe the structure that triggers it.

## Licensing

Parasma is licensed under the Apache License 2.0. Anything you post in an
issue, such as a reproduction or a suggested change, is taken as offered under
the same terms, as section 5 of that licence provides. Only post what you have
the right to share.

## Security

Report vulnerabilities privately; see [SECURITY.md](SECURITY.md).
