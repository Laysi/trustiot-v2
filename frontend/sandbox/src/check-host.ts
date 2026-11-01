// The checker's host page (`/sandbox/check.html` with `check.js`; frontend/sandbox/README.md,
// 「檢查服務的流程」). The checking service drives it over the DevTools protocol: it calls
// `tqCheckHost.start(request)`, and the page reports through the binding the service adds with
// `Runtime.addBinding`. The page sits in the trusted layer: the sandbox frame it creates has an
// opaque origin and cannot reach it. What the sandbox sends is still the page's, so the host page
// reports only that something happened, and the error text only as a problem may carry it.
import { SandboxHost } from "./host.ts";
import { CheckRequestSchema, MAX_FRAME_HEIGHT, type SandboxMessage } from "./protocol.ts";
import { visibleMessage } from "./text.ts";

/** The binding the checking service adds (`Runtime.addBinding`); it receives each report as JSON. */
export const REPORT_BINDING = "tqCheckReport";

/** The frame's size before the page reports its height, in CSS pixels. */
export const FRAME_WIDTH = 1200;
export const FRAME_HEIGHT = 800;

/**
 * What the host page reports, each kind at most once: the runtime is ready (the host then sends
 * `init` and `data`), `render` returned for the first generation, the first generation finished
 * drawing, the first error. `returned` is what tells a page that never finished its `render`
 * (`not_rendered`) from one that did and kept changing (`timeout`). The sandbox can forge any of
 * them, so the checking service still confirms the quiet itself.
 */
export type CheckReport =
  | { type: "ready" }
  | { type: "returned" }
  | { type: "rendered" }
  | { type: "error"; message?: string };

export interface CheckHost {
  /** Starts the check of `request` (a `CheckRequest`); once per page load. Throws on anything else. */
  start(request: unknown): void;
}

export function createCheckHost(win: Window): CheckHost {
  let host: SandboxHost | null = null;
  const sent = new Set<CheckReport["type"]>();
  const report = (entry: CheckReport) => {
    if (sent.has(entry.type)) return;
    sent.add(entry.type);
    const binding = (win as unknown as Record<string, unknown>)[REPORT_BINDING];
    if (typeof binding === "function") binding(JSON.stringify(entry));
  };
  return Object.freeze({
    start(input: unknown) {
      if (host !== null) throw new Error("tqCheckHost.start runs once per page load");
      const request = CheckRequestSchema.parse(input);
      const created = new SandboxHost({
        container: win.document.body,
        runtime: request.runtime,
        base: ".",
        title: "dashboard",
        onMessage: (message: SandboxMessage) => {
          switch (message.type) {
            case "ready":
              report({ type: "ready" });
              created.init({
                page: request.page,
                spec: request.spec,
                locale: request.locale,
                tz: request.tz,
                theme: "light",
                mode: "check",
              });
              created.data(request.datasets.map((result) => ({ status: "ok" as const, result })));
              break;
            case "returned":
              report({ type: "returned" });
              break;
            case "rendered":
              report({ type: "rendered" });
              break;
            case "error": {
              const text = message.message === undefined ? undefined : visibleMessage(message.message);
              report(text === undefined ? { type: "error" } : { type: "error", message: text });
              break;
            }
            case "resize":
              created.frame.height = String(Math.min(MAX_FRAME_HEIGHT, Math.max(FRAME_HEIGHT, message.height)));
              break;
            case "source":
              break;
          }
        },
      });
      created.frame.width = String(FRAME_WIDTH);
      created.frame.height = String(FRAME_HEIGHT);
      host = created;
    },
  });
}
