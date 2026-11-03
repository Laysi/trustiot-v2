/**
 * Measures how long the sign-in page and the signed-in pages take to load through the real web
 * service (services/web: its security headers and cache policy) under a throttled network, with a
 * cold and a warm HTTP cache (Laysi/trustiot-v2#948; the method and the numbers are in
 * docs/test-reports/2026-11-01-948-bundle-size-load-time.md). It needs no dependency: it speaks
 * the Chrome DevTools Protocol over Bun's WebSocket to a headless Chromium that it starts itself.
 *
 * What it starts, and stops again by the process it started:
 * - the web service executable, on two free loopback ports, serving `--site` (and a second one serving
 *   `--upgrade-site`, when given);
 * - an ingress stand-in on a free loopback port: `/api` and `/auth` get the small canned answers
 *   below (a signed-in or a signed-out identity service, no delay), every other request is
 *   forwarded unchanged to the web service, so the page sees the web service's own headers;
 * - the browser, from the command after `--`, with `--remote-debugging-port=0` added. It connects
 *   only to the DevTools address that browser printed.
 *
 * Every repetition of a scenario and profile gets its own browser context (an empty cache and no
 * cookies). The cold load is the first navigation in it; the warm load navigates to the same URL
 * again in the same context, so `/static/` files come from the cache and `index.html` is revalidated
 * (`Cache-Control: no-cache` gives a 304).
 *
 * Network profiles use `Network.emulateNetworkConditions`: a throughput cap and a latency added to
 * every request (the response headers arrive no earlier than the latency after the request).
 * It does not model TCP or TLS handshakes, loss or packet scheduling, and the CPU is not throttled
 * unless `--cpu-slowdown` says so.
 *
 * Usage, from the repository root (the browser command is everything after `--`):
 *   bun frontend/tools/measure-load.ts --site frontend/dist --web target/release/web --out <dir> \
 *     [--runs 7] [--profiles none,fast4g,slow4g] [--scenarios login,home,settings] \
 *     [--cpu-slowdown 1] [--compress none|gzip|br] [--upgrade-site <dir>] [--stale-chunk] -- <browser command: --headless, --user-data-dir, and in a container --disable-dev-shm-usage>
 */

import { mkdirSync, readdirSync, writeFileSync } from "node:fs";
import { join, resolve } from "node:path";
import { brotliCompressSync, constants, gzipSync } from "node:zlib";

/** A network profile in DevTools terms: bytes per second (-1 for no limit) and a latency in ms. */
export interface Profile {
  name: string;
  downloadBytesPerSec: number;
  uploadBytesPerSec: number;
  latencyMs: number;
}

/**
 * `fast4g` is WebPageTest's "4G" (9 Mbit/s down, 1.5 Mbit/s up, 170 ms round trip) and `slow4g`
 * is Lighthouse's "slow 4G" (1.6 Mbit/s down, 750 kbit/s up, 150 ms round trip).
 */
export const PROFILES: Readonly<Record<string, Profile>> = {
  none: { name: "none", downloadBytesPerSec: -1, uploadBytesPerSec: -1, latencyMs: 0 },
  fast4g: {
    name: "fast4g",
    downloadBytesPerSec: (9 * 1000 * 1000) / 8,
    uploadBytesPerSec: (1.5 * 1000 * 1000) / 8,
    latencyMs: 170,
  },
  slow4g: {
    name: "slow4g",
    downloadBytesPerSec: (1.6 * 1000 * 1000) / 8,
    uploadBytesPerSec: (750 * 1000) / 8,
    latencyMs: 150,
  },
};

export interface Scenario {
  name: string;
  path: string;
  /** Whether the stand-in answers as an identity service with a session. */
  signedIn: boolean;
  /** A JavaScript expression that is true once the page shows what the user came for. */
  ready: string;
}

const HEADING = (text: string) =>
  `[...document.querySelectorAll("h1")].some((heading) => heading.textContent === ${JSON.stringify(text)})`;

export const SCENARIOS: Readonly<Record<string, Scenario>> = {
  login: {
    name: "login",
    path: "/login",
    signedIn: false,
    ready: `document.querySelector('input[type="email"]') !== null`,
  },
  home: { name: "home", path: "/", signedIn: true, ready: HEADING("Home") },
  settings: { name: "settings", path: "/settings", signedIn: true, ready: HEADING("Settings") },
};

/** The numbers of one navigation. Times are milliseconds from the navigation's start. */
export interface Load {
  /** The path the page ended on (a redirect changes it). */
  finalPath: string;
  ttfb: number;
  fcp: number | null;
  lcp: number | null;
  /** When `Scenario.ready` first held. */
  ready: number | null;
  domContentLoaded: number;
  load: number;
  requests: number;
  fromCache: number;
  /** Bytes on the wire (headers included) for every request. */
  transferBytes: number;
  /** The fastest of three uncached requests for a tiny file after the load: shows the emulated round trip. */
  probeMs: number;
  /** Bytes on the wire for the files under /static/ only. */
  staticBytes: number;
  files: RequestRecord[];
}

export interface RequestRecord {
  path: string;
  status: number;
  fromCache: boolean;
  transferBytes: number;
  headers?: Record<string, string>;
}

export function median(values: readonly number[]): number {
  if (values.length === 0) throw new Error("median of nothing");
  const sorted = [...values].sort((a, b) => a - b);
  const middle = sorted.length >> 1;
  return sorted.length % 2 === 1
    ? (sorted[middle] as number)
    : ((sorted[middle - 1] as number) + (sorted[middle] as number)) / 2;
}

/** The address the browser printed: `DevTools listening on ws://...`. */
export function parseDevToolsUrl(text: string): string | null {
  return /DevTools listening on (ws:\/\/\S+)/.exec(text)?.[1] ?? null;
}

// ---------------------------------------------------------------------------------------------
// The stand-in for the ingress

interface StandIn {
  readonly port: number;
  signedIn: boolean;
  /** The port of the web service the other requests go to. */
  upstreamPort: number;
  /** Requests the stand-in answered itself (paths only). */
  readonly apiRequests: string[];
  /**
   * Files under /static/ that a new deployment has removed: while `lost` holds a path, the request for
   * it is answered 404 as the web service does and is listed in `misses`; with `once`, the first miss
   * ends it (the next deployment's files are there again, as after a reload).
   */
  readonly chunks: { lost: Set<string>; once: boolean; misses: string[] };
  stop(): void;
}

const JSON_HEADERS = { "content-type": "application/json", "cache-control": "no-store" };

const TENANT = { tenant_id: "00000000-0000-4000-8000-000000000001", name: "Demo", role: "admin", state: "active" };
const SESSION = {
  user_id: "00000000-0000-4000-8000-000000000002",
  email: "user@example.com",
  platform_admin: false,
  active_tenant: TENANT,
  tenants: [TENANT],
};
const PREFERENCES = { language: "en", ai_language: "en", time_zone: "UTC", theme: "light" };

function apiAnswer(signedIn: boolean, method: string, pathname: string): Response {
  const json = (status: number, body: unknown) => new Response(JSON.stringify(body), { status, headers: JSON_HEADERS });
  if (!signedIn) return json(401, { code: "unauthenticated", message_key: "error.unauthenticated" });
  if (method === "GET" && pathname === "/api/identity/v1/sessions/current") return json(200, SESSION);
  if (method === "GET" && pathname === "/api/identity/v1/preferences") return json(200, PREFERENCES);
  return json(404, { code: "not_found", message_key: "error.not_found" });
}

/** What a compressing ingress or CDN in front of the web service would do (the web service itself sends files as they are). */
export type Compression = "none" | "gzip" | "br";

const COMPRESSIBLE = /^(text\/|image\/svg\+xml)/;

/** gzip level 6 and Brotli quality 5, the usual proxy defaults. */
export function compress(body: Uint8Array, encoding: Exclude<Compression, "none">): Buffer {
  return encoding === "gzip"
    ? gzipSync(body, { level: 6 })
    : brotliCompressSync(body, { params: { [constants.BROTLI_PARAM_QUALITY]: 5 } });
}

function startStandIn(initialWebPort: number, compression: Compression): StandIn {
  const apiRequests: string[] = [];
  const compressed = new Map<string, Buffer>();
  const state = { signedIn: false, upstreamPort: initialWebPort };
  const chunks = { lost: new Set<string>(), once: false, misses: [] as string[] };
  const server = Bun.serve({
    hostname: "127.0.0.1",
    port: 0,
    async fetch(request) {
      const url = new URL(request.url);
      if (url.pathname.startsWith("/api/") || url.pathname.startsWith("/auth/")) {
        apiRequests.push(`${request.method} ${url.pathname}`);
        return apiAnswer(state.signedIn, request.method, url.pathname);
      }
      if (chunks.lost.has(url.pathname)) {
        chunks.misses.push(url.pathname);
        if (chunks.once) chunks.lost = new Set();
        return new Response("not found", {
          status: 404,
          headers: { "content-type": "text/plain", "cache-control": "no-store" },
        });
      }
      const headers = new Headers(request.headers);
      headers.delete("host");
      headers.delete("accept-encoding");
      const upstream = await fetch(`http://127.0.0.1:${state.upstreamPort}${url.pathname}${url.search}`, {
        method: request.method,
        headers,
        redirect: "manual",
        decompress: false,
      });
      const wants = request.headers.get("accept-encoding") ?? "";
      if (
        compression !== "none" &&
        upstream.status === 200 &&
        request.method === "GET" &&
        wants.includes(compression) &&
        COMPRESSIBLE.test(upstream.headers.get("content-type") ?? "")
      ) {
        const key = `${compression} ${url.pathname} ${upstream.headers.get("etag")}`;
        let body = compressed.get(key);
        if (body === undefined) {
          body = compress(new Uint8Array(await upstream.arrayBuffer()), compression);
          compressed.set(key, body);
        }
        const answer = new Headers(upstream.headers);
        answer.set("content-encoding", compression);
        answer.set("content-length", String(body.length));
        answer.append("vary", "Accept-Encoding");
        return new Response(new Uint8Array(body), { status: 200, headers: answer });
      }
      return new Response(upstream.status === 304 || request.method === "HEAD" ? null : upstream.body, {
        status: upstream.status,
        headers: upstream.headers,
      });
    },
  });
  return {
    port: server.port as number,
    get signedIn() {
      return state.signedIn;
    },
    set signedIn(value: boolean) {
      state.signedIn = value;
    },
    get upstreamPort() {
      return state.upstreamPort;
    },
    set upstreamPort(value: number) {
      state.upstreamPort = value;
    },
    apiRequests,
    chunks,
    stop: () => void server.stop(true),
  };
}

function freePort(): number {
  const listener = Bun.listen({ hostname: "127.0.0.1", port: 0, socket: { data() {} } });
  const port = listener.port;
  listener.stop(true);
  return port;
}

// ---------------------------------------------------------------------------------------------
// The DevTools client

/** What a DevTools command may take before it counts as lost (a slow throttled page is not a lost command). */
const COMMAND_TIMEOUT_MS = 90_000;

type Params = Record<string, unknown>;
type Listener = (params: Params) => void;

class Cdp {
  private nextId = 1;
  private readonly pending = new Map<
    number,
    { resolve(value: Params): void; reject(error: Error): void; method: string }
  >();
  private readonly listeners = new Map<string, Listener[]>();

  private constructor(private readonly socket: WebSocket) {
    socket.addEventListener("message", (event) => {
      const message = JSON.parse(String(event.data)) as {
        id?: number;
        method?: string;
        params?: Params;
        result?: Params;
        error?: { message: string };
        sessionId?: string;
      };
      if (message.id !== undefined) {
        const waiting = this.pending.get(message.id);
        this.pending.delete(message.id);
        if (message.error) waiting?.reject(new Error(`${waiting.method}: ${message.error.message}`));
        else waiting?.resolve(message.result ?? {});
        return;
      }
      for (const listener of this.listeners.get(`${message.sessionId ?? ""}|${message.method}`) ?? []) {
        listener(message.params ?? {});
      }
    });
  }

  static connect(url: string): Promise<Cdp> {
    return new Promise((done, fail) => {
      const socket = new WebSocket(url);
      socket.addEventListener("open", () => done(new Cdp(socket)));
      socket.addEventListener("error", () => fail(new Error(`could not connect to ${url}`)));
    });
  }

  send(method: string, params: Params = {}, sessionId?: string): Promise<Params> {
    const id = this.nextId++;
    return new Promise((resolveSend, reject) => {
      const timer = setTimeout(() => {
        this.pending.delete(id);
        reject(
          new Error(
            `${method} did not answer within ${COMMAND_TIMEOUT_MS} ms: ${JSON.stringify(params).slice(0, 100)}`,
          ),
        );
      }, COMMAND_TIMEOUT_MS);
      this.pending.set(id, {
        resolve: (value) => {
          clearTimeout(timer);
          resolveSend(value);
        },
        reject: (error) => {
          clearTimeout(timer);
          reject(error);
        },
        method,
      });
      this.socket.send(JSON.stringify({ id, method, params, ...(sessionId ? { sessionId } : {}) }));
    });
  }

  on(sessionId: string, method: string, listener: Listener): void {
    const key = `${sessionId}|${method}`;
    this.listeners.set(key, [...(this.listeners.get(key) ?? []), listener]);
  }

  /** Fails every command that is waiting, for example when the page crashed under it. */
  rejectAll(error: Error): void {
    for (const [id, waiting] of this.pending) {
      this.pending.delete(id);
      waiting.reject(error);
    }
  }

  close(): void {
    this.socket.close();
  }
}

function sleep(ms: number): Promise<void> {
  return new Promise((done) => setTimeout(done, ms));
}

/** Waits until `check` is true, polling; fails naming what was awaited. */
async function waitFor(what: string, timeoutMs: number, check: () => boolean | Promise<boolean>): Promise<void> {
  const deadline = Date.now() + timeoutMs;
  while (!(await check())) {
    if (Date.now() > deadline) throw new Error(`timed out after ${timeoutMs} ms waiting for ${what}`);
    await sleep(25);
  }
}

// ---------------------------------------------------------------------------------------------
// One page in its own browser context

const NAVIGATION_TIMEOUT_MS = 120_000;
const QUIET_MS = 600;

function observerSource(ready: string): string {
  return `(() => {
    const marks = { ready: null, lcp: null };
    window.__marks = marks;
    const check = () => {
      if (marks.ready === null) {
        try { if (${ready}) marks.ready = performance.now(); } catch {}
      }
    };
    new MutationObserver(check).observe(document, { subtree: true, childList: true, characterData: true, attributes: true });
    new PerformanceObserver((list) => {
      const last = list.getEntries().at(-1);
      if (last) marks.lcp = last.startTime;
    }).observe({ type: "largest-contentful-paint", buffered: true });
  })();`;
}

class Page {
  private inflight = 0;
  private lastActivity = Date.now();
  private requests = new Map<string, RequestRecord & { type?: string }>();
  private loadFired = false;

  private constructor(
    private readonly cdp: Cdp,
    private readonly sessionId: string,
    private readonly contextId: string,
    private readonly origin: string,
    private readonly scenario: Scenario,
  ) {}

  static async open(
    cdp: Cdp,
    profile: Profile,
    scenario: Scenario,
    origin: string,
    cpuSlowdown: number,
  ): Promise<Page> {
    const { browserContextId } = (await cdp.send("Target.createBrowserContext")) as { browserContextId: string };
    const { targetId } = (await cdp.send("Target.createTarget", { url: "about:blank", browserContextId })) as {
      targetId: string;
    };
    const { sessionId } = (await cdp.send("Target.attachToTarget", { targetId, flatten: true })) as {
      sessionId: string;
    };
    const page = new Page(cdp, sessionId, browserContextId, origin, scenario);
    page.listen();
    await cdp.send("Inspector.enable", {}, sessionId);
    await cdp.send("Page.enable", {}, sessionId);
    await cdp.send("Network.enable", {}, sessionId);
    await cdp.send("Network.setCacheDisabled", { cacheDisabled: false }, sessionId);
    await cdp.send(
      "Emulation.setDeviceMetricsOverride",
      { width: 1280, height: 800, deviceScaleFactor: 1, mobile: false },
      sessionId,
    );
    if (cpuSlowdown > 1) await cdp.send("Emulation.setCPUThrottlingRate", { rate: cpuSlowdown }, sessionId);
    await cdp.send(
      "Network.emulateNetworkConditions",
      {
        offline: false,
        latency: profile.latencyMs,
        downloadThroughput: profile.downloadBytesPerSec,
        uploadThroughput: profile.uploadBytesPerSec,
      },
      sessionId,
    );
    await cdp.send("Page.addScriptToEvaluateOnNewDocument", { source: observerSource(scenario.ready) }, sessionId);
    return page;
  }

  private listen(): void {
    const on = (method: string, listener: Listener) => this.cdp.on(this.sessionId, method, listener);
    const touch = () => {
      this.lastActivity = Date.now();
    };
    on("Network.requestWillBeSent", (params) => {
      const request = params.request as { url: string };
      const url = new URL(request.url);
      if (url.protocol !== "http:") return;
      this.inflight++;
      touch();
      this.requests.set(params.requestId as string, {
        path: url.pathname,
        status: 0,
        fromCache: false,
        transferBytes: 0,
        type: params.type as string | undefined,
      });
    });
    on("Network.requestServedFromCache", (params) => {
      const record = this.requests.get(params.requestId as string);
      if (record) record.fromCache = true;
    });
    on("Network.responseReceived", (params) => {
      const record = this.requests.get(params.requestId as string);
      const response = params.response as { status: number; fromDiskCache?: boolean; headers: Record<string, string> };
      if (!record) return;
      record.status = response.status;
      if (response.fromDiskCache) record.fromCache = true;
      record.headers = Object.fromEntries(
        Object.entries(response.headers)
          .map(([name, value]) => [name.toLowerCase(), value] as const)
          .filter(([name]) =>
            ["cache-control", "etag", "content-security-policy", "content-length", "content-type"].includes(name),
          ),
      );
      touch();
    });
    const finish = (params: Params) => {
      const record = this.requests.get(params.requestId as string);
      if (record && this.inflight > 0) this.inflight--;
      if (record && typeof params.encodedDataLength === "number") record.transferBytes = params.encodedDataLength;
      touch();
    };
    on("Network.loadingFinished", finish);
    on("Network.loadingFailed", finish);
    on("Inspector.targetCrashed", () => {
      this.cdp.rejectAll(new Error("the page crashed (a small /dev/shm can do that: add --disable-dev-shm-usage)"));
    });
    on("Page.loadEventFired", () => {
      this.loadFired = true;
      touch();
    });
  }

  /** Navigates, waits for the ready marker, the load event and a quiet network, and reads the numbers. */
  async visit(): Promise<Load> {
    this.requests = new Map();
    this.inflight = 0;
    this.loadFired = false;
    this.lastActivity = Date.now();
    await this.cdp.send("Page.navigate", { url: `${this.origin}${this.scenario.path}` }, this.sessionId);
    const evaluate = <T>(expression: string): Promise<T> => this.evaluate<T>(expression);
    await waitFor(`the ${this.scenario.name} page to be ready`, NAVIGATION_TIMEOUT_MS, async () => {
      if (!this.loadFired) return false;
      return (await evaluate<number | null>("window.__marks?.ready ?? null")) !== null;
    });
    await waitFor(
      "the network to be quiet",
      NAVIGATION_TIMEOUT_MS,
      () => this.inflight === 0 && Date.now() - this.lastActivity >= QUIET_MS,
    );
    const timing = await evaluate<{
      finalPath: string;
      ttfb: number;
      fcp: number | null;
      lcp: number | null;
      ready: number | null;
      domContentLoaded: number;
      load: number;
    }>(`(() => {
      const nav = performance.getEntriesByType("navigation")[0];
      const fcp = performance.getEntriesByName("first-contentful-paint")[0];
      return {
        finalPath: location.pathname,
        ttfb: nav.responseStart,
        fcp: fcp ? fcp.startTime : null,
        lcp: window.__marks.lcp,
        ready: window.__marks.ready,
        domContentLoaded: nav.domContentLoadedEventEnd,
        load: nav.loadEventEnd,
      };
    })()`);
    const files = [...this.requests.values()].map(({ type: _type, ...record }) => record);
    const probeMs = await evaluate<number>(`(async () => {
      const times = [];
      for (let i = 0; i < 3; i++) {
        const start = performance.now();
        await (await fetch("/robots.txt", { cache: "no-store" })).arrayBuffer();
        times.push(performance.now() - start);
      }
      return Math.min(...times);
    })()`);
    return {
      ...timing,
      probeMs,
      requests: files.length,
      fromCache: files.filter((file) => file.fromCache).length,
      transferBytes: files.reduce((sum, file) => sum + file.transferBytes, 0),
      staticBytes: files
        .filter((file) => file.path.startsWith("/static/"))
        .reduce((sum, file) => sum + file.transferBytes, 0),
      files,
    };
  }

  /** The value of a JavaScript expression in the page. */
  async evaluate<T>(expression: string): Promise<T> {
    const answer = (await this.cdp.send(
      "Runtime.evaluate",
      { expression, returnByValue: true, awaitPromise: true },
      this.sessionId,
    )) as { result?: { value?: T } };
    return answer.result?.value as T;
  }

  /** Leaves the page, so the next visit is a new navigation in the same context (and cache). */
  async blank(): Promise<void> {
    await this.cdp.send("Page.navigate", { url: "about:blank" }, this.sessionId);
    await sleep(200);
  }

  async close(): Promise<void> {
    await this.cdp.send("Target.disposeBrowserContext", { browserContextId: this.contextId });
  }
}

// ---------------------------------------------------------------------------------------------
// The run

export interface Options {
  site: string;
  /** A second build, served after the first load as if a new version had been deployed. */
  upgradeSite?: string;
  /** Check what a tab that is still open does when a deployment has removed a chunk, instead of measuring. */
  staleChunk: boolean;
  web: string;
  out: string;
  runs: number;
  profiles: string[];
  scenarios: string[];
  cpuSlowdown: number;
  compression: Compression;
  browser: string[];
}

export function parseArgs(argv: readonly string[]): Options {
  const separator = argv.indexOf("--");
  if (separator < 0 || separator === argv.length - 1) throw new Error("the browser command goes after `--`");
  const flags = argv.slice(0, separator);
  const value = (name: string): string | undefined => {
    const index = flags.indexOf(name);
    return index < 0 ? undefined : flags[index + 1];
  };
  const required = (name: string): string => {
    const found = value(name);
    if (!found) throw new Error(`${name} is required`);
    return found;
  };
  const list = (name: string, fallback: string[], known: Record<string, unknown>): string[] => {
    const items = value(name)?.split(",") ?? fallback;
    for (const item of items) {
      if (!(item in known)) throw new Error(`${name}: unknown "${item}" (one of ${Object.keys(known).join(", ")})`);
    }
    return items;
  };
  const runs = Number(value("--runs") ?? "7");
  const cpuSlowdown = Number(value("--cpu-slowdown") ?? "1");
  if (!Number.isInteger(runs) || runs < 1) throw new Error("--runs must be a positive integer");
  const compression = value("--compress") ?? "none";
  if (compression !== "none" && compression !== "gzip" && compression !== "br")
    throw new Error("--compress is none, gzip or br");
  if (!Number.isFinite(cpuSlowdown) || cpuSlowdown < 1) throw new Error("--cpu-slowdown must be at least 1");
  const upgradeSite = value("--upgrade-site");
  return {
    site: resolve(required("--site")),
    ...(upgradeSite ? { upgradeSite: resolve(upgradeSite) } : {}),
    staleChunk: flags.includes("--stale-chunk"),
    web: resolve(required("--web")),
    out: resolve(required("--out")),
    runs,
    profiles: list("--profiles", ["none", "fast4g", "slow4g"], PROFILES),
    scenarios: list("--scenarios", ["login", "home", "settings"], SCENARIOS),
    cpuSlowdown,
    compression,
    browser: argv.slice(separator + 1),
  };
}

export interface RunRecord {
  scenario: string;
  profile: string;
  cache: "cold" | "warm" | "upgrade";
  run: number;
  load: Load;
}

const METRICS = ["ttfb", "fcp", "ready", "lcp", "load"] as const;

/** One row per scenario, profile and cache state: the median of each metric over the runs. */
export function summarize(records: readonly RunRecord[]): string {
  const groups = new Map<string, RunRecord[]>();
  for (const record of records) {
    const key = `${record.scenario}\t${record.profile}\t${record.cache}`;
    groups.set(key, [...(groups.get(key) ?? []), record]);
  }
  const rows = [
    "| scenario | profile | cache | runs | ttfb ms | fcp ms | ready ms | lcp ms | load ms | probe ms | KB on the wire | requests (from cache) |",
    "| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |",
  ];
  for (const [key, group] of groups) {
    const [scenario, profile, cache] = key.split("\t");
    const cell = (metric: (typeof METRICS)[number]) => {
      const numbers = group.map((record) => record.load[metric]).filter((n): n is number => n !== null);
      return numbers.length === 0
        ? "-"
        : `${Math.round(median(numbers))} (${Math.round(Math.min(...numbers))}-${Math.round(Math.max(...numbers))})`;
    };
    const probe = Math.round(median(group.map((record) => record.load.probeMs)));
    const kb = median(group.map((record) => record.load.transferBytes)) / 1000;
    const requests = median(group.map((record) => record.load.requests));
    const cached = median(group.map((record) => record.load.fromCache));
    rows.push(
      `| ${scenario} | ${profile} | ${cache} | ${group.length} | ${METRICS.map(cell).join(" | ")} | ${probe} | ${kb.toFixed(1)} | ${requests} (${cached}) |`,
    );
  }
  return rows.join("\n");
}

async function readDevToolsUrl(stderr: ReadableStream<Uint8Array>): Promise<{ url: string; rest: Promise<void> }> {
  const reader = stderr.getReader();
  const decoder = new TextDecoder();
  let text = "";
  const deadline = Date.now() + 120_000;
  while (Date.now() < deadline) {
    const chunk = await reader.read();
    if (chunk.done) break;
    text += decoder.decode(chunk.value);
    const url = parseDevToolsUrl(text);
    if (url) {
      // Keep reading, so a full pipe never blocks the browser.
      const rest = (async () => {
        while (!(await reader.read()).done) {}
      })();
      return { url, rest };
    }
  }
  throw new Error(`the browser did not print a DevTools address; its output:\n${text.slice(-2000)}`);
}

interface Web {
  port: number;
  process: ReturnType<typeof Bun.spawn>;
}

/** Starts the web service on two free loopback ports and waits until it is ready. */
async function startWeb(binary: string, site: string): Promise<Web> {
  const port = freePort();
  const healthPort = freePort();
  const child = Bun.spawn([binary], {
    env: {
      ...process.env,
      TRUSTIOT__ROOT_DIR: site,
      TRUSTIOT__HTTP__LISTEN_ADDR: `127.0.0.1:${port}`,
      TRUSTIOT__HTTP__HEALTH_LISTEN_ADDR: `127.0.0.1:${healthPort}`,
    },
    stdout: "ignore",
    stderr: "inherit",
  });
  try {
    await waitFor("the web service to be ready", 60_000, async () => {
      try {
        return (await fetch(`http://127.0.0.1:${healthPort}/readyz`)).ok;
      } catch {
        return false;
      }
    });
  } catch (error) {
    child.kill();
    throw error;
  }
  return { port, process: child };
}

export interface StaleChunkResult {
  /** Chunks the old tab had not loaded when the deployment removed them. */
  removedChunks: number;
  /** The page loaded again by itself after the first missing chunk. */
  reloadedAfterFirstMiss: boolean;
  reloadStamp: string | null;
  /** The page it ended on after that reload. */
  headingAfterReload: string | null;
  /** The second missing chunk, within the minute, did not reload again. */
  noSecondReload: boolean;
  /** What the second missing chunk showed instead. */
  headingAfterSecondMiss: string | null;
  misses: string[];
}

const HEADING_TEXT = `[...document.querySelectorAll("h1")].map((heading) => heading.textContent)[0] ?? null`;

/**
 * An old tab is open on the home page; a deployment then removes the chunks it has not loaded yet.
 * The first page it goes to that needs one must reload (src/app/preload-error.ts); the next missing
 * chunk within a minute must not reload again but show the error page.
 */
async function checkStaleChunk(cdp: Cdp, standIn: StandIn, origin: string, site: string): Promise<StaleChunkResult> {
  standIn.signedIn = true;
  const page = await Page.open(cdp, PROFILES.none as Profile, SCENARIOS.home as Scenario, origin, 1);
  const goTo = (path: string) =>
    page.evaluate(
      `(() => { history.pushState({}, "", ${JSON.stringify(path)}); window.dispatchEvent(new PopStateEvent("popstate")); })()`,
    );
  const origin_ = () => page.evaluate<number>("performance.timeOrigin").catch(() => Number.NaN);
  try {
    const first = await page.visit();
    const loaded = new Set(first.files.map((file) => file.path));
    const removed = readdirSync(join(site, "static"))
      .filter((name) => name.endsWith(".js"))
      .map((name) => `/static/${name}`)
      .filter((path) => !loaded.has(path));
    if (removed.length === 0)
      throw new Error("the build has no chunk that the home page does not load: nothing to check");
    const before = await origin_();
    standIn.chunks.lost = new Set(removed);
    standIn.chunks.once = true;
    await goTo("/settings");
    await waitFor("the page to reload after the missing chunk", 60_000, async () => {
      const now = await origin_();
      return Number.isFinite(now) && now !== before;
    });
    await waitFor("the settings page after the reload", 60_000, async () => {
      return (await page.evaluate<string | null>(HEADING_TEXT).catch(() => null)) === "Settings";
    });
    const reloadStamp = await page.evaluate<string | null>(`sessionStorage.getItem("trustiot.preload-reload-at")`);
    const headingAfterReload = await page.evaluate<string | null>(HEADING_TEXT);

    // A second deployment: the chunks this page has not loaded are gone again.
    const nowLoaded = new Set(
      await page.evaluate<string[]>(
        `performance.getEntriesByType("resource").map((entry) => new URL(entry.name).pathname)`,
      ),
    );
    standIn.chunks.lost = new Set(removed.filter((path) => !nowLoaded.has(path)));
    standIn.chunks.once = false;
    const stamp = await origin_();
    await goTo("/devices/1");
    await waitFor("the error page for the second missing chunk", 60_000, async () => {
      return (await page.evaluate<string | null>(HEADING_TEXT).catch(() => null)) === "Something went wrong";
    });
    await sleep(2000);
    const after = await origin_();
    return {
      removedChunks: removed.length,
      reloadedAfterFirstMiss: true,
      reloadStamp,
      headingAfterReload,
      noSecondReload: after === stamp,
      headingAfterSecondMiss: await page.evaluate<string | null>(HEADING_TEXT),
      misses: standIn.chunks.misses,
    };
  } finally {
    standIn.chunks.lost = new Set();
    await page.close();
  }
}

async function main(options: Options): Promise<void> {
  mkdirSync(options.out, { recursive: true });
  const webs: Web[] = [];
  let standIn: StandIn | undefined;
  let browser: ReturnType<typeof Bun.spawn> | undefined;
  let cdp: Cdp | undefined;
  try {
    const web = await startWeb(options.web, options.site);
    webs.push(web);
    const upgraded = options.upgradeSite ? await startWeb(options.web, options.upgradeSite) : undefined;
    if (upgraded) webs.push(upgraded);
    standIn = startStandIn(web.port, options.compression);
    const origin = `http://127.0.0.1:${standIn.port}`;
    browser = Bun.spawn([...options.browser, "--remote-debugging-port=0"], { stdout: "ignore", stderr: "pipe" });
    const { url } = await readDevToolsUrl(browser.stderr as ReadableStream<Uint8Array>);
    cdp = await Cdp.connect(url);
    const version = (await cdp.send("Browser.getVersion")) as { product: string; userAgent: string };

    if (options.staleChunk) {
      const result = await checkStaleChunk(cdp, standIn, origin, options.site);
      writeFileSync(
        join(options.out, "stale-chunk.json"),
        `${JSON.stringify({ browser: version.product, ...result }, null, 1)}\n`,
      );
      process.stdout.write(`browser: ${version.product}\n${JSON.stringify(result, null, 1)}\n`);
      return;
    }

    const records: RunRecord[] = [];
    for (const scenarioName of options.scenarios) {
      const scenario = SCENARIOS[scenarioName] as Scenario;
      for (const profileName of options.profiles) {
        const profile = PROFILES[profileName] as Profile;
        for (let run = 1; run <= options.runs; run++) {
          standIn.signedIn = scenario.signedIn;
          const page = await Page.open(cdp, profile, scenario, origin, options.cpuSlowdown);
          try {
            records.push({
              scenario: scenarioName,
              profile: profileName,
              cache: "cold",
              run,
              load: await page.visit(),
            });
            await page.blank();
            records.push({
              scenario: scenarioName,
              profile: profileName,
              cache: "warm",
              run,
              load: await page.visit(),
            });
            if (upgraded) {
              // A new version is deployed: the same browser, with the old version's files cached, visits again.
              standIn.upstreamPort = upgraded.port;
              await page.blank();
              records.push({
                scenario: scenarioName,
                profile: profileName,
                cache: "upgrade",
                run,
                load: await page.visit(),
              });
              standIn.upstreamPort = web.port;
            }
          } finally {
            await page.close();
          }
          console.error(`${scenarioName} ${profileName} run ${run}/${options.runs} done`);
        }
      }
    }

    const summary = summarize(records);
    // No local paths in the file: it is kept as test evidence.
    const { runs, profiles, scenarios, cpuSlowdown, compression } = options;
    const upgrade = options.upgradeSite !== undefined;
    const report = {
      browser: version.product,
      userAgent: version.userAgent,
      options: { runs, profiles, scenarios, cpuSlowdown, compression, upgrade },
      apiRequests: [...new Set(standIn.apiRequests)],
      records,
    };
    writeFileSync(join(options.out, "results.json"), `${JSON.stringify(report, null, 1)}\n`);
    writeFileSync(join(options.out, "summary.md"), `${summary}\n`);
    process.stdout.write(`browser: ${version.product}\n`);
    process.stdout.write(`API requests the stand-in saw: ${[...new Set(standIn.apiRequests)].join(", ") || "none"}\n`);
    process.stdout.write(`${summary}\n`);
  } finally {
    cdp?.close();
    browser?.kill();
    standIn?.stop();
    for (const web of webs) web.process.kill();
    await Promise.allSettled([browser?.exited, ...webs.map((web) => web.process.exited)]);
  }
}

if (import.meta.main) {
  try {
    await main(parseArgs(process.argv.slice(2)));
  } catch (error) {
    console.error(error instanceof Error ? error.message : error);
    process.exit(1);
  }
}
