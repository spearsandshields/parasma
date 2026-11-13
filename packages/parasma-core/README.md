# @parasma/core

The engine layer of the [Parasma](https://github.com/spearsandshields/parasma) document viewer: the WebAssembly
layout and paint engine, font resolution, image transfer, and the
off-main-thread render worker. It is installed as a dependency of
[`@parasma/snippet`](https://github.com/spearsandshields/parasma/tree/main/packages/snippet), which is what most
applications use directly.

```ts
import { Viewer, sniffKind, supportedFormats } from "@parasma/core";
```

The package ships a WebAssembly binary (`dist/wasm/`) fetched at runtime via
`import.meta.url`, so the consuming app's bundler must serve it as a static
asset; production builds with Vite, webpack, Rollup and esbuild do. The
render worker ships as `dist/worker.js` and is started the same way. To host
the binary and fonts yourself, set `assetBaseUrl`.

**Vite:** add `optimizeDeps: { exclude: ["@parasma/core"] }` to your Vite
config. The dev server pre-bundles dependencies into a cache folder where the
engine can no longer find its WebAssembly file. Production builds need no
configuration.

Third-party licences for the code compiled into the binary are listed in
`THIRD-PARTY-LICENSES.md` inside the package. More: the
[repository README](https://github.com/spearsandshields/parasma#readme).
