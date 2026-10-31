// Test support for the outer end of the protocol: a host window in happy-dom whose iframes load
// nothing, and a stand-in for a sandbox frame's window that records what the host posts to it
// and posts messages "from" it.
import { Window } from "happy-dom";

export interface FakeFrame {
  /** What the host posted to the frame, with the target origin it named. */
  sent: { message: unknown; targetOrigin: string }[];
  /** Delivers `data` to the host window as a message event from this frame (or `source`). */
  post(data: unknown, options?: { origin?: string; source?: object | null }): void;
  /** Fires the frame's `load` event, as a (re)load of its document would. */
  load(): void;
}

export interface HostWindow {
  window: Window;
  document: Document;
  /** Gives `frame` a stand-in window: happy-dom does not load frames here. */
  attach(frame: HTMLIFrameElement): FakeFrame;
  close(): Promise<void>;
}

export function openHostWindow(): HostWindow {
  const happy = new Window({
    url: "https://check.invalid/sandbox/check.html",
    settings: { disableIframePageLoading: true },
  });
  const document = happy.document as unknown as Document;
  return {
    window: happy,
    document,
    attach(frame) {
      const sent: FakeFrame["sent"] = [];
      const content = {
        postMessage(message: unknown, targetOrigin: string) {
          sent.push({ message, targetOrigin });
        },
      };
      Object.defineProperty(frame, "contentWindow", { value: content, configurable: true });
      return {
        sent,
        post(data, options = {}) {
          const source = options.source === undefined ? content : options.source;
          const event = new happy.MessageEvent("message", {
            data,
            origin: options.origin ?? "null",
            source: source as never,
          });
          happy.dispatchEvent(event);
        },
        load() {
          frame.dispatchEvent(new happy.Event("load") as unknown as Event);
        },
      };
    },
    close: async () => {
      await happy.happyDOM.abort();
      await happy.happyDOM.close();
    },
  };
}
