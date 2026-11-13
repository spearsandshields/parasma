/** Tiny DOM helper — keeps the chrome builders declarative without a framework. */
export function el<K extends keyof HTMLElementTagNameMap>(
  tag: K,
  attrs: Partial<Record<string, string>> & { class?: string } = {},
  children: (Node | string)[] = [],
): HTMLElementTagNameMap[K] {
  const node = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs)) {
    if (v == null) continue;
    if (k === "class") node.className = v;
    else node.setAttribute(k, v);
  }
  for (const c of children) {
    node.append(typeof c === "string" ? document.createTextNode(c) : c);
  }
  return node;
}

/** Brief accent flash on a live-value element when its text changes. */
export function flash(node: HTMLElement): void {
  node.classList.remove("ps-flash");
  // Force reflow so re-adding the class restarts the animation.
  void node.offsetWidth;
  node.classList.add("ps-flash");
}
