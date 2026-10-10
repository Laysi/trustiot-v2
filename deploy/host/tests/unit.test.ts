// The cloudflared unit, the token parser, the command line and the pinned signing key.
import { describe, expect, test } from "bun:test";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { USAGE, parseArgs } from "../src/cli.ts";
import {
  CLOUDFLARE_KEY,
  CLOUDFLARE_KEY_FINGERPRINT,
  MAX_EXPOSURE,
  checkUnitName,
  credentialPath,
  renderUnit,
} from "../src/desired.ts";
import { TokenError, parseTunnelToken } from "../src/token.ts";

// GitHub sets CI=true: there a missing tool fails the test instead of skipping it.
const IN_CI = process.env.CI === "true";

function available(tool: string): boolean {
  const found = Bun.which(tool) !== null;
  if (!found && IN_CI) throw new Error(`${tool} is required in CI`);
  return found;
}

describe("cloudflared unit", () => {
  const unit = renderUnit("cloudflared");
  const directives = unit.split("\n").filter((line) => /^[A-Za-z]+=/.test(line));

  test("runs sandboxed: dynamic user, read-only OS, no new privileges, no capabilities", () => {
    for (const directive of [
      "DynamicUser=yes",
      "ProtectSystem=strict",
      "NoNewPrivileges=yes",
      "CapabilityBoundingSet=",
      "AmbientCapabilities=",
      "ProtectHome=yes",
      "PrivateDevices=yes",
    ]) {
      expect(directives).toContain(directive);
    }
    expect(directives.some((line) => /^(User|Group|SupplementaryGroups)=/.test(line))).toBe(false);
  });

  test("gets the token through LoadCredential, never on its command line", () => {
    expect(directives).toContain(`LoadCredential=tunnel-token:${credentialPath("cloudflared")}`);
    const execStart = directives.find((line) => line.startsWith("ExecStart="));
    expect(execStart).toBe("ExecStart=/usr/bin/cloudflared --no-autoupdate tunnel run --token-file %d/tunnel-token");
    expect(unit).not.toMatch(/--token(?!-file)|TUNNEL_TOKEN=/);
  });

  test("outlasts cloudflared's 30-second grace period when stopping", () => {
    expect(directives).toContain("TimeoutStopSec=60s");
  });

  test("puts every directive in the section systemd reads it from", () => {
    const sections = new Map<string, string[]>();
    let section = "";
    for (const line of unit.split("\n")) {
      const header = /^\[(\w+)\]$/.exec(line);
      if (header?.[1]) section = header[1];
      else if (/^[A-Za-z]+=/.test(line)) sections.set(section, [...(sections.get(section) ?? []), line.split("=")[0] ?? ""]);
    }
    // StartLimitIntervalSec= is a [Unit] setting; under [Service] systemd ignores it.
    expect(sections.get("Unit")).toEqual(["Description", "Documentation", "After", "Wants", "StartLimitIntervalSec"]);
    expect(sections.get("Install")).toEqual(["WantedBy"]);
    expect(sections.get("Service")).not.toContain("StartLimitIntervalSec");
  });

  test("passes systemd-analyze verify", () => {
    if (!available("systemd-analyze")) return;
    const dir = mkdtempSync(join(tmpdir(), "trustiot-host-verify-"));
    try {
      const file = join(dir, "cloudflared.service");
      writeFileSync(file, unit);
      const result = Bun.spawnSync({ cmd: ["systemd-analyze", "verify", "--man=no", file], stdout: "pipe", stderr: "pipe" });
      // The binary may be missing on the test machine; any other complaint is a real error.
      const complaints = result.stderr
        .toString()
        .split("\n")
        .filter((line) => line.trim() !== "" && !line.includes("/usr/bin/cloudflared"));
      expect(complaints).toEqual([]);
    } finally {
      rmSync(dir, { recursive: true, force: true });
    }
  });

  test(`scores at most ${MAX_EXPOSURE} in systemd-analyze security`, () => {
    if (!available("systemd-analyze")) return;
    const dir = mkdtempSync(join(tmpdir(), "trustiot-host-unit-"));
    try {
      const file = join(dir, "cloudflared.service");
      writeFileSync(file, unit);
      const result = Bun.spawnSync({
        cmd: ["systemd-analyze", "security", "--offline=true", "--no-pager", file],
        stdout: "pipe",
        stderr: "pipe",
      });
      const output = result.stdout.toString();
      const match = /Overall exposure level for \S+: (\d+(?:\.\d+)?)/.exec(output);
      expect(match, `${output}\n${result.stderr.toString()}`).not.toBeNull();
      expect(Number(match?.[1])).toBeLessThanOrEqual(MAX_EXPOSURE);
    } finally {
      rmSync(dir, { recursive: true, force: true });
    }
  });

  test("unit names are restricted to a safe alphabet", () => {
    expect(() => checkUnitName("cloudflared-e2e-64")).not.toThrow();
    for (const bad of ["", "Cloudflared", "../etc/passwd", "a b", "cloudflared.service", "-x"]) {
      expect(() => checkUnitName(bad)).toThrow(/invalid unit name/);
    }
  });
});

describe("tunnel token", () => {
  const id = "0b6c1c3e-4a59-4f33-9d55-5b8a2d7f9e10";
  const secret = "c2VjcmV0LXZhbHVlLXRoYXQtaXMtbG9uZy1lbm91Z2g=";
  const encode = (value: unknown) => new TextEncoder().encode(Buffer.from(JSON.stringify(value)).toString("base64"));

  test("accepts a token and reports only the tunnel ID", () => {
    const token = parseTunnelToken(encode({ a: "acct", t: id, s: secret }));
    expect(token.tunnelId).toBe(id);
    expect(token.value).not.toContain("\n");
  });

  test("trims the newline a file or a pipe adds", () => {
    const raw = Buffer.from(JSON.stringify({ a: "acct", t: id, s: secret })).toString("base64");
    expect(parseTunnelToken(new TextEncoder().encode(`  ${raw}\n`)).value).toBe(raw);
  });

  test.each([
    ["empty", new TextEncoder().encode("\n")],
    ["an install command", new TextEncoder().encode(`cloudflared service install ${secret}`)],
    ["not JSON", new TextEncoder().encode(Buffer.from("not json").toString("base64"))],
    ["without the secret", encode({ a: "acct", t: id })],
    ["with a bad tunnel ID", encode({ a: "acct", t: "not-a-uuid", s: secret })],
    ["too long", new TextEncoder().encode("A".repeat(5000))],
  ])("rejects a token that is %s, without quoting it", (_name, input) => {
    let error: unknown;
    try {
      parseTunnelToken(input);
    } catch (caught) {
      error = caught;
    }
    expect(error).toBeInstanceOf(TokenError);
    expect((error as Error).message).not.toContain(secret);
    expect((error as Error).message).not.toContain("AAAA");
  });
});

describe("command line", () => {
  test("defaults", () => {
    expect(parseArgs([])).toEqual({
      help: false,
      tokenSource: { kind: "none" },
      options: { unit: "cloudflared", skipK3s: false, reconfigureK3s: false, reinstallCloudflared: false, dryRun: false },
    });
  });

  test("all options", () => {
    const parsed = parseArgs(["--token-file=/root/token", "--unit", "cloudflared-e2e-64.service", "--skip-k3s", "--dry-run"]);
    expect(parsed.tokenSource).toEqual({ kind: "file", path: "/root/token" });
    expect(parsed.options).toEqual({ unit: "cloudflared-e2e-64", skipK3s: true, reconfigureK3s: false, reinstallCloudflared: false, dryRun: true });
    expect(parseArgs(["--token-stdin", "--reconfigure-k3s"]).tokenSource).toEqual({ kind: "stdin" });
  });

  test("refuses a token on the command line", () => {
    expect(() => parseArgs(["--token", "eyJh"])).toThrow(/--token-stdin or --token-file/);
    expect(() => parseArgs(["--token=eyJh"])).toThrow(/--token-stdin or --token-file/);
  });

  test("refuses unknown arguments, missing values and two token sources", () => {
    expect(() => parseArgs(["eyJhIjoi"])).toThrow(/unknown argument/);
    expect(() => parseArgs(["--unit"])).toThrow(/needs a value/);
    expect(() => parseArgs(["--token-stdin", "--token-file", "/x"])).toThrow(/only once/);
  });

  test("the usage text lists every option", () => {
    for (const flag of ["--token-stdin", "--token-file", "--unit", "--skip-k3s", "--reconfigure-k3s", "--reinstall-cloudflared", "--dry-run"]) {
      expect(USAGE).toContain(flag);
    }
  });
});

describe("Cloudflare package signing key", () => {
  test("is the pinned key", () => {
    if (!available("gpg")) return;
    const home = mkdtempSync(join(tmpdir(), "trustiot-host-gpg-"));
    try {
      const result = Bun.spawnSync({
        cmd: ["gpg", "--homedir", home, "--batch", "--with-colons", "--show-keys"],
        stdin: new TextEncoder().encode(CLOUDFLARE_KEY),
        stdout: "pipe",
        stderr: "pipe",
      });
      const fingerprints = result.stdout
        .toString()
        .split("\n")
        .filter((line) => line.startsWith("fpr:"))
        .map((line) => line.split(":")[9]);
      expect(fingerprints[0]).toBe(CLOUDFLARE_KEY_FINGERPRINT);
    } finally {
      rmSync(home, { recursive: true, force: true });
    }
  });
});
