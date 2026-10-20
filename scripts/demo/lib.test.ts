// The shared rules of scripts/demo/lib.ts, without a cluster or ssh (Laysi/trustiot-v2#457).
import { describe, expect, test } from "bun:test";
import { mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { Log, checkConfirmation, confirmCode, shq, stableJson, stateDiff } from "./lib.ts";

describe("lib", () => {
  test("shq quotes everything the remote shell would interpret", () => {
    expect(shq("plain-arg_1.yaml")).toBe("plain-arg_1.yaml");
    expect(shq("a b")).toBe("'a b'");
    expect(shq("it's $HOME `x`")).toBe(`'it'\\''s $HOME \`x\`'`);
    expect(shq("")).toBe("''");
  });

  test("a plan's confirmation code does not depend on key order and changes with the plan", () => {
    expect(stableJson({ b: 1, a: [{ d: 2, c: 3 }] })).toBe('{"a":[{"c":3,"d":2}],"b":1}');
    expect(confirmCode({ a: 1, b: 2 })).toBe(confirmCode({ b: 2, a: 1 }));
    expect(confirmCode({ a: 1 })).not.toBe(confirmCode({ a: 2 }));
    expect(confirmCode({ a: 1 })).toMatch(/^[0-9a-f]{12}$/);
  });

  test("--apply needs the code of the plan that was shown", () => {
    const dir = mkdtempSync(join(tmpdir(), "demo-log-"));
    try {
      const log = new Log("test", dir);
      const plan = { delete: ["namespace/poc"] };
      const base = { sshHost: "box", lockHeld: false };
      expect(checkConfirmation(log, plan, { ...base, apply: false, confirm: "" }, "cmd")).toBe(false);
      expect(readFileSync(log.file, "utf8")).toContain(`--apply --confirm ${confirmCode(plan)}`);
      expect(() => checkConfirmation(log, plan, { ...base, apply: true, confirm: "" }, "cmd")).toThrow(/needs --confirm/);
      expect(() => checkConfirmation(log, plan, { ...base, apply: true, confirm: "000000000000" }, "cmd")).toThrow(/plan changed/);
      expect(checkConfirmation(log, plan, { ...base, apply: true, confirm: confirmCode(plan) }, "cmd")).toBe(true);
    } finally {
      rmSync(dir, { recursive: true, force: true });
    }
  });

  test("the state diff names what came and went", () => {
    const before = { namespaces: { names: ["a", "b"], sha256: "1" }, crds: { names: ["x"], sha256: "2" } };
    const after = { namespaces: { names: ["a", "c"], sha256: "3" }, crds: { names: ["x"], sha256: "2" } };
    expect(stateDiff(before, after)).toEqual({ namespaces: { added: ["c"], removed: ["b"], changed: true } });
  });
});
