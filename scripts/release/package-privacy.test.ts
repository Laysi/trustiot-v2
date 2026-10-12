import { describe, expect, test } from "bun:test";
import { type PackagePrivacy, classifyAnonymousPull, privacyError, privacyNote, privacySummary } from "./package-privacy.ts";

const REF = (name: string): string => `ghcr.io/laysi/trustiot-v2/${name}:1.2.3`;
const CHART_REF = "ghcr.io/laysi/trustiot-v2/charts/trustiot:1.2.3";
const REFUSED = "Error: reading manifest 1.2.3: unauthorized: authentication required";

describe("classifyAnonymousPull", () => {
  test("a refused anonymous request means private", () => {
    expect(classifyAnonymousPull("identity", REF("identity"), 1, REFUSED).state).toBe("private");
    expect(classifyAnonymousPull("identity", REF("identity"), 1, "requested access to the resource is denied").state).toBe("private");
  });

  test("a successful anonymous request means public", () => {
    expect(classifyAnonymousPull("identity", REF("identity"), 0, "").state).toBe("public");
  });

  test("any other failure means the visibility is unknown, with the first line of the answer", () => {
    const result = classifyAnonymousPull("identity", REF("identity"), 1, "dial tcp: lookup ghcr.io: no such host\nmore\n");
    expect(result).toEqual({ name: "identity", ref: REF("identity"), state: "unknown", detail: "dial tcp: lookup ghcr.io: no such host" });
    expect(classifyAnonymousPull("identity", REF("identity"), 2, "").detail).toBe("skopeo exited with status 2");
    expect(classifyAnonymousPull("identity", REF("identity"), 1, "x".repeat(500)).detail).toHaveLength(200);
  });
});

describe("privacyError", () => {
  const privateResult = (name: string): PackagePrivacy => ({ name, ref: REF(name), state: "private" });

  test("every package private: no error", () => {
    const results = [privateResult("identity"), privateResult("postgres"), { name: "trustiot", ref: CHART_REF, state: "private" as const }];
    expect(privacyError(results)).toBeUndefined();
    expect(privacySummary(results)).toBeUndefined();
  });

  test("local test mode skips the check: no error", () => {
    const results: PackagePrivacy[] = [
      { name: "identity", ref: "127.0.0.1:5068/laysi/trustiot-v2/identity:1.2.3", state: "skipped" },
      { name: "trustiot", ref: "127.0.0.1:5068/laysi/trustiot-v2/charts/trustiot:1.2.3", state: "skipped" },
    ];
    expect(privacyError(results)).toBeUndefined();
    expect(privacySummary(results)).toBeUndefined();
    expect(results.map(privacyNote)).toEqual(["。", "。"]);
  });

  test("one public package fails and is named", () => {
    const results: PackagePrivacy[] = [privateResult("identity"), { name: "postgres", ref: REF("postgres"), state: "public" }];
    const error = privacyError(results);
    expect(error?.message).toStartWith(`postgres can be pulled anonymously (${REF("postgres")}); the packages must be private`);
    expect(error?.message).not.toContain(REF("identity"));
    expect(privacySummary(results)).toContain(`\`${REF("postgres")}\`（可以匿名拉取）`);
    expect(privacyNote(results[1]!)).toContain("未通過：可以匿名拉取");
    expect(privacyNote(results[0]!)).toBe("；匿名拉取被拒（套件為私有）。");
  });

  test("several public packages are all named, before anything else", () => {
    const results: PackagePrivacy[] = [
      { name: "identity", ref: REF("identity"), state: "public" },
      privateResult("release-selftest"),
      { name: "postgres", ref: REF("postgres"), state: "public" },
      { name: "trustiot", ref: CHART_REF, state: "public" },
    ];
    const message = privacyError(results)?.message ?? "";
    expect(message).toStartWith("identity, postgres, trustiot can be pulled anonymously");
    for (const ref of [REF("identity"), REF("postgres"), CHART_REF]) {
      expect(message).toContain(ref);
    }
    expect(message).not.toContain(REF("release-selftest"));
  });

  test("an unknown visibility fails too, and is named with the registry's answer", () => {
    const results: PackagePrivacy[] = [
      { name: "trustiot", ref: CHART_REF, state: "unknown", detail: "connection reset by peer" },
      { name: "identity", ref: REF("identity"), state: "public" },
    ];
    const message = privacyError(results)?.message ?? "";
    expect(message).toStartWith("identity can be pulled anonymously");
    expect(message).toContain(`cannot tell whether ${CHART_REF} (connection reset by peer) is private`);
    expect(privacySummary(results)).toContain(`\`${CHART_REF}\`（無法判斷是否私有）`);
    expect(privacyNote(results[0]!)).toContain("未通過：無法判斷套件是否私有");
    const onlyUnknown = privacyError([results[0]!]);
    expect(onlyUnknown?.message).toStartWith(`cannot tell whether ${CHART_REF}`);
  });
});
