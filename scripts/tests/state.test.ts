import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import {
  existsSync,
  mkdirSync,
  mkdtempSync,
  readdirSync,
  rmSync,
  statSync,
  utimesSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { registerCleanup, runCleanups } from "../lib/cleanup.ts";
import { generateCredentials } from "../lib/secrets.ts";
import {
  acquireLock,
  ensurePrivateDir,
  loadState,
  removeStateExcept,
  resolveInstance,
  saveState,
  stateDir,
  stateRoot,
  withPrivateFile,
  writePrivateFile,
  type State,
} from "../lib/state.ts";

let scratch: string;

beforeEach(() => {
  scratch = mkdtempSync(join(tmpdir(), "trustiot-dev-test-"));
});

afterEach(() => {
  rmSync(scratch, { recursive: true, force: true });
});

const mode = (path: string) => statSync(path).mode & 0o777;

describe("paths", () => {
  test("follow XDG_STATE_HOME and fall back to ~/.local/state", () => {
    expect(stateRoot({ XDG_STATE_HOME: "/x/state" })).toBe("/x/state/trustiot-v2/dev");
    expect(stateRoot({ HOME: "/home/a" })).toBe("/home/a/.local/state/trustiot-v2/dev");
    // A relative XDG_STATE_HOME is invalid per the spec and ignored.
    expect(stateRoot({ XDG_STATE_HOME: "relative", HOME: "/home/a" })).toBe(
      "/home/a/.local/state/trustiot-v2/dev",
    );
    expect(stateDir({ HOME: "/home/a" }, "one")).toBe("/home/a/.local/state/trustiot-v2/dev/one");
  });

  test("never point into the repository", () => {
    expect(stateRoot({ HOME: "/home/a" })).not.toContain("trustiot-v2/scripts");
  });
});

describe("resolveInstance", () => {
  test("defaults and validates", () => {
    expect(resolveInstance({})).toBe("default");
    expect(resolveInstance({ TRUSTIOT_DEV_INSTANCE: "pr-12" })).toBe("pr-12");
    for (const bad of ["", "Up", "1a", "a_b", "a b", "../x", "a".repeat(25)]) {
      expect(() => resolveInstance({ TRUSTIOT_DEV_INSTANCE: bad })).toThrow();
    }
  });
});

describe("private files", () => {
  test("directories are 0700 and files 0600, whatever the umask", () => {
    const previous = process.umask(0o000);
    try {
      const dir = join(scratch, "a", "b");
      ensurePrivateDir(dir);
      expect(mode(dir)).toBe(0o700);
      const file = join(dir, "secret");
      writePrivateFile(file, "value");
      expect(mode(file)).toBe(0o600);
    } finally {
      process.umask(previous);
    }
  });

  test("an overwrite is atomic and leaves no temporary file", () => {
    const file = join(scratch, "secret");
    writePrivateFile(file, "one");
    writePrivateFile(file, "two");
    expect(readdirSync(scratch)).toEqual(["secret"]);
    expect(Bun.file(file).size).toBe(3);
  });

  test("withPrivateFile removes the file even when the action fails", async () => {
    const dir = join(scratch, "run");
    let seen = "";
    await expect(
      withPrivateFile(dir, "x.env", "A=1\n", async (path) => {
        seen = path;
        expect(mode(path)).toBe(0o600);
        throw new Error("boom");
      }),
    ).rejects.toThrow("boom");
    expect(existsSync(seen)).toBe(false);
  });
});

describe("interrupts", () => {
  test("runCleanups removes the secret file and the lock held by a running command", async () => {
    const release = acquireLock(scratch, "t");
    let seen = "";
    await withPrivateFile(join(scratch, "run"), "x.env", "A=1\n", async (path) => {
      seen = path;
      expect(existsSync(path)).toBe(true);
      runCleanups(); // what the SIGINT and SIGTERM handlers do
      expect(existsSync(path)).toBe(false);
    });
    expect(existsSync(join(scratch, "t.lock"))).toBe(false);
    release(); // releasing after an interrupt is harmless
    expect(seen).not.toBe("");
  });

  test("a finished cleanup is not run again", () => {
    let runs = 0;
    const forget = registerCleanup(() => runs++);
    forget();
    runCleanups();
    expect(runs).toBe(0);
  });
});

describe("state file", () => {
  const state = (): State => ({
    version: 2,
    instance: "t",
    startedAt: "2026-10-10T00:00:00.000Z",
    ports: { postgres: 1, clickhouse: 2, mqtt: 3, services: {} },
    credentials: generateCredentials(),
  });

  test("round-trips and is private", () => {
    const dir = join(scratch, "t");
    const saved = state();
    saveState(dir, saved);
    expect(mode(dir)).toBe(0o700);
    expect(mode(join(dir, "state.json"))).toBe(0o600);
    expect(loadState(dir)).toEqual(saved);
  });

  test("is absent before up and rejects an unknown version", () => {
    const dir = join(scratch, "t");
    expect(loadState(dir)).toBeUndefined();
    mkdirSync(dir);
    writeFileSync(join(dir, "state.json"), '{"version":3}');
    expect(() => loadState(dir)).toThrow(/unknown version/);
  });

  test("a state of the previous version points to down, which keeps the data, not to purge", () => {
    const dir = join(scratch, "t");
    mkdirSync(dir);
    writeFileSync(join(dir, "state.json"), '{"version":1}');
    expect(() => loadState(dir)).toThrow(/older version.*run the down command \(data volumes are kept\)/);
    expect(() => loadState(dir)).not.toThrow(/purge/);
  });
});

describe("lock", () => {
  test("a second holder is refused until the first releases", () => {
    const release = acquireLock(scratch, "t");
    expect(() => acquireLock(scratch, "t")).toThrow(/another command/);
    release();
    acquireLock(scratch, "t")();
  });

  test("instances do not block each other", () => {
    const a = acquireLock(scratch, "a");
    const b = acquireLock(scratch, "b");
    a();
    b();
  });

  test("a lock left by a dead process is taken over", () => {
    const lock = join(scratch, "t.lock");
    mkdirSync(lock);
    // PIDs above the kernel's maximum cannot exist.
    writeFileSync(join(lock, "pid"), "99999999\n");
    acquireLock(scratch, "t")();
  });

  test("a lock directory without an owner is not taken while it is fresh, but is when old", () => {
    const lock = join(scratch, "t.lock");
    mkdirSync(lock);
    // A non-empty directory, so the atomic rename of a real lock cannot replace it.
    writeFileSync(join(lock, "unexpected"), "");
    expect(() => acquireLock(scratch, "t")).toThrow(/try again/);
    const old = new Date(Date.now() - 60_000);
    utimesSync(lock, old, old);
    acquireLock(scratch, "t")();
  });

  test("taking and releasing leaves nothing behind", () => {
    acquireLock(scratch, "t")();
    expect(readdirSync(scratch)).toEqual([]);
    const release = acquireLock(scratch, "t");
    expect(() => acquireLock(scratch, "t")).toThrow(/another command/);
    expect(readdirSync(scratch)).toEqual(["t.lock"]);
    release();
  });
});

describe("removeStateExcept", () => {
  test("removes everything but the named entries, which is what down leaves", () => {
    mkdirSync(join(scratch, "identity"));
    writeFileSync(join(scratch, "identity", "signing-key.pem"), "k");
    mkdirSync(join(scratch, "tls"));
    writeFileSync(join(scratch, "tls", "ca.crt"), "c");
    writeFileSync(join(scratch, "state.json"), "{}");
    removeStateExcept(scratch, ["identity"]);
    expect(readdirSync(scratch)).toEqual(["identity"]);
    expect(readdirSync(join(scratch, "identity"))).toEqual(["signing-key.pem"]);
  });

  test("does nothing for a directory that does not exist", () => {
    expect(() => removeStateExcept(join(scratch, "absent"), ["identity"])).not.toThrow();
  });
});
