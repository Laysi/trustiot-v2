// The server sends message keys, never text (M1, crates/contract/src/error.rs). Every key a
// service can send needs a translation here, so a Rust change that adds one fails the frontend
// job until the translations land in the same pull request (docs/architecture/ci-release.md).
import { describe, expect, test } from "bun:test";
import { readdirSync, readFileSync, statSync } from "node:fs";
import { join, relative } from "node:path";
import { fileURLToPath } from "node:url";

import { ERROR_CODES } from "../api/errors.ts";
import { ERROR_CODE_MESSAGES, translateError } from "./errors.ts";
import { isMessageId } from "./messages.ts";

const ROOT = fileURLToPath(new URL("../../../", import.meta.url));

/**
 * Keys the scan finds that the frontend deliberately does not translate, each with the reason.
 * Empty so far.
 */
const IGNORED: ReadonlyMap<string, string> = new Map();

/** The Rust source directories whose message keys reach the browser. */
function rustSourceDirs(): string[] {
  const services = readdirSync(join(ROOT, "services"))
    .map((name) => join(ROOT, "services", name, "src"))
    .filter((dir) => {
      try {
        return statSync(dir).isDirectory();
      } catch {
        return false;
      }
    });
  return [join(ROOT, "crates", "contract", "src"), ...services];
}

function rustFiles(dir: string): string[] {
  return readdirSync(dir).flatMap((name) => {
    const path = join(dir, name);
    if (statSync(path).isDirectory()) return name === "tests" ? [] : rustFiles(path);
    return name.endsWith(".rs") && name !== "tests.rs" ? [path] : [];
  });
}

/**
 * The message keys a Rust file defines: `MessageKey::new("…")` and `message_key!("…")`, outside
 * comments and outside its `#[cfg(test)]` module (the repository puts that module at the end).
 */
export function messageKeysIn(source: string): string[] {
  const testModule = source.indexOf("#[cfg(test)]");
  const code = (testModule < 0 ? source : source.slice(0, testModule))
    .split("\n")
    .map((line) => line.replace(/\/\/.*$/, ""))
    .join("\n");
  return Array.from(code.matchAll(/(?:MessageKey::new|message_key!)\(\s*"([^"]+)"/g), (match) => match[1] as string);
}

function serverKeys(): Map<string, string> {
  const keys = new Map<string, string>();
  for (const dir of rustSourceDirs()) {
    for (const file of rustFiles(dir)) {
      for (const key of messageKeysIn(readFileSync(file, "utf8"))) {
        if (!keys.has(key)) keys.set(key, relative(ROOT, file));
      }
    }
  }
  return keys;
}

describe("server message keys", () => {
  test("every error code in api/openapi.json is known here and has its default message", () => {
    const spec = JSON.parse(readFileSync(join(ROOT, "api", "openapi.json"), "utf8"));
    const codes: string[] = spec.components.schemas.ErrorCode.enum;
    expect<string[]>([...ERROR_CODES].sort()).toEqual([...codes].sort());
    const messages: Record<string, string> = ERROR_CODE_MESSAGES;
    for (const code of codes) {
      expect(messages[code]).toBe(`error.${code}`);
      expect(isMessageId(`error.${code}`)).toBe(true);
    }
  });

  test("every message key a service can send has a translation", () => {
    const missing = [...serverKeys()]
      .filter(([key]) => !IGNORED.has(key) && !isMessageId(key))
      .map(([key, file]) => `${key} (${file})`);
    expect(missing).toEqual([]);
  });

  test("the scan finds the keys it must", () => {
    // Guards the scan: the default keys and a service key must be found, or the test above
    // would pass without looking at anything.
    const keys = serverKeys();
    expect(keys.has("error.not_found")).toBe(true);
    expect(keys.has("tenant.name_invalid")).toBe(true);
  });

  test("keys only in comments or test modules are not counted", () => {
    expect(
      messageKeysIn(
        [
          '/// let key = contract::message_key!("doc.example");',
          'pub const A: MessageKey = MessageKey::new("real.key"); // MessageKey::new("comment.key")',
          "#[cfg(test)]",
          'mod tests { const B: MessageKey = MessageKey::new("test.key"); }',
        ].join("\n"),
      ),
    ).toEqual(["real.key"]);
  });

  test("the cross-site refusal of the identity service has a translation", () => {
    // services/identity answers a cross-site write with this key (Laysi/trustiot-v2#123).
    expect(isMessageId("security.cross_site_request")).toBe(true);
  });
});

describe("translateError", () => {
  test("a known message key is translated with its values", () => {
    expect(
      translateError(
        "en",
        { code: "validation_failed", message_key: "tenant.name_invalid", details: { max: 100 } },
        422,
      ),
    ).toBe("Enter a name of 1 to 100 characters.");
  });

  test("an unknown message key falls back to the code's message", () => {
    expect(translateError("zh-TW", { code: "conflict", message_key: "gateway.already_bound" }, 409)).toBe(
      "內容已被其他人變更，請重新載入後再試一次。",
    );
  });

  test("a response without an M1 body falls back to the status", () => {
    expect(translateError("en", null, 503)).toBe("The service is temporarily unavailable. Try again later.");
    expect(translateError("en", null, 401)).toBe("Your session has ended. Sign in again.");
    expect(translateError("en", null, 0)).toBe("Could not reach the server. Check your connection and try again.");
    expect(translateError("en", null, 502)).toBe("Something went wrong on our side. Try again later.");
    expect(translateError("en", null, 418)).toBe("Something went wrong. Try again later.");
  });
});
