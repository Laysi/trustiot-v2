import { describe, expect, test } from "bun:test";
import { readFileSync } from "node:fs";
import { basename } from "node:path";

import { CHARACTER_REFERENCE_ALIAS } from "../../vite.config.ts";

/**
 * The Markdown parser's character references (vite.config.ts, `CHARACTER_REFERENCE_ALIAS`): the
 * build must take the package's table lookup, not its browser build, which writes into
 * `innerHTML`. A package upgrade that renames the files or moves the lookup back into the DOM
 * fails here instead of in the browser.
 */
describe("character references in the build", () => {
  test("the alias names the package's default build, which uses no HTML sink", () => {
    expect(CHARACTER_REFERENCE_ALIAS.find.test("decode-named-character-reference")).toBe(true);
    expect(basename(CHARACTER_REFERENCE_ALIAS.replacement)).toBe("index.js");
    const source = readFileSync(CHARACTER_REFERENCE_ALIAS.replacement, "utf8");
    expect(source).not.toContain("innerHTML");
    expect(source).not.toContain("document");
  });

  test("the lookup decodes what the browser build decodes", async () => {
    const { decodeNamedCharacterReference } = await import(CHARACTER_REFERENCE_ALIAS.replacement);
    expect(decodeNamedCharacterReference("amp")).toBe("&");
    expect(decodeNamedCharacterReference("colon")).toBe(":");
    expect(decodeNamedCharacterReference("no-such-name")).toBe(false);
  });
});
