// The checker's entry to the comparison (`compare.js`, frontend/sandbox/README.md, 「比對程式」).
// The checking service injects compare.js into an isolated world of the sandbox frame: the
// document is the page's, the JavaScript globals and prototypes are the isolated world's own, so
// nothing the page changed in its realm reaches this code. Its input comes only from the caller.
import { compareDocument, verdictOf } from "./compare.ts";
import { captureDom, type DomWindow } from "./dom.ts";
import { type CheckVerdict, CompareInputSchema } from "./protocol.ts";

/** `tqCompare(input)`: the verdict on the document as it is now, against `input`'s data. */
export function createCompare(win: DomWindow): (input: unknown) => CheckVerdict {
  const dom = captureDom(win);
  return (input) => {
    const parsed = CompareInputSchema.parse(input);
    // The checker runs in a browser, which lays the document out (compareDocument's `layout`).
    return verdictOf(
      compareDocument(dom, { spec: parsed.spec, results: parsed.datasets, tz: parsed.tz }, parsed.locale, true),
    );
  };
}
