/**
 * The acceptance artifact: a complete BYO-chrome office viewer in ~60 lines of
 * GENERIC integration code, demonstrating the integration cost. Every shape
 * here is a generic pattern: app-owned
 * toolbar over `p.commands`, SIBLING chrome (a formula bar beside the stage —
 * the layout provide/inject can't serve), search via kernel commands with the
 * default reveal, live theme, truncation notice. Plain markup on purpose —
 * smallness is the claim under test.
 */
import { defineComponent, h, ref, watch } from "vue";
import { useParasma } from "../src/use-parasma";

export const MiniViewer = defineComponent({
  name: "MiniViewer",
  setup(_, { expose }) {
    const stage = ref<HTMLElement | null>(null);
    const p = useParasma(stage, { theme: "auto" });
    expose(p); // parents / template refs get the whole surface
    const query = ref("");
    watch(query, (q) => p.commands.search(q));

    async function openFile(e: Event) {
      const f = (e.target as HTMLInputElement).files?.[0];
      if (f) await p.load(new Uint8Array(await f.arrayBuffer()), f.name);
    }

    return () =>
      h("div", { class: "mini-viewer" }, [
        h("div", { class: "mini-toolbar" }, [
          h("input", { type: "file", onChange: openFile }),
          // Spreadsheets: sheet tabs (the grid view is continuous — pages
          // don't exist there). Everything else: page navigation.
          ...(p.documentKind.value === "excel"
            ? p.sheets.value.map((s, i) =>
                h(
                  "button",
                  { class: { "mini-tab": true, active: i === p.activeSheet.value }, onClick: () => p.commands.selectSheet(i) },
                  s.name,
                ),
              )
            : [
                h("button", { onClick: () => p.commands.goToPage(p.currentPage.value - 1) }, "‹"),
                h("span", {}, `${p.currentPage.value} / ${p.pageCount.value}`),
                h("button", { onClick: () => p.commands.goToPage(p.currentPage.value + 1) }, "›"),
                h("button", { disabled: !!p.printProgress.value, onClick: () => void p.commands.print().catch(() => {}) }, "🖨"),
              ]),
          h("button", { onClick: () => p.commands.zoomOut() }, "−"),
          h("button", { onClick: () => p.commands.zoomIn() }, "+"),
          h("button", { onClick: () => p.commands.fitWidth() }, "fit"),
          h("button", { onClick: () => p.commands.toggleFullscreen() }, "⛶"),
          h("button", { class: { active: p.tool.value === "hand" }, onClick: () => p.commands.toggleTool() }, "✋"),
          h("input", {
            value: query.value,
            placeholder: "search",
            onInput: (e: Event) => (query.value = (e.target as HTMLInputElement).value),
          }),
          ...(p.search.value.query
            ? [
                h("span", {}, `${(p.search.value.activeIdx ?? -1) + 1} / ${p.search.value.total}`),
                h("button", { onClick: () => p.commands.searchPrev() }, "↑"),
                h("button", { onClick: () => p.commands.searchNext() }, "↓"),
              ]
            : []),
          p.status.value === "loading" ? h("span", {}, "loading…") : null,
        ]),
        p.documentKind.value === "excel"
          ? h("div", { class: "mini-formula-bar" }, [
              h("b", {}, p.cell.value?.ref ?? ""),
              h("span", {}, p.cell.value?.display ?? ""),
            ])
          : null,
        p.truncatedAt.value !== null
          ? h("div", { class: "mini-notice" }, `showing first ${p.truncatedAt.value} pages`)
          : null,
        p.status.value === "error" ? h("div", { class: "mini-error" }, String(p.error.value)) : null,
        // Empty state as an OVERLAY SIBLING — never as a child of the stage:
        // the stage ref is the engine's mount root, and Vue reconciling
        // children inside it clobbers the engine's injected DOM on the next
        // re-render (measured: a loaded document went blank).
        h("div", { class: "mini-stage-wrap" }, [
          p.status.value === "idle"
            ? h("div", { class: "mini-empty" }, "Open a document to preview it here")
            : null,
          h("div", { ref: stage, class: "parasma-snippet mini-stage" }),
        ]),
      ]);
  },
});
