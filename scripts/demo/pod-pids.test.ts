// The edit, the plan and the run of scripts/demo/pod-pids.ts against a fake demo machine, and its
// remote scripts with bash on a temporary directory, without ssh (Laysi/trustiot-v2#881).
import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { chmodSync, existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, statSync, symlinkSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { POD_PIDS_LIMIT } from "../nightly/dashboard-check.ts";
import { type CommonOptions, Log, confirmCode, sha256 } from "./lib.ts";
import {
  BACKUP_FILE,
  CONFIG_FILE,
  type Facts,
  type HostFacts,
  KUBELET_ARG,
  NEW_BLOCK,
  type PodPidsOps,
  RESTART_WAIT_MS,
  changeOf,
  limitsLine,
  factsScript,
  parseHostFacts,
  planConfigEdit,
  planPodPids,
  planRollback,
  runPodPids,
  writeScript,
} from "./pod-pids.ts";

/** The demo machine's config.yaml on 2026-11-01. */
const DEMO_CONFIG = "disable:\n  - traefik\n";

describe("planConfigEdit", () => {
  test("appends the kubelet-arg block when the file has none (the demo machine's file)", () => {
    const edit = planConfigEdit(DEMO_CONFIG);
    expect(edit).toEqual({
      kind: "edit",
      text: `${DEMO_CONFIG}kubelet-arg:\n  - pod-max-pids=16384\n`,
      added: [
        { line: 3, text: "kubelet-arg:" },
        { line: 4, text: "  - pod-max-pids=16384" },
      ],
    });
    expect(KUBELET_ARG).toBe(`pod-max-pids=${POD_PIDS_LIMIT}`);
  });

  test("adds a newline first when the file does not end with one, and handles an empty file", () => {
    expect(planConfigEdit("disable:\n  - traefik")).toMatchObject({ kind: "edit", text: `disable:\n  - traefik\n${NEW_BLOCK}` });
    expect(planConfigEdit("")).toMatchObject({ kind: "edit", text: NEW_BLOCK, added: [{ line: 1, text: "kubelet-arg:" }, { line: 2, text: "  - pod-max-pids=16384" }] });
  });

  test("merges into an existing kubelet-arg list, keeping every other line and comment", () => {
    const text = "# k3s\nkubelet-arg:\n    - max-pods=50   # tuned\n    - \"eviction-hard=memory.available<100Mi\"\nwrite-kubeconfig-mode: \"0600\"\n";
    const edit = planConfigEdit(text);
    expect(edit).toEqual({
      kind: "edit",
      text: "# k3s\nkubelet-arg:\n    - max-pods=50   # tuned\n    - \"eviction-hard=memory.available<100Mi\"\n    - pod-max-pids=16384\nwrite-kubeconfig-mode: \"0600\"\n",
      added: [{ line: 5, text: "    - pod-max-pids=16384" }],
    });
  });

  test("leaves the file alone when the item is already there", () => {
    expect(planConfigEdit(`${DEMO_CONFIG}${NEW_BLOCK}`)).toEqual({ kind: "present" });
  });

  test("refuses whatever it would clobber or cannot extend safely", () => {
    const refusal = (text: string) => {
      const edit = planConfigEdit(text);
      expect(edit.kind).toBe("refuse");
      return edit.kind === "refuse" ? edit.reason : "";
    };
    expect(refusal("kubelet-arg:\n  - pod-max-pids=4096\n")).toContain("already has pod-max-pids=4096");
    expect(refusal("kubelet-arg:\n  - pod-max-pids=16384\n  - pod-max-pids=4096\n")).toContain("already has");
    expect(refusal("kubelet-arg:\n  - a=b\nkubelet-arg:\n  - c=d\n")).toContain("2 kubelet-arg keys");
    expect(refusal("kubelet-arg+:\n  - a=b\n")).toContain("kubelet-arg+");
    expect(refusal("kubelet-arg: [a=b]\n")).toContain("not a block list");
    expect(refusal("kubelet-arg: a=b\n")).toContain("not a list of strings");
    expect(refusal("kubelet-arg:\n  - a=b\n  # note\n  - c=d\n")).toContain("not a block list of one item per line");
    expect(refusal("kubelet-arg:\n  - a=b\n   - c=d\n")).toMatch(/one indentation|not a block list/);
    expect(refusal("kubelet-arg:\n  - 42\n")).toContain("not a list of strings");
    expect(refusal("- a\n- b\n")).toContain("not one YAML mapping");
    expect(refusal("disable: [traefik\n")).toContain("not valid YAML");
    expect(refusal("disable:\r\n  - traefik\r\n")).toContain("CR line endings");
  });
});

const host = (overrides: Partial<HostFacts> = {}): HostFacts => ({
  config: DEMO_CONFIG,
  configD: [],
  unitMentions: [],
  envFiles: [],
  backupSha256: undefined,
  k3s: { active: "active", pid: "1000" },
  ...overrides,
});
const facts = (overrides: Partial<HostFacts> = {}, limit: number | undefined = -1, extra: Partial<Facts> = {}): Facts => ({
  host: host(overrides),
  limits: { "demo-node": limit },
  dashboardChecks: [],
  ...extra,
});

describe("planPodPids", () => {
  test("the demo machine as it was on 2026-11-01: write the block, back up, restart", () => {
    const plan = planPodPids(facts());
    expect(plan.blockers).toEqual([]);
    expect(plan.restart).toBe(true);
    expect(plan.write).toMatchObject({ fromSha256: sha256(DEMO_CONFIG), toSha256: sha256(`${DEMO_CONFIG}${NEW_BLOCK}`) });
    expect(plan.ok).toContain("kubelet now: the node: podPidsLimit -1");
    expect(limitsLine({ a: -1, b: 16384 })).toBe("node 1: podPidsLimit -1; node 2: podPidsLimit 16384");
    expect(limitsLine({ a: undefined })).toBe("the node: podPidsLimit unreadable");
  });

  test("nothing to do once the kubelet runs with the limit", () => {
    const plan = planPodPids(facts({ config: `${DEMO_CONFIG}${NEW_BLOCK}` }, POD_PIDS_LIMIT));
    expect(plan).toMatchObject({ blockers: [], restart: false });
    expect(plan.write).toBeUndefined();
  });

  test("only a restart when the file already has it but the kubelet does not (an interrupted run)", () => {
    const plan = planPodPids(facts({ config: `${DEMO_CONFIG}${NEW_BLOCK}`, backupSha256: sha256(DEMO_CONFIG) }));
    expect(plan.write).toBeUndefined();
    expect(plan.restart).toBe(true);
    expect(plan.blockers).toEqual([]);
  });

  test("blocks on every other place k3s reads kubelet arguments or a config from, a stray backup, k3s not active and more nodes", () => {
    expect(planPodPids(facts({ configD: ["50-kubelet.yaml"] })).blockers[0]).toContain("config.yaml.d sets kubelet-arg too (50-kubelet.yaml)");
    expect(planPodPids(facts({ unitMentions: ["kubelet-arg"] })).blockers[0]).toContain("the k3s unit names kubelet-arg");
    expect(planPodPids(facts({ envFiles: ["/etc/default/k3s"] })).blockers[0]).toContain("/etc/default/k3s");
    expect(planPodPids(facts({ backupSha256: "0".repeat(64) })).blockers[0]).toContain(`${BACKUP_FILE} exists and is not the file as it is now`);
    expect(planPodPids(facts({ k3s: { active: "activating", pid: "1" } })).blockers[0]).toContain("k3s.service is activating");
    expect(planPodPids(facts({ config: undefined })).blockers).toContain(`${CONFIG_FILE} does not exist: this is not the demo machine's k3s as the runbook describes it`);
    expect(planPodPids({ ...facts(), limits: { a: -1, b: -1 } }).blockers[0]).toContain("2 nodes");
    expect(planPodPids({ ...facts(), limits: { "demo-node": undefined } }).blockers[0]).toContain("could not be read");
    expect(planPodPids(facts({ config: "kubelet-arg:\n  - pod-max-pids=4096\n" })).blockers[0]).toContain("already has pod-max-pids=4096");
    // A backup that is the file as it is now (an earlier run stopped before the rename) is fine.
    expect(planPodPids(facts({ backupSha256: sha256(DEMO_CONFIG) })).blockers).toEqual([]);
  });

  test("the confirmation code covers the file change and the restart", () => {
    const one = confirmCode(changeOf(planPodPids(facts())));
    expect(confirmCode(changeOf(planPodPids(facts())))).toBe(one);
    expect(confirmCode(changeOf(planPodPids(facts({ config: "disable:\n  - servicelb\n" }))))).not.toBe(one);
  });
});

describe("planRollback", () => {
  const applied = `${DEMO_CONFIG}${NEW_BLOCK}`;
  test("restores the backup while the file is exactly what --apply wrote from it", () => {
    const plan = planRollback(facts({ config: applied, backup: DEMO_CONFIG, backupSha256: sha256(DEMO_CONFIG) }, POD_PIDS_LIMIT));
    expect(plan.blockers).toEqual([]);
    expect(plan.write).toMatchObject({ fromSha256: sha256(applied), toSha256: sha256(DEMO_CONFIG), text: DEMO_CONFIG });
    expect(plan.restart).toBe(true);
  });

  test("refuses when the file changed since, there is no backup, or the dashboard check runs", () => {
    expect(planRollback(facts({ config: `${applied}# later\n`, backup: DEMO_CONFIG })).blockers[0]).toContain("is not what --apply writes");
    expect(planRollback(facts({ config: applied })).blockers[0]).toContain("does not exist: --apply never changed this file");
    expect(planRollback(facts({ config: applied, backup: DEMO_CONFIG }, POD_PIDS_LIMIT, { dashboardChecks: ["trustiot/trustiot-dashboard-check"] })).blockers[0]).toContain("switch it off");
  });

  test("only a restart when the file is already the backup but the kubelet still has the limit", () => {
    const plan = planRollback(facts({ config: DEMO_CONFIG, backup: DEMO_CONFIG }, POD_PIDS_LIMIT));
    expect(plan.write).toBeUndefined();
    expect(plan.restart).toBe(true);
    expect(planRollback(facts({ config: DEMO_CONFIG, backup: DEMO_CONFIG }, -1)).restart).toBe(false);
    // An unreadable limit is not taken for the default either.
    expect(planRollback({ ...facts({ config: DEMO_CONFIG, backup: DEMO_CONFIG }), limits: { "demo-node": undefined } }).restart).toBe(true);
  });

  test("refuses while the dashboard check runs, also when only the restart is left", () => {
    const running = { dashboardChecks: ["trustiot/trustiot-dashboard-check"] };
    expect(planRollback(facts({ config: DEMO_CONFIG, backup: DEMO_CONFIG }, POD_PIDS_LIMIT, running)).blockers[0]).toContain("switch it off");
    expect(planRollback(facts({ config: applied, backup: DEMO_CONFIG }, POD_PIDS_LIMIT, running)).blockers[0]).toContain("switch it off");
  });
});

// --- The run against a fake machine -----------------------------------------------------------

interface Machine {
  config: string;
  backup: string | undefined;
  limit: number;
  pid: number;
  active: string;
  apiReady: boolean;
  /** What the kubelet reads after the next restart, unless the test overrides it. */
  limitAfterRestart?: number;
  events: string[];
  lockHeld: boolean;
  now: number;
}

function fakeOps(machine: Machine, overrides: Partial<PodPidsOps> = {}): PodPidsOps {
  const limitFromConfig = () => (machine.config.includes(KUBELET_ARG) ? POD_PIDS_LIMIT : -1);
  return {
    tunnel: async () => ({ active: "active", pid: "7", restarts: "0", started: "t" }),
    async facts(withBackup) {
      return {
        host: {
          config: machine.config,
          configD: [],
          unitMentions: [],
          envFiles: [],
          backupSha256: machine.backup === undefined ? undefined : sha256(machine.backup),
          ...(withBackup ? { backup: machine.backup } : {}),
          k3s: { active: machine.active, pid: String(machine.pid) },
        },
        limits: { "demo-node": machine.limit },
        dashboardChecks: [],
      };
    },
    async health() {
      return { active: machine.active, pid: String(machine.pid), apiReady: machine.apiReady, nodeReady: machine.apiReady };
    },
    async lock(step) {
      machine.events.push(`lock ${step}`);
      machine.lockHeld = true;
      return async () => {
        machine.events.push("unlock");
        machine.lockHeld = false;
      };
    },
    async write(write, backup) {
      expect(machine.lockHeld).toBe(true);
      expect(sha256(machine.config)).toBe(write.fromSha256);
      if (backup) machine.backup ??= machine.config;
      machine.config = write.text;
      machine.events.push(`write ${write.toSha256.slice(0, 8)}`);
      return "written";
    },
    async restartK3s() {
      expect(machine.lockHeld).toBe(true);
      machine.events.push("restart");
      machine.pid += 1;
      machine.limit = machine.limitAfterRestart ?? limitFromConfig();
    },
    now: () => machine.now,
    async sleep(ms) {
      machine.now += ms;
    },
    ...overrides,
  };
}

let dir: string;
beforeEach(() => {
  dir = mkdtempSync(join(tmpdir(), "pod-pids-"));
  chmodSync(dir, 0o700);
});
afterEach(() => rmSync(dir, { recursive: true, force: true }));

const options = (apply: boolean, confirm = ""): CommonOptions => ({ sshHost: "demo", apply, confirm, lockHeld: false });
const machineOf = (overrides: Partial<Machine> = {}): Machine => ({ config: DEMO_CONFIG, backup: undefined, limit: -1, pid: 1000, active: "active", apiReady: true, events: [], lockHeld: false, now: 0, ...overrides });
const quiet = <T>(body: () => Promise<T>): Promise<T> => {
  const log = console.log;
  console.log = () => {};
  return body().finally(() => {
    console.log = log;
  });
};

describe("runPodPids", () => {
  test("a dry run changes nothing and prints the code; --apply with it backs up, writes, restarts and checks the limit", async () => {
    const machine = machineOf();
    const plan = await quiet(() => runPodPids(fakeOps(machine), new Log("pod-pids", dir), options(false), false));
    expect(machine.events).toEqual([]);
    const code = confirmCode(changeOf(plan));
    await quiet(() => runPodPids(fakeOps(machine), new Log("pod-pids", dir), options(true, code), false));
    expect(machine.events).toEqual(["lock pod-pids", `write ${sha256(`${DEMO_CONFIG}${NEW_BLOCK}`).slice(0, 8)}`, "restart", "unlock"]);
    expect(machine.config).toBe(`${DEMO_CONFIG}${NEW_BLOCK}`);
    expect(machine.backup).toBe(DEMO_CONFIG);
    expect(machine.limit).toBe(POD_PIDS_LIMIT);
    // Running again changes nothing.
    machine.events = [];
    await quiet(() => runPodPids(fakeOps(machine), new Log("pod-pids", dir), options(true, code), false));
    expect(machine.events).toEqual([]);
  });

  test("--apply refuses a wrong code, and a plan that changed under the lock, before changing anything", async () => {
    const machine = machineOf();
    await expect(quiet(() => runPodPids(fakeOps(machine), new Log("pod-pids", dir), options(true, "000000000000"), false))).rejects.toThrow("the plan changed since it was shown");
    const code = confirmCode(changeOf(planPodPids(await fakeOps(machine).facts(false))));
    let reads = 0;
    const ops = fakeOps(machine, {
      async facts(withBackup) {
        reads += 1;
        const facts = await fakeOps(machine).facts(withBackup);
        // Someone edits the file between the dry plan and the plan under the lock.
        return reads === 1 ? facts : { ...facts, host: { ...facts.host, config: `${DEMO_CONFIG}# edited\n` } };
      },
    });
    await expect(quiet(() => runPodPids(ops, new Log("pod-pids", dir), options(true, code), false))).rejects.toThrow("changed since the plan was made");
    expect(machine.events).toEqual(["lock pod-pids", "unlock"]);
    expect(machine.config).toBe(DEMO_CONFIG);
  });

  test("--apply refuses while a blocker exists", async () => {
    const machine = machineOf({ config: "kubelet-arg:\n  - pod-max-pids=4096\n" });
    await expect(quiet(() => runPodPids(fakeOps(machine), new Log("pod-pids", dir), options(true, "x"), false))).rejects.toThrow("blockers above; nothing changed");
    expect(machine.events).toEqual([]);
  });

  test("fails, naming the backup and --rollback, when the kubelet does not take the limit; releases the lock", async () => {
    const machine = machineOf({ limitAfterRestart: -1 });
    const code = confirmCode(changeOf(planPodPids(await fakeOps(machine).facts(false))));
    await expect(quiet(() => runPodPids(fakeOps(machine), new Log("pod-pids", dir), options(true, code), false))).rejects.toThrow(/does not run with podPidsLimit 16384 .*--rollback/);
    expect(machine.events.at(-1)).toBe("unlock");
  });

  test("waits out failed reads after the restart, and fails when the kubelet never reads back", async () => {
    const machine = machineOf();
    const code = confirmCode(changeOf(planPodPids(await fakeOps(machine).facts(false))));
    let failing = false;
    let failures = 0;
    const base = fakeOps(machine);
    const ops = fakeOps(machine, {
      async restartK3s() {
        await base.restartK3s();
        failing = true;
      },
      async facts(withBackup) {
        if (failing && failures < 3) {
          failures += 1;
          throw new Error("the API server is not ready");
        }
        return base.facts(withBackup);
      },
    });
    await quiet(() => runPodPids(ops, new Log("pod-pids", dir), options(true, code), false));
    expect(failures).toBe(3);
    expect(machine.limit).toBe(POD_PIDS_LIMIT);

    const never = machineOf();
    const neverCode = confirmCode(changeOf(planPodPids(await fakeOps(never).facts(false))));
    const neverBase = fakeOps(never);
    let restarted = false;
    const unreadable = fakeOps(never, {
      async restartK3s() {
        await neverBase.restartK3s();
        restarted = true;
      },
      async facts(withBackup) {
        if (restarted) throw new Error("configz unreadable");
        return neverBase.facts(withBackup);
      },
    });
    await expect(quiet(() => runPodPids(unreadable, new Log("pod-pids", dir), options(true, neverCode), false))).rejects.toThrow("does not run with podPidsLimit 16384");
  });

  test("fails after RESTART_WAIT_MS when k3s does not come back", async () => {
    const machine = machineOf();
    const code = confirmCode(changeOf(planPodPids(await fakeOps(machine).facts(false))));
    const ops = fakeOps(machine, {
      async restartK3s() {
        machine.events.push("restart");
        machine.active = "activating";
      },
    });
    await expect(quiet(() => runPodPids(ops, new Log("pod-pids", dir), options(true, code), false))).rejects.toThrow(`timed out after ${RESTART_WAIT_MS / 1000} s waiting for k3s to come back`);
    expect(machine.now).toBeGreaterThan(RESTART_WAIT_MS);
    expect(machine.events.at(-1)).toBe("unlock");
  });

  test("--rollback puts the backup back and restarts", async () => {
    const machine = machineOf({ config: `${DEMO_CONFIG}${NEW_BLOCK}`, backup: DEMO_CONFIG, limit: POD_PIDS_LIMIT });
    const code = confirmCode(changeOf(planRollback(await fakeOps(machine).facts(true))));
    await quiet(() => runPodPids(fakeOps(machine), new Log("pod-pids", dir), options(true, code), true));
    expect(machine.config).toBe(DEMO_CONFIG);
    expect(machine.limit).toBe(-1);
    expect(machine.events).toEqual(["lock pod-pids-rollback", `write ${sha256(DEMO_CONFIG).slice(0, 8)}`, "restart", "unlock"]);
  });

  test("--rollback fails unless the kubelet reads back its default limit", async () => {
    const machine = machineOf({ config: `${DEMO_CONFIG}${NEW_BLOCK}`, backup: DEMO_CONFIG, limit: POD_PIDS_LIMIT, limitAfterRestart: 4096 });
    const code = confirmCode(changeOf(planRollback(await fakeOps(machine).facts(true))));
    await expect(quiet(() => runPodPids(fakeOps(machine), new Log("pod-pids", dir), options(true, code), true))).rejects.toThrow("does not run with its default podPidsLimit -1");
    expect(machine.events.at(-1)).toBe("unlock");
  });
});

// --- The remote scripts, with bash on a temporary directory -------------------------------------

function bash(script: string): { code: number; stdout: string; stderr: string } {
  const result = Bun.spawnSync(["bash", "-s"], { stdin: new TextEncoder().encode(script), stdout: "pipe", stderr: "pipe" });
  return { code: result.exitCode ?? -1, stdout: result.stdout.toString(), stderr: result.stderr.toString() };
}

describe("the remote scripts", () => {
  test("factsScript reads the file, the backup and config.yaml.d, and parseHostFacts decodes them", () => {
    const etc = join(dir, "k3s");
    mkdirSync(join(etc, "config.yaml.d"), { recursive: true });
    writeFileSync(join(etc, "config.yaml"), "token: never-printed\ndisable:\n  - traefik\n");
    writeFileSync(join(etc, "config.yaml.d", "10-other.yaml"), "kubelet-arg:\n  - a=b\n");
    writeFileSync(join(etc, "config.yaml.d", "20-plain.yaml"), "disable: []\n");
    const out = bash(factsScript(etc, false));
    expect(out.code).toBe(0);
    expect(out.stdout).not.toContain("never-printed");
    const facts = parseHostFacts(out.stdout);
    expect(facts.config).toBe("token: never-printed\ndisable:\n  - traefik\n");
    expect(facts.configD).toEqual(["10-other.yaml"]);
    expect(facts.backupSha256).toBeUndefined();
    writeFileSync(join(etc, "config.yaml.pre-881"), "old\n");
    const withBackup = parseHostFacts(bash(factsScript(etc, true)).stdout);
    expect(withBackup.backupSha256).toBe(sha256("old\n"));
    expect(withBackup.backup).toBe("old\n");
    expect(() => parseHostFacts("config=ABSENT\n")).toThrow("stopped early");
  });

  test("writeScript backs up, swaps in the planned bytes with the old file's mode, and is a compare-and-swap", () => {
    const etc = join(dir, "k3s");
    mkdirSync(etc);
    const file = join(etc, "config.yaml");
    writeFileSync(file, DEMO_CONFIG);
    chmodSync(file, 0o640);
    const plan = planPodPids(facts());
    const write = plan.write!;
    expect(writeScript(etc, write, true)).not.toContain("pod-max-pids");
    const first = bash(writeScript(etc, write, true));
    expect(first).toMatchObject({ code: 0, stdout: "written\n" });
    expect(readFileSync(file, "utf8")).toBe(`${DEMO_CONFIG}${NEW_BLOCK}`);
    expect(readFileSync(`${file}.pre-881`, "utf8")).toBe(DEMO_CONFIG);
    expect(statSync(file).mode & 0o777).toBe(0o640);
    expect(statSync(`${file}.pre-881`).mode & 0o777).toBe(0o640);
    expect(existsSync(`${file}.new-881`)).toBe(false);
    // Again: already written, nothing changes.
    expect(bash(writeScript(etc, write, true))).toMatchObject({ code: 0, stdout: "already written\n" });
    // Rolling back restores the backup byte for byte.
    const back = planRollback(facts({ config: readFileSync(file, "utf8"), backup: DEMO_CONFIG }, POD_PIDS_LIMIT)).write!;
    expect(bash(writeScript(etc, back, false))).toMatchObject({ code: 0, stdout: "written\n" });
    expect(readFileSync(file, "utf8")).toBe(DEMO_CONFIG);
  });

  test("writeScript changes nothing when the file or the backup is not what the plan saw", () => {
    const etc = join(dir, "k3s");
    mkdirSync(etc);
    const file = join(etc, "config.yaml");
    const write = planPodPids(facts()).write!;
    writeFileSync(file, `${DEMO_CONFIG}# edited\n`);
    const edited = bash(writeScript(etc, write, true));
    expect(edited.code).toBe(3);
    expect(edited.stderr).toContain("changed since the plan");
    expect(readFileSync(file, "utf8")).toBe(`${DEMO_CONFIG}# edited\n`);
    expect(existsSync(`${file}.pre-881`)).toBe(false);
    writeFileSync(file, DEMO_CONFIG);
    writeFileSync(`${file}.pre-881`, "something else\n");
    const stray = bash(writeScript(etc, write, true));
    expect(stray.code).toBe(3);
    expect(stray.stderr).toContain("exists and is not the file as it is now");
    expect(readFileSync(file, "utf8")).toBe(DEMO_CONFIG);
    expect(readFileSync(`${file}.pre-881`, "utf8")).toBe("something else\n");
  });

  test("a symbolic link is reported by factsScript and refused by writeScript, whose write never reaches its target", () => {
    const etc = join(dir, "k3s");
    mkdirSync(etc);
    const target = join(dir, "elsewhere.yaml");
    writeFileSync(target, DEMO_CONFIG);
    symlinkSync(target, join(etc, "config.yaml"));
    expect(parseHostFacts(bash(factsScript(etc, false)).stdout).symlink).toBe(true);
    expect(planPodPids(facts({ symlink: true })).blockers[0]).toContain("symbolic link");
    const write = planPodPids(facts()).write!;
    const linked = bash(writeScript(etc, write, true));
    expect(linked.code).toBe(3);
    expect(linked.stderr).toContain("is a symbolic link");
    expect(readFileSync(target, "utf8")).toBe(DEMO_CONFIG);
    // A planted link where the new file goes is refused the same way.
    rmSync(join(etc, "config.yaml"));
    writeFileSync(join(etc, "config.yaml"), DEMO_CONFIG);
    symlinkSync(target, join(etc, "config.yaml.new-881"));
    expect(bash(writeScript(etc, write, true)).code).toBe(3);
    expect(readFileSync(target, "utf8")).toBe(DEMO_CONFIG);
    expect(parseHostFacts(bash(factsScript(etc, false)).stdout).symlink).toBe(false);
  });
});
