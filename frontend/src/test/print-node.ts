// How a DOM node prints in a failed `expect` or a `console.log` (src/test/setup.ts installs it).
//
// Bun's `expect` prints the value it got by walking every property it can reach. From a happy-dom
// node that is the document, the window and, through React's fiber keys, the whole rendered app:
// on 2026-11-06 one failed `expect(node).toBeNull()` built a message of 130 to 730 million
// characters in 6 to 33 s, synchronously, so a `waitFor` around it could not even time out
// (Laysi/trustiot-v2#1138). A node prints as its markup instead, which is what a reader of the
// failure needs.

/** The most characters of a node's markup that are printed. */
export const NODE_PRINT_LIMIT = 2_000;

/** A DOM node as printed: an element's markup, any other node's name and text, cut at NODE_PRINT_LIMIT. */
export function printNode(node: Node): string {
  const markup = (node as Partial<Element>).outerHTML;
  const full = typeof markup === "string" ? markup : `${node.nodeName} ${JSON.stringify(node.textContent ?? "")}`;
  return full.length > NODE_PRINT_LIMIT
    ? `${full.slice(0, NODE_PRINT_LIMIT)}… (${full.length - NODE_PRINT_LIMIT} more characters)`
    : full;
}

/** Makes every node of the registered DOM print with `printNode`. */
export function installNodePrinter(): void {
  Object.defineProperty(Node.prototype, Bun.inspect.custom, {
    value(this: Node) {
      return printNode(this);
    },
    configurable: true,
  });
}
