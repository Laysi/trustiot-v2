import { describe, expect, test } from "bun:test";
import { readFileSync } from "node:fs";

const SRC = new URL("../../", import.meta.url).pathname;

function sources(): string[] {
  return [...new Bun.Glob("**/*.{ts,tsx}").scanSync({ cwd: SRC })].filter((file) => !file.includes(".test."));
}

describe("the 「問 AI」 button", () => {
  test("is one component, which the alerts and reports pages use", () => {
    const definitions = sources().filter((file) =>
      /(?:function|const)\s+\w*Ask\w*Button\b/.test(readFileSync(`${SRC}${file}`, "utf8")),
    );
    expect(definitions).toEqual(["components/ask/AskAiButton.tsx"]);
    for (const file of ["pages/alerts/RuleDetailPage.tsx", "pages/reports/ReportPage.tsx"]) {
      expect(readFileSync(`${SRC}${file}`, "utf8")).toContain("components/ask/AskAiButton.tsx");
    }
  });
});
