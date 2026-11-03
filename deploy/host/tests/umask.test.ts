// The caller's umask: sudo passes a stricter one (027, 077) on to the installer, and dnf would
// then leave its system state and caches readable by root only (Laysi/trustiot-v2#476). The
// installer runs every system tool under 022, and its own files and directories get their
// modes explicitly, whatever the umask.
import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { randomBytes } from "node:crypto";
import { mkdtempSync, rmSync, statSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { CREDSTORE, configPath, credentialPath } from "../src/desired.ts";
import { main } from "../src/main.ts";
import { HostSystem } from "../src/system.ts";
import { FAKE_INSTALLER_SHA256, FakeHost } from "./fake-host.ts";

// Made up at run time, like the token in install.test.ts.
function makeToken(): string {
  const secret = randomBytes(32).toString("base64");
  return Buffer.from(JSON.stringify({ a: "4f1d0e9c8b7a6f5e4d3c2b1a09f8e7d6", t: "0b6c1c3e-4a59-4f33-9d55-5b8a2d7f9e10", s: secret })).toString(
    "base64",
  );
}

let savedUmask = 0;
let scratch = "";

beforeEach(() => {
  savedUmask = process.umask();
  scratch = mkdtempSync(join(tmpdir(), "trustiot-host-umask-"));
});

afterEach(() => {
  process.umask(savedUmask);
  rmSync(scratch, { recursive: true, force: true });
});

describe("caller's umask", () => {
  test("started under umask 077, every system tool runs under 022 and the secrets keep 0600 and 0700", async () => {
    const tokenFile = join(scratch, "tunnel-token");
    writeFileSync(tokenFile, `${makeToken()}\n`, { mode: 0o600 });
    const host = new FakeHost();

    process.umask(0o077);
    const code = await main(["--token-file", tokenFile], host, host.log, { k3sInstallerSha256: FAKE_INSTALLER_SHA256 });

    expect(code).toBe(0);
    // dnf, rpm, systemctl, firewall-cmd, the k3s installer and the rest: all of them.
    expect(host.commands.map((cmd) => cmd[0])).toEqual(expect.arrayContaining(["dnf", "rpm", "systemctl", "firewall-cmd", "sh"]));
    const seen = new Set(host.commandUmasks.map((mask) => `0${mask.toString(8).padStart(3, "0")}`));
    expect([...seen]).toEqual(["0022"]);
    expect(host.commandUmasks).toHaveLength(host.commands.length);
    expect(host.files.get(credentialPath("cloudflared"))).toMatchObject({ mode: 0o600, uid: 0 });
    expect(host.files.get(configPath("cloudflared"))).toMatchObject({ mode: 0o600, uid: 0 });
    expect(host.directories.get(CREDSTORE)).toMatchObject({ mode: 0o700, uid: 0 });
  });
});

describe("HostSystem directories, whatever the umask", () => {
  // Real directories under a temporary path: makeDirectory needs no root for that. writeFile
  // also changes the owner to root, so it is covered only by its explicit chmod.
  test.each([
    ["0077", 0o077],
    ["0022", 0o022],
    ["0000", 0o000],
  ])("umask %s: makeDirectory gives the directory its mode and the parents it creates 0755", async (_name, umask) => {
    process.umask(umask);
    const system = new HostSystem();
    const top = join(scratch, "a");
    await system.makeDirectory(join(top, "b", "c"), 0o755);
    for (const path of [top, join(top, "b"), join(top, "b", "c")]) expect(statSync(path).mode & 0o7777).toBe(0o755);
    await system.makeDirectory(join(scratch, "x", "secret"), 0o700);
    expect(statSync(join(scratch, "x")).mode & 0o7777).toBe(0o755);
    expect(statSync(join(scratch, "x", "secret")).mode & 0o7777).toBe(0o700);
  });

  test("an existing parent directory keeps its own mode", async () => {
    process.umask(0o022);
    const parent = join(scratch, "existing");
    await new HostSystem().makeDirectory(parent, 0o750);
    await new HostSystem().makeDirectory(join(parent, "child"), 0o700);
    expect(statSync(parent).mode & 0o7777).toBe(0o750);
    expect(statSync(join(parent, "child")).mode & 0o7777).toBe(0o700);
  });
});
