import { createApp } from "vue";
import { MiniViewer } from "../examples/mini-viewer";

const app = createApp(MiniViewer);
const vm = app.mount("#app");
// Dev hook (demo only): the exposed composable, for console/harness probing.
(window as unknown as { __mini: unknown }).__mini = vm;
