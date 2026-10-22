import { describe, expect, test } from "bun:test";

import { isReservedPath, loginPath, safeNextPath } from "./paths.ts";
import { RELOAD_INTERVAL_MS, RELOAD_KEY, reloadOnPreloadError, shouldReload } from "./preload-error.ts";

describe("safeNextPath", () => {
  test.each(["/", "/devices/42", "/devices/42?tab=history#chart", "/%E8%A8%AD%E5%82%99"])(
    "keeps the SPA path %s",
    (path) => {
      expect(safeNextPath(path)).toBe(path);
    },
  );

  test.each([
    null,
    "",
    "devices",
    "//evil.example.test/x",
    "/\\evil.example.test",
    "https://evil.example.test/",
    "javascript:alert(1)",
    "/\tevil",
    "/api/identity/v1/tenants",
    "/auth/logout",
    "/static/abc.js",
    "/login",
  ])("rejects %p", (raw) => {
    expect(safeNextPath(raw)).toBe("/");
  });

  test("loginPath carries the page to return to", () => {
    expect(loginPath("/devices/42?tab=1")).toBe("/login?next=%2Fdevices%2F42%3Ftab%3D1");
    expect(loginPath("/")).toBe("/login");
    expect(loginPath("//evil.example.test")).toBe("/login");
  });

  test("reserved paths", () => {
    for (const path of ["/api", "/api/x", "/auth/login", "/internal/x", "/static/a.js", "/healthz", "/robots.txt"]) {
      expect(isReservedPath(path)).toBe(true);
    }
    for (const path of ["/", "/apis", "/authors", "/devices", "/statics"]) {
      expect(isReservedPath(path)).toBe(false);
    }
  });
});

class MemoryStorage {
  readonly values = new Map<string, string>();
  getItem(key: string) {
    return this.values.get(key) ?? null;
  }
  setItem(key: string, value: string) {
    this.values.set(key, value);
  }
}

describe("reload after a deployment removed a chunk", () => {
  test("reloads once, then not again within the interval", () => {
    const storage = new MemoryStorage();
    expect(shouldReload(storage, 1_000_000)).toBe(true);
    expect(storage.values.get(RELOAD_KEY)).toBe("1000000");
    expect(shouldReload(storage, 1_000_000 + RELOAD_INTERVAL_MS - 1)).toBe(false);
    expect(shouldReload(storage, 1_000_000 + RELOAD_INTERVAL_MS)).toBe(true);
  });

  test("without working storage it never reloads (no loop guard)", () => {
    expect(shouldReload(undefined, 1)).toBe(false);
    const throwing = {
      getItem: () => {
        throw new Error("SecurityError");
      },
      setItem: () => {},
    };
    expect(shouldReload(throwing, 1)).toBe(false);
  });

  test("the vite:preloadError listener reloads and prevents the error", () => {
    const target = new EventTarget();
    let reloads = 0;
    reloadOnPreloadError({
      target: target as unknown as Window,
      storage: new MemoryStorage(),
      reload: () => {
        reloads += 1;
      },
      now: () => 5_000_000,
    });
    const first = new Event("vite:preloadError", { cancelable: true });
    target.dispatchEvent(first);
    expect(reloads).toBe(1);
    expect(first.defaultPrevented).toBe(true);
    const second = new Event("vite:preloadError", { cancelable: true });
    target.dispatchEvent(second);
    expect(reloads).toBe(1);
    expect(second.defaultPrevented).toBe(false);
  });
});
