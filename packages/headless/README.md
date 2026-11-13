# @parasma/headless

The framework-free plugin kernel of the [Parasma](https://github.com/spearsandshields/parasma) document viewer: the
plugin registry, event bus, state store and teardown contract that
[`@parasma/snippet`](https://github.com/spearsandshields/parasma/tree/main/packages/snippet) (or your own UI)
drives. No DOM and no UI framework. It is installed as a dependency of
`@parasma/snippet`, which is what most applications use directly.

```ts
import { createKernel } from "@parasma/headless";
import { /* low-level internals */ } from "@parasma/headless/internal";
```

Commands and events: [docs/COMMANDS.md](https://github.com/spearsandshields/parasma/blob/main/packages/headless/docs/COMMANDS.md).
More: the [repository README](https://github.com/spearsandshields/parasma#readme).
