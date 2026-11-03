import { describe, expect, test } from "bun:test";
import { brotliDecompressSync, gunzipSync } from "node:zlib";

import { compress, median, parseArgs, parseDevToolsUrl, type RunRecord, summarize } from "./measure-load.ts";

const BROWSER = ["--", "chrome", "--headless"];
const REQUIRED = ["--site", "dist", "--web", "bin/web", "--out", "out"];

describe("median", () => {
  test("takes the middle value, or the mean of the two in the middle", () => {
    expect(median([5, 1, 3])).toBe(3);
    expect(median([4, 1, 3, 2])).toBe(2.5);
  });

  test("refuses nothing to take the median of", () => {
    expect(() => median([])).toThrow();
  });
});

describe("parseDevToolsUrl", () => {
  test("reads the address the browser printed", () => {
    const output = "[0101/000000:WARNING] noise\nDevTools listening on ws://127.0.0.1:41234/devtools/browser/abc-123\n";
    expect(parseDevToolsUrl(output)).toBe("ws://127.0.0.1:41234/devtools/browser/abc-123");
  });

  test("finds nothing before the browser has printed it", () => {
    expect(parseDevToolsUrl("[0101/000000:WARNING] noise")).toBeNull();
  });
});

describe("parseArgs", () => {
  test("keeps the browser command after `--` and defaults the rest", () => {
    const options = parseArgs([...REQUIRED, ...BROWSER]);
    expect(options.browser).toEqual(["chrome", "--headless"]);
    expect(options.runs).toBe(7);
    expect(options.profiles).toEqual(["none", "fast4g", "slow4g"]);
    expect(options.scenarios).toEqual(["login", "home", "settings"]);
    expect(options.compression).toBe("none");
    expect(options.staleChunk).toBe(false);
    expect(options.upgradeSite).toBeUndefined();
  });

  test("takes the upgrade build and the stale chunk check from flags", () => {
    const options = parseArgs([...REQUIRED, "--upgrade-site", "next", "--stale-chunk", ...BROWSER]);
    expect(options.upgradeSite).toMatch(/next$/);
    expect(options.staleChunk).toBe(true);
  });

  test("takes the run count, profiles and compression from flags", () => {
    const options = parseArgs([...REQUIRED, "--runs", "3", "--profiles", "slow4g", "--compress", "br", ...BROWSER]);
    expect(options.runs).toBe(3);
    expect(options.profiles).toEqual(["slow4g"]);
    expect(options.compression).toBe("br");
  });

  test("refuses a missing browser command, an unknown profile and a bad count", () => {
    expect(() => parseArgs(REQUIRED)).toThrow("after `--`");
    expect(() => parseArgs([...REQUIRED, "--profiles", "edge", ...BROWSER])).toThrow('unknown "edge"');
    expect(() => parseArgs([...REQUIRED, "--runs", "0", ...BROWSER])).toThrow("--runs");
    expect(() => parseArgs(["--site", "dist", ...BROWSER])).toThrow("--web is required");
  });
});

describe("compress", () => {
  test("gzip and Brotli decode back to the file", () => {
    const body = new TextEncoder().encode("export const x = 1;\n".repeat(200));
    expect(new Uint8Array(gunzipSync(compress(body, "gzip")))).toEqual(body);
    expect(new Uint8Array(brotliDecompressSync(compress(body, "br")))).toEqual(body);
    expect(compress(body, "gzip").length).toBeLessThan(body.length);
  });
});

describe("summarize", () => {
  const load = (ready: number, transferBytes: number) => ({
    finalPath: "/login",
    ttfb: 1,
    fcp: ready,
    lcp: ready,
    ready,
    domContentLoaded: ready,
    load: ready,
    probeMs: 150,
    requests: 6,
    fromCache: 0,
    transferBytes,
    staticBytes: transferBytes,
    files: [],
  });

  test("gives one row per scenario, profile and cache state with the median and the range", () => {
    const records: RunRecord[] = [
      { scenario: "login", profile: "slow4g", cache: "cold", run: 1, load: load(3000, 200_000) },
      { scenario: "login", profile: "slow4g", cache: "cold", run: 2, load: load(5000, 200_000) },
      { scenario: "login", profile: "slow4g", cache: "cold", run: 3, load: load(4000, 200_000) },
      { scenario: "login", profile: "slow4g", cache: "warm", run: 1, load: load(200, 3000) },
    ];
    const rows = summarize(records).split("\n");
    expect(rows).toHaveLength(4);
    expect(rows[2]).toContain("| login | slow4g | cold | 3 | 1 (1-1) | 4000 (3000-5000) | 4000 (3000-5000) |");
    expect(rows[2]).toContain("| 200.0 | 6 (0) |");
    expect(rows[3]).toContain("| login | slow4g | warm | 1 |");
  });
});
