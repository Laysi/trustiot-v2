import { describe, expect, test } from "bun:test";

import {
  conversationPath,
  conversationsPath,
  isConversationPath,
  isReservedPath,
  loginPath,
  safeNextPath,
  spaPath,
} from "./paths.ts";
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

  // Each passes the check on the raw text, and parsing it (dot segments, `%2e`, `\` read as `/`)
  // yields a path that starts with `//`, a reserved path or the sign-in page.
  test.each([
    "/.//evil.example.test",
    "/..//evil.example.test",
    "/%2e%2e//evil.example.test",
    "/%2E%2E//evil.example.test",
    "/%2e.//evil.example.test",
    "/.%2E//evil.example.test",
    "/%2e//evil.example.test",
    "/./../..//evil.example.test",
    "/a/..//evil.example.test?x=1#y",
    "/./\\evil.example.test",
    "/.\\/evil.example.test",
    "/x/../\\evil",
    "/X/../\\EVIL.EXAMPLE.TEST",
    "/x/%2E%2E/\\evil.example.test",
    "/x/../login",
    "/x/%2e%2e/api/identity/v1/tenants",
    "/LOGIN",
    "/Login?next=%2F",
    "/login/",
  ])("rejects %p, which parses into another host or a path it may not return to", (raw) => {
    expect(safeNextPath(raw)).toBe("/");
  });

  test("normalizes dot segments that stay inside the SPA", () => {
    expect(safeNextPath("/devices/./42/../43")).toBe("/devices/43");
  });

  // The parser leaves percent-escapes as they are, but the router decodes each segment before it
  // matches: checked on the raw path alone, these would reach the sign-in page or a reserved path.
  test.each([
    "/%6Cogin",
    "/%6Cogin?next=/settings",
    "/%4C%4FGIN",
    "/l%6Fgin/",
    "/%61pi/x",
    "/%61%70%69",
    "/%73andbox/x",
    "/%2Fevil.example.test",
    "/x/%2F%2Fevil.example.test",
    "/./%5Cevil.example.test",
    "/%5C%5Cevil.example.test",
    "/%0Aevil",
    "/%E0%A4%A",
  ])("rejects %p, which the router decodes into a path it may not return to", (raw) => {
    expect(safeNextPath(raw)).toBe("/");
    expect(spaPath(raw)).toBeNull();
  });

  test("keeps escapes that decode to an ordinary segment", () => {
    expect(safeNextPath("/devices/%E8%A8%AD%E5%82%99")).toBe("/devices/%E8%A8%AD%E5%82%99");
    expect(safeNextPath("/devices/a%20b")).toBe("/devices/a%20b");
    expect(safeNextPath("/%6Cogins")).toBe("/%6Cogins");
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

describe("spaPath", () => {
  test("is the path safeNextPath keeps, or null where safeNextPath falls back to the home page", () => {
    expect(spaPath("/settings?x=1#y")).toBe("/settings?x=1#y");
    expect(spaPath("/")).toBe("/");
    for (const refused of [
      "",
      null,
      undefined,
      "settings",
      "//evil.example.test",
      "/\\evil.example.test",
      "https://evil.example.test/",
      "javascript:alert(1)",
      "/api/assistant/v1/conversations",
      "/login",
      "/.//evil.example.test",
    ]) {
      expect(spaPath(refused)).toBeNull();
      expect(safeNextPath(refused)).toBe("/");
    }
  });
});

describe("conversation paths", () => {
  test("the history's tabs and a conversation's page", () => {
    expect(conversationsPath()).toBe("/conversations");
    expect(conversationsPath("archived")).toBe("/conversations?tab=archived");
    expect(conversationsPath("deleted")).toBe("/conversations?tab=deleted");
    expect(conversationPath("0b6c3c2e-0000-4000-8000-000000000001")).toBe(
      "/conversations/0b6c3c2e-0000-4000-8000-000000000001",
    );
    expect(conversationPath("a/b")).toBe("/conversations/a%2Fb");
  });

  test("a conversation's page is a conversation page, the history is not", () => {
    for (const path of [
      "/conversations/abc",
      "/Conversations/abc/",
      "/conversations/0b6c3c2e-0000-4000-8000-000000000001",
    ]) {
      expect(isConversationPath(path)).toBe(true);
    }
    for (const path of ["/", "/conversations", "/conversations/", "/conversations/a/b", "/dashboards/abc"]) {
      expect(isConversationPath(path)).toBe(false);
    }
  });
});

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
