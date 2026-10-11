// Where one dev environment keeps its secrets and its resolved settings. Everything lives under
// the user's XDG state directory, never in the repository, with owner-only permissions.
import { randomBytes } from "node:crypto";
import {
  chmodSync,
  closeSync,
  existsSync,
  mkdirSync,
  openSync,
  readFileSync,
  renameSync,
  rmSync,
  statSync,
  writeSync,
} from "node:fs";
import { homedir } from "node:os";
import { join } from "node:path";

import { registerCleanup } from "./cleanup.ts";
import type { Credentials } from "./secrets.ts";
import type { Ports as InfraPorts } from "./stack.ts";

export const INSTANCE_PATTERN = /^[a-z][a-z0-9-]{0,23}$/;

export function resolveInstance(env: Record<string, string | undefined>): string {
  const instance = env.TRUSTIOT_DEV_INSTANCE ?? "default";
  if (!INSTANCE_PATTERN.test(instance)) {
    throw new Error(
      "TRUSTIOT_DEV_INSTANCE must start with a lowercase letter and contain only lowercase " +
        "letters, digits and '-' (at most 24 characters)",
    );
  }
  return instance;
}

/** `$XDG_STATE_HOME/trustiot-v2/dev` (default `~/.local/state/trustiot-v2/dev`). */
export function stateRoot(env: Record<string, string | undefined>): string {
  const base = env.XDG_STATE_HOME?.startsWith("/")
    ? env.XDG_STATE_HOME
    : join(env.HOME || homedir(), ".local", "state");
  return join(base, "trustiot-v2", "dev");
}

export function stateDir(env: Record<string, string | undefined>, instance: string): string {
  return join(stateRoot(env), instance);
}

/** The CA certificate of the instance (public) that host-side tools and services trust. */
export function caCertificatePath(dir: string): string {
  return join(dir, "tls", "ca.crt");
}

export interface Ports extends InfraPorts {
  /** Host ports of the services, by service name. */
  services: Record<string, number>;
}

/** What `up` resolved; `seed`, `status` and `env` read it back. Contains secrets: mode 0600. */
export interface State {
  version: 2;
  instance: string;
  startedAt: string;
  ports: Ports;
  credentials: Credentials;
}

/** Creates a directory (and parents) that only the owner can enter. */
export function ensurePrivateDir(path: string): void {
  mkdirSync(path, { recursive: true, mode: 0o700 });
  chmodSync(path, 0o700);
}

/**
 * Writes a file that only the owner can read. The file is created with mode 0600 under a
 * temporary name and renamed into place, so the content is never visible with wider
 * permissions and readers never see a half-written file.
 */
export function writePrivateFile(path: string, content: string): void {
  const temporary = `${path}.${randomBytes(6).toString("hex")}.tmp`;
  const fd = openSync(temporary, "wx", 0o600);
  try {
    writeSync(fd, content);
  } catch (error) {
    closeSync(fd);
    rmSync(temporary, { force: true });
    throw error;
  }
  closeSync(fd);
  chmodSync(temporary, 0o600);
  renameSync(temporary, path);
}

export function statePath(dir: string): string {
  return join(dir, "state.json");
}

export function saveState(dir: string, state: State): void {
  ensurePrivateDir(dir);
  writePrivateFile(statePath(dir), `${JSON.stringify(state, null, 2)}\n`);
}

export function loadState(dir: string): State | undefined {
  const path = statePath(dir);
  if (!existsSync(path)) return undefined;
  const state = JSON.parse(readFileSync(path, "utf8")) as { version?: number };
  if (state.version === 1) {
    // Written before the Postgres TLS change (Laysi/trustiot-v2#273): it has no service logins
    // and the running Postgres has no TLS. `down` does not read the state, removes those
    // containers and keeps the data volumes; the next `up` makes a current state on the same
    // data, so nothing is lost.
    throw new Error(
      `${path} was written by an older version of this script, and its containers have no ` +
        "Postgres TLS; run the down command (data volumes are kept) and then up",
    );
  }
  if (state.version !== 2) {
    throw new Error(`${path} has an unknown version; run the purge command and start again`);
  }
  return state as State;
}

export function removeState(dir: string): void {
  rmSync(dir, { recursive: true, force: true });
}

/** Runs `action` with a private file holding `content`, and removes the file afterwards. */
export async function withPrivateFile<T>(
  dir: string,
  name: string,
  content: string,
  action: (path: string) => Promise<T>,
): Promise<T> {
  ensurePrivateDir(dir);
  const path = join(dir, `${name}.${randomBytes(6).toString("hex")}`);
  writePrivateFile(path, content);
  const cleanup = () => rmSync(path, { force: true });
  const forget = registerCleanup(cleanup);
  try {
    return await action(path);
  } finally {
    cleanup();
    forget();
  }
}

/**
 * A per-instance lock so two `up`, `down` or `purge` commands cannot interleave. The lock is a
 * directory that appears atomically with its owner's PID inside (a private temporary directory
 * is renamed into place; a rename onto an existing directory fails), so no other command can see
 * a lock without an owner. A lock whose owner no longer exists is taken over.
 */
export function acquireLock(root: string, instance: string): () => void {
  ensurePrivateDir(root);
  const lock = join(root, `${instance}.lock`);
  const owner = join(lock, "pid");
  for (let attempt = 0; attempt < 5; attempt++) {
    const pending = `${lock}.${randomBytes(6).toString("hex")}.pending`;
    mkdirSync(pending, { mode: 0o700 });
    writePrivateFile(join(pending, "pid"), `${process.pid}\n`);
    try {
      renameSync(pending, lock);
      const cleanup = () => rmSync(lock, { recursive: true, force: true });
      const forget = registerCleanup(cleanup);
      return () => {
        cleanup();
        forget();
      };
    } catch (error) {
      rmSync(pending, { recursive: true, force: true });
      const code = (error as NodeJS.ErrnoException).code;
      if (code !== "EEXIST" && code !== "ENOTEMPTY") throw error;
    }
    const pid = Number.parseInt(existsSync(owner) ? readFileSync(owner, "utf8") : "", 10);
    if (!Number.isInteger(pid) || pid <= 0) {
      // Either the lock was just released (try again) or it is damaged: a lock is created
      // together with its PID, so one without a PID that is also old was not made by us.
      try {
        if (Date.now() - statSync(lock).mtimeMs > 10_000) rmSync(lock, { recursive: true, force: true });
      } catch {
        // gone already
      }
      continue;
    }
    if (isAlive(pid)) {
      throw new Error(`another command is working on instance "${instance}" (pid ${pid})`);
    }
    reclaim(lock, pid);
  }
  throw new Error(`cannot take the lock for instance "${instance}"; try again`);
}

/**
 * Removes a lock whose owner (`stalePid`) is gone. The directory is renamed away first, so only
 * one command can win; if what was renamed turns out to be another command's fresh lock, it is
 * put back.
 */
function reclaim(lock: string, stalePid: number): void {
  const moved = `${lock}.${randomBytes(6).toString("hex")}.stale`;
  try {
    renameSync(lock, moved);
  } catch (error) {
    if ((error as NodeJS.ErrnoException).code === "ENOENT") return;
    throw error;
  }
  const pid = Number.parseInt(readFileSync(join(moved, "pid"), "utf8"), 10);
  if (pid === stalePid) {
    rmSync(moved, { recursive: true, force: true });
    return;
  }
  try {
    renameSync(moved, lock);
  } catch {
    rmSync(moved, { recursive: true, force: true });
    throw new Error("lock contention while reclaiming a stale lock; try again");
  }
}

function isAlive(pid: number): boolean {
  try {
    process.kill(pid, 0);
    return true;
  } catch (error) {
    return (error as NodeJS.ErrnoException).code === "EPERM";
  }
}
