// The shared rules of scripts/demo/lib.ts, without a cluster or ssh (Laysi/trustiot-v2#457,
// Laysi/trustiot-v2#465): quoting, confirmation, the lab lock, records and the ops tunnel.
import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { chmodSync, mkdirSync, mkdtempSync, readFileSync, rmSync, statSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import type { RunResult } from "../release/exec.ts";
import {
  Box,
  Log,
  type OpsTunnel,
  checkConfirmation,
  clusterState,
  confirmCode,
  lockOwner,
  opsTunnelUnchanged,
  ownsLock,
  shq,
  stableJson,
  stateDiff,
  takeLock,
  withOpsTunnel,
} from "./lib.ts";

const ISSUE = "Laysi/trustiot-v2#465";
const ok = (stdout = ""): RunResult => ({ stdout, stderr: "", exitCode: 0 });
const dropped: RunResult = { stdout: "", stderr: "Connection closed by remote host", exitCode: 255 };

/**
 * The lab lock as the demo machine's shell would treat it: `owner` is the content of
 * ~/e2e.lock/owner (undefined: no lock directory). `drop` makes chosen commands exit 255, before
 * (`before`) or after (`after`) their effect.
 */
class FakeLockHost {
  owner: string | undefined;
  readonly commands: string[] = [];
  drop: { match: RegExp; when: "before" | "after"; times: number } | undefined;

  readonly run = async (cmd: string[]): Promise<RunResult> => {
    const command = cmd.at(-1)!;
    this.commands.push(command);
    const drop = this.drop !== undefined && this.drop.times > 0 && this.drop.match.test(command) ? this.drop : undefined;
    if (drop !== undefined) drop.times -= 1;
    if (drop?.when === "before") return dropped;
    const result = this.effect(command);
    return drop?.when === "after" ? dropped : result;
  };

  private effect(command: string): RunResult {
    // The owner line is the quoted argument that is not printf's format.
    const quoted = [...command.matchAll(/'([^']*)'/g)].map((m) => m[1]).find((q) => q !== "%s\\n");
    if (command.startsWith("mkdir ")) {
      if (this.owner !== undefined) return ok(`HELD ${this.owner}`);
      this.owner = quoted!;
      return ok("ACQUIRED");
    }
    if (command.startsWith("if [ ")) {
      if (this.owner === quoted) {
        this.owner = undefined;
        return ok("RELEASED");
      }
      return ok(`NOT_OURS ${this.owner ?? ""}`);
    }
    if (command.startsWith("cat ")) return ok(this.owner === undefined ? "" : `${this.owner}\n`);
    throw new Error(`unexpected command: ${command}`);
  }
}

describe("lib", () => {
  let dir: string;
  let log: Log;

  beforeEach(() => {
    dir = mkdtempSync(join(tmpdir(), "demo-lib-"));
    log = new Log("test", dir);
  });
  afterEach(() => rmSync(dir, { recursive: true, force: true }));

  const box = (host: FakeLockHost | ((cmd: string[]) => Promise<RunResult>)) =>
    new Box("demo-box", log, { run: typeof host === "function" ? host : host.run, sleep: async () => {} });

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
    const plan = { delete: ["namespace/poc"] };
    const base = { sshHost: "box", lockHeld: false };
    expect(checkConfirmation(log, plan, { ...base, apply: false, confirm: "" }, "cmd")).toBe(false);
    expect(readFileSync(log.file, "utf8")).toContain(`--apply --confirm ${confirmCode(plan)}`);
    expect(() => checkConfirmation(log, plan, { ...base, apply: true, confirm: "" }, "cmd")).toThrow(/needs --confirm/);
    expect(() => checkConfirmation(log, plan, { ...base, apply: true, confirm: "000000000000" }, "cmd")).toThrow(/plan changed/);
    expect(checkConfirmation(log, plan, { ...base, apply: true, confirm: confirmCode(plan) }, "cmd")).toBe(true);
  });

  test("the state diff names what came and went", () => {
    const before = { namespaces: { names: ["a", "b"], sha256: "1" }, crds: { names: ["x"], sha256: "2" } };
    const after = { namespaces: { names: ["a", "c"], sha256: "3" }, crds: { names: ["x"], sha256: "2" } };
    expect(stateDiff(before, after)).toEqual({ namespaces: { added: ["c"], removed: ["b"], changed: true } });
  });

  describe("Box", () => {
    test("accepts only an ssh host name or alias, never an option or a command", () => {
      for (const bad of ["", "-oProxyCommand=touch x", "a b", "host;id", "$(id)", "user@host"]) {
        expect(() => new Box(bad, log)).toThrow(/ssh host name or alias/);
      }
      expect(new Box("demo-box_1.lan", log).host).toBe("demo-box_1.lan");
    });

    test("quotes every kubectl argument, so the remote shell sees exactly the arguments given", async () => {
      const seen: string[][] = [];
      const b = box(async (cmd) => {
        seen.push(cmd);
        return ok();
      });
      const args = ["get", "a b", "$(touch /tmp/x)", "it's", "`id`", "jsonpath={.items[*]}{\"\\t\"}", "--x=;rm -rf ~"];
      await b.tryKubectl(args);
      expect(seen[0]?.slice(0, 4)).toEqual(["ssh", "-o", "BatchMode=yes", "demo-box"]);
      const remote = seen[0]![4]!;
      expect(remote.startsWith("sudo k3s kubectl ")).toBe(true);
      // Run the same line through a POSIX shell with printf in kubectl's place.
      const echoed = Bun.spawnSync(["sh", "-c", remote.replace(/^sudo k3s kubectl /, "printf '%s\\n' ")]);
      expect(echoed.stdout.toString().split("\n").slice(0, -1)).toEqual(args);
    });

    test("repeats a command only when ssh dropped and the command is safe twice", async () => {
      let calls = 0;
      const b = box(async () => {
        calls += 1;
        return dropped;
      });
      expect((await b.sh("x")).exitCode).toBe(255);
      expect(calls).toBe(4);
      calls = 0;
      await b.sh("x", { retry: false });
      expect(calls).toBe(1);
    });
  });

  describe("the lab lock", () => {
    test("the owner line is `<issue> <UTC time> demo <step>`, and only an issue reference may own it", () => {
      expect(lockOwner(ISSUE, "cleanup", new Date("2026-10-20T07:00:00.123Z"))).toBe("Laysi/trustiot-v2#465 2026-10-20T07:00:00Z demo cleanup");
      expect(() => lockOwner("#465", "x")).toThrow(/not an issue reference/);
      expect(() => lockOwner("Laysi/trustiot-v2#465 x", "x")).toThrow(/not an issue reference/);
    });

    test("ownership is the exact issue followed by a space", () => {
      expect(ownsLock("Laysi/trustiot-v2#465 2026-10-20T07:00:00Z demo cleanup", ISSUE)).toBe(true);
      expect(ownsLock("Laysi/trustiot-v2#465 2026-10-20T07:00:00Z", ISSUE)).toBe(true);
      expect(ownsLock("Laysi/trustiot-v2#4650 2026-10-20T07:00:00Z", ISSUE)).toBe(false);
      expect(ownsLock("Laysi/trustiot-v2#465x 2026-10-20T07:00:00Z", ISSUE)).toBe(false);
      expect(ownsLock("Laysi/trustiot-v2#4570 2026-10-20T07:00:00Z", "Laysi/trustiot-v2#457")).toBe(false);
      expect(ownsLock("Laysi/trustiot-v2#457 2026-10-20T07:00:00Z demo flux", ISSUE)).toBe(false);
      expect(ownsLock("Laysi/trustiot-v2#465", ISSUE)).toBe(false);
      expect(ownsLock("", ISSUE)).toBe(false);
    });

    test("--lock-held accepts only this issue's own lock", async () => {
      const host = new FakeLockHost();
      for (const owner of [
        "Laysi/trustiot-v2#4650 2026-10-20T07:00:00Z",
        "Laysi/trustiot-v2#457 2026-10-20T07:00:00Z demo flux",
        "Laysi/trustiot-v2#465x 2026-10-20T07:00:00Z",
        undefined,
        "",
      ]) {
        host.owner = owner;
        await expect(takeLock(box(host), ISSUE, "cleanup", true)).rejects.toThrow(/--lock-held, but the lab lock is/);
      }
      host.owner = "Laysi/trustiot-v2#465 2026-10-20T07:00:00Z by hand";
      const release = await takeLock(box(host), ISSUE, "cleanup", true);
      await release();
      expect(host.owner).toBe("Laysi/trustiot-v2#465 2026-10-20T07:00:00Z by hand");
      expect(host.commands.some((c) => c.startsWith("mkdir") || c.startsWith("if [ "))).toBe(false);
    });

    test("takes a free lock once, refuses a held one, and releases only its own", async () => {
      const host = new FakeLockHost();
      const release = await takeLock(box(host), ISSUE, "cleanup", false);
      expect(ownsLock(host.owner!, ISSUE)).toBe(true);
      expect(host.owner).toMatch(/ demo cleanup$/);
      await expect(takeLock(box(host), "Laysi/trustiot-v2#466", "tunnel", false)).rejects.toThrow(/held by: Laysi\/trustiot-v2#465 /);
      // Someone replaced the lock meanwhile: it stays.
      host.owner = "Laysi/trustiot-v2#466 2026-10-20T08:00:00Z demo tunnel";
      await release();
      expect(host.owner).toBe("Laysi/trustiot-v2#466 2026-10-20T08:00:00Z demo tunnel");
      expect(readFileSync(log.file, "utf8")).toContain("no longer this run's");
    });

    test("never repeats the mkdir; after a dropped connection it reads the owner line back", async () => {
      const host = new FakeLockHost();
      host.drop = { match: /^mkdir /, when: "after", times: 1 };
      const release = await takeLock(box(host), ISSUE, "cleanup", false);
      expect(host.commands.filter((c) => c.startsWith("mkdir"))).toHaveLength(1);
      expect(readFileSync(log.file, "utf8")).toContain("took the lab lock");
      await release();
      expect(host.owner).toBeUndefined();

      const other = new FakeLockHost();
      other.drop = { match: /^mkdir /, when: "before", times: 1 };
      await expect(takeLock(box(other), ISSUE, "cleanup", false)).rejects.toThrow(/ssh dropped while taking the lab lock; it now reads: \(no owner line\)/);
      expect(other.commands.filter((c) => c.startsWith("mkdir"))).toHaveLength(1);
      expect(other.owner).toBeUndefined();
    });

    test("a release that ssh cannot deliver says so and how to clean up, never that the lock is someone else's", async () => {
      const host = new FakeLockHost();
      const release = await takeLock(box(host), ISSUE, "cleanup", false);
      host.drop = { match: /^if \[ /, when: "before", times: 10 };
      await release();
      const text = readFileSync(log.file, "utf8");
      expect(text).toContain("WARNING: releasing the lab lock failed (ssh)");
      expect(text).toContain("rm -rf ~/e2e.lock");
      expect(text).not.toContain("no longer this run's");
      expect(text).not.toContain("not ours");
    });
  });

  describe("records", () => {
    test("a log directory it creates is 0700, its files 0600", () => {
      const fresh = join(dir, "a", "b");
      const l = new Log("x", fresh);
      expect(statSync(fresh).mode & 0o777).toBe(0o700);
      expect(statSync(l.file).mode & 0o777).toBe(0o600);
      expect(statSync(l.save("plan", { a: 1 })).mode & 0o777).toBe(0o600);
    });

    test("an existing directory is never chmod-ed; one other users can open is refused", () => {
      const open = join(dir, "open");
      mkdirSync(open);
      chmodSync(open, 0o755);
      expect(() => new Log("x", open)).toThrow(/open to other users \(mode 755\)/);
      expect(statSync(open).mode & 0o777).toBe(0o755);
      const closed = join(dir, "closed");
      mkdirSync(closed, { mode: 0o700 });
      chmodSync(closed, 0o700);
      expect(new Log("x", closed).dir).toBe(closed);
    });

    test("the cluster state keeps each ClusterRoleBinding's subjects", async () => {
      const b = box(async (cmd) => {
        const command = cmd.at(-1)!;
        if (command.includes("clusterrolebindings")) {
          return ok(JSON.stringify({ items: [{ metadata: { name: "poc-read" }, subjects: [{ kind: "ServiceAccount", namespace: "poc", name: "probe-a" }, { kind: "User", name: "alice" }] }] }));
        }
        return ok(JSON.stringify({ items: [] }));
      });
      const state = await clusterState(b);
      expect(state["clusterrolebindings.rbac.authorization.k8s.io"]?.subjects).toEqual({ "poc-read": ["ServiceAccount:poc/probe-a", "User:alice"] });
    });
  });

  describe("the ops tunnel", () => {
    const tunnel = (overrides: Partial<OpsTunnel> = {}): OpsTunnel => ({ active: "active", pid: "4242", restarts: "0", started: "Mon 2026-10-19 10:00:00 CST", ...overrides });

    test("a run does not start while the tunnel is down", async () => {
      let ran = false;
      await expect(
        withOpsTunnel(async () => tunnel({ active: "failed" }), log, async () => {
          ran = true;
        }),
      ).rejects.toThrow(/cloudflared.service is failed/);
      expect(ran).toBe(false);
    });

    test("a restart is noticed even when the PID comes back, also when the run fails", async () => {
      expect(opsTunnelUnchanged(tunnel(), tunnel())).toContain("kept running (same process)");
      for (const after of [tunnel({ pid: "5000" }), tunnel({ restarts: "1" }), tunnel({ started: "Tue 2026-10-20 15:00:00 CST" }), tunnel({ active: "activating" })]) {
        expect(opsTunnelUnchanged(tunnel(), after)).toMatch(/^WARNING: cloudflared.service changed/);
      }
      const reads = [tunnel(), tunnel({ restarts: "1" })];
      await expect(
        withOpsTunnel(async () => reads.shift()!, log, async () => {
          throw new Error("mid-run failure");
        }),
      ).rejects.toThrow("mid-run failure");
      expect(readFileSync(log.file, "utf8")).toContain("WARNING: cloudflared.service changed");
    });
  });
});
