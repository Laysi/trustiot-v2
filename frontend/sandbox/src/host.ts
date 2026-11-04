// The host adapter (frontend/sandbox/README.md, 「postMessage 協定」): the outer end of the
// protocol. It creates the sandbox frame, sends `init` and `data`, and hands its owner only the
// sandbox messages that come from that frame's window and parse as exactly one of the protocol's
// messages. The SPA's custom page (frontend/src/dashboard/custom-page) and the checker's host page
// (check-host.ts) both use it.
//
// Every window message from the sandbox is the page's: the page runs in the same realm as the
// runtime and can post anything the runtime can. The adapter therefore only filters and parses;
// what a message means is up to its owner, who must check a `source` reference against the data it
// holds and show an `error` message only as the page's own text.
//
// In display mode (`mode: "view"`) the adapter also opens a MessageChannel and hands one end to the
// runtime with `init`; the runtime keeps it out of the page's reach. Reports on that port
// (`onReport`) are what the owner may base showing the frame on (README, 「顯示時比對的強制」).
// After `init` the adapter stops listening to the window altogether in that mode: nothing the page
// posts there reaches the owner, and a flood of it costs the host page nothing.
import {
  DataMessageSchema,
  type DatasetResult,
  InitMessageSchema,
  type PortMessage,
  PROTOCOL,
  type PublicSpec,
  parsePortMessage,
  parseSandboxMessage,
  RUNTIMES,
  type SandboxMessage,
} from "./protocol.ts";

type InitMessage = ReturnType<typeof InitMessageSchema.parse>;

/** A dataset for a `data` message: its result, or the message key of why it has none. */
export type DeliveredDataset =
  | { status: "ok"; result: DatasetResult }
  | { status: "error"; dataset: string; message_key: string };

export interface SandboxHostOptions {
  /** The element the frame is appended to. */
  container: Element;
  /** The runtime version the dashboard version was checked with. */
  runtime: (typeof RUNTIMES)[number];
  /**
   * The URL of the directory that holds the version directories, such as `/sandbox` (the SPA)
   * or `.` (check.html, which is in that directory); the frame loads `<base>/v<runtime>/sandbox.html`.
   */
  base: string;
  /** The frame's accessible name. */
  title: string;
  /**
   * Every window message from the frame that parses, in order; in display mode only those before
   * `init` (that is, `ready`).
   */
  onMessage(message: SandboxMessage): void;
  /** Display mode: every report on the runtime's port that parses, in order. Required for that mode. */
  onReport?(message: PortMessage): void;
  /** The frame left the sandbox document: it navigated or reloaded. Nothing more is sent or delivered. */
  onNavigate?(): void;
}

type HostState = "loading" | "ready" | "gone";

/** One sandbox frame and its end of the protocol. */
export class SandboxHost {
  readonly frame: HTMLIFrameElement;
  private state: HostState = "loading";
  private loads = 0;
  private initSent = false;
  private pendingInit: InitMessage | null = null;
  /** Display mode: the host's end of the runtime's port, and the end that goes with `init`. */
  private port: MessagePort | null = null;
  private transfer: MessagePort | null = null;
  private pendingData: unknown = null;
  private generation = 0;
  private readonly window: Window;
  private readonly listener: (event: MessageEvent) => void;
  private readonly onLoad: () => void;

  constructor(private readonly options: SandboxHostOptions) {
    if (!RUNTIMES.includes(options.runtime)) throw new Error(`runtime ${options.runtime} is not in this build`);
    const doc = options.container.ownerDocument;
    const view = doc.defaultView;
    if (view === null) throw new Error("the container is not in a window");
    this.window = view;
    const frame = doc.createElement("iframe");
    // The sandbox attribute has to be in place before the frame starts loading.
    frame.setAttribute("sandbox", "allow-scripts");
    frame.setAttribute("referrerpolicy", "no-referrer");
    frame.setAttribute("title", options.title);
    frame.setAttribute("src", `${options.base}/v${options.runtime}/sandbox.html`);
    this.frame = frame;
    this.listener = (event) => this.handle(event);
    this.onLoad = () => {
      this.loads++;
      if (this.loads > 1) this.leave();
    };
    view.addEventListener("message", this.listener);
    frame.addEventListener("load", this.onLoad);
    options.container.appendChild(frame);
  }

  /** Sends `init` once, when the runtime is ready. The spec is the public part (`publicSpec`). */
  init(message: {
    page: string;
    spec: PublicSpec;
    locale: InitMessage["locale"];
    tz: string;
    theme: InitMessage["theme"];
    mode: InitMessage["mode"];
  }): void {
    if (this.initSent || this.pendingInit !== null) throw new Error("init is sent once");
    // Parsing drops anything the protocol does not carry, such as a full spec's queries.
    const parsed = InitMessageSchema.parse({
      tq: PROTOCOL,
      type: "init",
      runtime: this.options.runtime,
      ...message,
    });
    if (parsed.mode === "view") {
      const onReport = this.options.onReport;
      if (onReport === undefined) throw new Error("display mode needs onReport");
      const Channel = (this.window as unknown as { MessageChannel: typeof MessageChannel }).MessageChannel;
      const channel = new Channel();
      channel.port1.onmessage = (event: MessageEvent) => {
        if (this.state === "gone") return;
        const report = parsePortMessage(event.data);
        if (report !== null) onReport(report);
      };
      this.port = channel.port1;
      this.transfer = channel.port2;
    }
    this.pendingInit = parsed;
    this.flush();
  }

  /** Sends every dataset's result or failure; each call is a new generation and draws again. */
  data(datasets: readonly DeliveredDataset[]): number {
    this.generation++;
    this.pendingData = DataMessageSchema.parse({ tq: PROTOCOL, type: "data", generation: this.generation, datasets });
    this.flush();
    return this.generation;
  }

  /** Removes the frame and stops listening. */
  destroy(): void {
    this.state = "gone";
    this.window.removeEventListener("message", this.listener);
    this.frame.removeEventListener("load", this.onLoad);
    this.closePort();
    this.frame.remove();
  }

  private closePort(): void {
    if (this.port !== null) {
      this.port.onmessage = null;
      this.port.close();
    }
    this.transfer?.close();
    this.port = null;
    this.transfer = null;
  }

  private flush(): void {
    if (this.state !== "ready") return;
    if (this.pendingInit !== null) {
      const transfer = this.transfer;
      this.send(this.pendingInit, transfer === null ? [] : [transfer]);
      this.transfer = null;
      if (this.pendingInit.mode === "view") this.window.removeEventListener("message", this.listener);
      this.pendingInit = null;
      this.initSent = true;
    }
    if (this.initSent && this.pendingData !== null) {
      this.send(this.pendingData);
      this.pendingData = null;
    }
  }

  private send(message: unknown, transfer: Transferable[] = []): void {
    // The sandbox's origin is opaque: there is no origin to name. The frame can only hold the
    // sandbox document or nothing (the embedder's CSP frame-src), and the adapter stops at the
    // first sign of a navigation.
    this.frame.contentWindow?.postMessage(message, "*", transfer);
  }

  private leave(): void {
    if (this.state === "gone") return;
    this.state = "gone";
    this.closePort();
    this.options.onNavigate?.();
  }

  private handle(event: MessageEvent): void {
    if (this.state === "gone") return;
    if (event.source === null || event.source !== this.frame.contentWindow || event.origin !== "null") return;
    const message = parseSandboxMessage(event.data);
    if (message === null) return;
    if (message.type === "ready") {
      // A second ready is a second runtime: the frame reloaded or navigated to another copy.
      if (this.state !== "loading") {
        this.leave();
        return;
      }
      this.state = "ready";
      this.options.onMessage(message);
      this.flush();
      return;
    }
    if (this.state !== "ready" || !this.initSent) return;
    if ((message.type === "rendered" || message.type === "returned") && message.generation > this.generation) return;
    this.options.onMessage(message);
  }
}
