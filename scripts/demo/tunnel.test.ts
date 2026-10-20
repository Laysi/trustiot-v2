// The rules of scripts/demo/tunnel.ts, without the demo machine, ssh or Cloudflare (Laysi/trustiot-v2#466).
import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { existsSync, lstatSync, mkdirSync, mkdtempSync, readdirSync, readFileSync, readlinkSync, rmSync, statSync, symlinkSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import type { RunResult } from "../release/exec.ts";
import { Box, LOCK_DIR, Log, OPS_TUNNEL_UNIT, ROOT, takeLock } from "./lib.ts";
import {
  APP_ONLY_PATHS,
  BYPASS_PATHS,
  CONFIG,
  CREDENTIAL,
  LOCK_STATE_COMMAND,
  DEMO_METRICS_PORT,
  DEMO_TUNNEL_UNIT,
  INSTALLED_VERSION_COMMAND,
  ISSUE,
  LOCAL_LINK,
  PACKAGE_MARKER,
  POD_SSH_RULE,
  RUN_DIR,
  RUN_UNIT_PREFIX,
  STAGE1_STATUS,
  STATE_DIR,
  START_REFUSED,
  SWAP_CHANGE,
  StillRunningError,
  TOKEN_PREFIX,
  TRUSTED_ZONE_VIEWS,
  ZONE_BACKUP,
  allowedChanges,
  applyInstall,
  auditScript,
  backupDecision,
  checkChanges,
  checkedCiphertext,
  checkedTunnelId,
  classify,
  cloudflaredPlan,
  codeState,
  curlArgs,
  describeCloudflared,
  expandHome,
  checkLockForDryRun,
  dryRunLockCheck,
  fileTypeCommand,
  filesWith,
  firewallCmd,
  firewalldRecordScript,
  installerArgs,
  installerChanges,
  installerWarnings,
  linkFacts,
  linkReport,
  listedChecksum,
  makePlan,
  mask,
  metricsPortHolder,
  parseCurlOutput,
  parseLockState,
  parsePost,
  parseZoneInfo,
  probePasses,
  probes,
  readCiphertextFile,
  readCloudflaredFacts,
  quietSh,
  readyConnections,
  redactSecrets,
  removePlan,
  removeScript,
  restoreProblems,
  requireNoUpdateTimer,
  restoreTrustedZoneScript,
  runId,
  runProbes,
  runnerScript,
  runningInstallUnits,
  spkiFingerprint,
  stage,
  splitAllowChanges,
  startRun,
  startCommand,
  summary,
  tunnelIdFromInstallerOutput,
  unitStates,
  waitForRun,
  zoneBackupScript,
  zoneDiff,
  zoneItems,
  zoneRestore,
  type ApplySteps,
  type CloudflaredFacts,
  type InstallPlan,
  type RunOutcome,
  type ZoneBackup,
} from "./tunnel.ts";

const HOSTS = { app: "app.example.com", gateway: "gw.example.com", ota: "ota.example.com" };
const TUNNEL = "5d0c3f2e-8a41-4b7e-9c26-1f3a7e90b4d8";
const ok = (stdout = ""): RunResult => ({ stdout, stderr: "", exitCode: 0 });
const sha = (data: string | Uint8Array) => new Bun.CryptoHasher("sha256").update(data).digest("hex");

/** firewall-cmd's message for --zone next to a configuration-wide option (firewall-cmd 2.5.2). */
const FIREWALL_WRONG_USAGE = "Wrong usage of --get-zones | --get-services | --get-icmptypes | --get-policies.";

/**
 * Writes a stand-in firewall-cmd into `bin` that refuses what the real one refuses, checked against
 * firewall-cmd 2.5.2 on 2026-10-20 (its `options_config` check, and a read-only
 * `firewall-cmd --zone=trusted --info-zone=trusted`, which exits 2): --zone next to an --info-*,
 * --get-zones-style or --list-all-* option exits 2 with firewall-cmd's message, and an argument
 * outside the ones these scripts may use exits 2 like firewall-cmd's argument parser. An action on
 * a zone without --zone acts on the default zone in the real one; the stand-in exits 99 for it.
 * Then `behaviour` (a shell `case "$*" in ... esac`) answers. Every call is appended to $FAKE_CALLS
 * when it is set.
 */
function strictFirewallCmd(bin: string, behaviour: string): void {
  writeFileSync(
    join(bin, "firewall-cmd"),
    `#!/bin/sh
[ -n "\${FAKE_CALLS:-}" ] && echo "$*" >> "$FAKE_CALLS"
zone= config= action=
for a in "$@"; do
  case "$a" in
    --zone=*) zone=1 ;;
    --info-*=* | --get-zones | --get-services | --get-icmptypes | --get-policies | --get-ipsets | --get-helpers | --get-zone-of-*=* | --list-all-zones | --list-all-policies) config=1 ;;
    --query-rich-rule=* | --query-source=* | --add-rich-rule=* | --remove-rich-rule=* | --add-source=* | --remove-source=*) action=1 ;;
    -q | --permanent) ;;
    *) echo "firewall-cmd: error: unrecognized arguments: $a" >&2; exit 2 ;;
  esac
done
if [ -n "$zone" ] && [ -n "$config" ]; then
  echo "usage: 'firewall-cmd --help' for usage information or see firewall-cmd(1) man page" >&2
  echo "${FIREWALL_WRONG_USAGE}" >&2
  exit 2
fi
if [ -n "$action" ] && [ -z "$zone" ]; then echo "stand-in: an action without --zone acts on the default zone" >&2; exit 99; fi
${behaviour}
`,
    { mode: 0o755 },
  );
}

/** The installer's dry run on the demo machine as of 2026-10-20: the 2026-10-11 lab run with the demo unit and its config. */
function dryRunOutput(tunnel: string = TUNNEL, extra: string[] = []): string {
  return [
    "  host: Fedora Linux Asahi Remix 44 (Server Edition), arm64, systemd 259",
    `+ would firewalld (runtime): in the trusted zone, ${POD_SSH_RULE}`,
    "+ would firewalld (runtime): remove the service network 10.43.0.0/16 from the trusted zone (it is never a source)",
    `+ would firewalld (permanent): in the trusted zone, ${POD_SSH_RULE}`,
    "+ would firewalld (permanent): remove the service network 10.43.0.0/16 from the trusted zone (it is never a source)",
    "  k3s: skipped (--skip-k3s), left as it is",
    "+ would write /etc/pki/rpm-gpg/RPM-GPG-KEY-cloudflare-packaging: Cloudflare's package signing key",
    "+ would write /etc/yum.repos.d/cloudflared.repo: cloudflared repository (https://pkg.cloudflare.com/cloudflared/rpm)",
    `+ would ${SWAP_CHANGE}`,
    `  tunnel token: tunnel ${tunnel}`,
    "+ would write /etc/credstore/cloudflared-demo.tunnel-token: tunnel token (root only)",
    "+ would write /etc/credstore/cloudflared-demo.cloudflared.yml: cloudflared config (root only)",
    "+ would write /etc/systemd/system/cloudflared-demo.service: cloudflared unit (sandboxed, token through LoadCredential)",
    "+ would reload systemd's unit files",
    "+ would enable cloudflared-demo.service at boot",
    "+ would start cloudflared-demo.service and wait for its first tunnel connection",
    ...extra.map((line) => `+ would ${line}`),
    "  check: nothing listens on TCP or UDP 80 or 443",
    "Done: 13 change(s) planned.",
    "",
  ].join("\n");
}

let dir: string;
let log: Log;
beforeEach(() => {
  dir = mkdtempSync(join(tmpdir(), "demo-tunnel-"));
  log = new Log("test", join(dir, "log"));
});
afterEach(() => rmSync(dir, { recursive: true, force: true }));

const fakeBox = (handler: (command: string, stdin?: string | Uint8Array) => RunResult) => new Box("demo-box", log, { run: async (cmd, stdin) => handler(cmd.at(-1) ?? "", stdin), sleep: async () => {} });

describe("tunnel.ts: inputs", () => {
  const ciphertext = Buffer.alloc(512, 7).toString("base64");

  test("takes only a 512-byte RSA-4096 ciphertext, and never quotes it in an error", () => {
    expect(checkedCiphertext(`${ciphertext.slice(0, 100)}\n${ciphertext.slice(100)}\n`)).toBe(ciphertext);
    const short = Buffer.alloc(300, 7).toString("base64");
    for (const bad of [short, `${ciphertext}!`, `not base64 ${ciphertext}`]) {
      expect(() => checkedCiphertext(bad)).toThrow(/--ciphertext/);
      try {
        checkedCiphertext(bad);
      } catch (error) {
        expect(String(error)).not.toContain(ciphertext.slice(0, 40));
      }
    }
  });

  test("--tunnel-id is a lower-case UUID", () => {
    expect(checkedTunnelId(TUNNEL)).toBe(TUNNEL);
    for (const bad of ["", TUNNEL.toUpperCase(), `${TUNNEL} `, "trustiot-demo"]) expect(() => checkedTunnelId(bad)).toThrow(/--tunnel-id/);
  });

  test("--allow-change may be repeated and keeps each exact line", () => {
    const line = "restore owner root:root and mode 0600 on /etc/credstore/cloudflared-demo.tunnel-token";
    expect(splitAllowChanges(["--ssh-host", "h", "--allow-change", line, "--allow-change", "x y", "--apply"])).toEqual({ rest: ["--ssh-host", "h", "--apply"], allowed: [line, "x y"] });
    expect(() => splitAllowChanges(["--allow-change"])).toThrow(/needs the exact change line/);
    expect(() => splitAllowChanges(["--allow-change", "--apply"])).toThrow(/needs the exact change line/);
  });

  test("the installer's own command-line parser accepts the arguments install passes", async () => {
    // deploy/host/src/cli.ts, the parser the installer binary runs; a path variable keeps the
    // scripts' type check out of deploy/host's sources.
    const cli = join(ROOT, "deploy/host/src/cli.ts");
    const { parseArgs } = (await import(cli)) as { parseArgs: (argv: string[]) => { tokenSource: { kind: string }; options: Record<string, unknown> } };
    for (const dryRun of [false, true]) {
      const parsed = parseArgs(installerArgs(dryRun));
      expect(parsed.tokenSource).toEqual({ kind: "stdin" });
      expect(parsed.options).toEqual({ unit: DEMO_TUNNEL_UNIT, metricsPort: DEMO_METRICS_PORT, skipK3s: true, reconfigureK3s: false, reinstallCloudflared: true, dryRun });
    }
  });

  test("the demo connector is its own unit with its own metrics port, never the ops tunnel's", () => {
    expect(DEMO_TUNNEL_UNIT).not.toBe(OPS_TUNNEL_UNIT.replace(/\.service$/, ""));
    expect(installerArgs(false)).toEqual(["--skip-k3s", "--unit", "cloudflared-demo", "--metrics-port", "20250", "--token-stdin", "--reinstall-cloudflared"]);
    expect(installerArgs(true).at(-1)).toBe("--dry-run");
    // cloudflared's own choice is 20241 to 20245, where the ops tunnel listens.
    expect(DEMO_METRICS_PORT < 20241 || DEMO_METRICS_PORT > 20245).toBe(true);
    expect(CREDENTIAL).toBe("/etc/credstore/cloudflared-demo.tunnel-token");
    expect(CONFIG).toBe("/etc/credstore/cloudflared-demo.cloudflared.yml");
  });

  test("the hand-off key and the ciphertext live only in tmpfs, never on the btrfs /etc", () => {
    expect(RUN_DIR).toBe("/run/trustiot-demo");
    expect(STATE_DIR.startsWith("/run/")).toBe(false);
    const script = runnerScript();
    expect(script).toContain('-inkey "$rundir/handoff.pem"');
    expect(script).toContain('install -m 0600 "$rundir/token.enc.b64" "$ct"');
    expect(script).toContain('ct="$rundir/$run.ciphertext"');
    expect(script).not.toContain("shred");
    expect(readFileSync(join(import.meta.dir, "tunnel.ts"), "utf8")).not.toMatch(/\/etc\/trustiot-demo|shred -u/);
  });

  test("finds the installer's checksum in SHA256SUMS", () => {
    const sums = `${"a".repeat(64)}  trustiot-host-install-x86_64\n${"b".repeat(64)}  trustiot-host-install-aarch64\n`;
    expect(listedChecksum(sums, "trustiot-host-install-aarch64")).toBe("b".repeat(64));
    expect(() => listedChecksum(sums, "other")).toThrow(/lists no other/);
  });
});

describe("tunnel.ts: the runner on the demo machine", () => {
  const paths = () => ({ state: join(dir, "state"), run: join(dir, "run"), root: join(dir, "root") });
  const setUp = () => {
    const p = paths();
    for (const d of [p.state, p.run, join(p.root, "usr/local/bin"), join(p.root, "usr/local/etc/cloudflared")]) mkdirSync(d, { recursive: true, mode: 0o700 });
    symlinkSync("/usr/bin/cloudflared", join(p.root, LOCAL_LINK));
    writeFileSync(join(p.root, PACKAGE_MARKER), "");
    writeFileSync(join(p.state, "run-installer"), runnerScript(p), { mode: 0o700 });
    return p;
  };
  const runner = (p: ReturnType<typeof paths>, args: string[], env: Record<string, string> = {}) =>
    Bun.spawnSync(["bash", join(p.state, "run-installer"), ...args], { env: { PATH: process.env.PATH ?? "/usr/bin:/bin", ...env }, stdin: "ignore", stdout: "pipe", stderr: "pipe" });

  test("decrypts on the demo machine straight into the installer's stdin, from its own checked copies", () => {
    const script = runnerScript();
    expect(script).toContain("set -o pipefail");
    expect(script).toContain("rsa_padding_mode:oaep -pkeyopt rsa_oaep_md:sha256 -pkeyopt rsa_mgf1_md:sha256");
    expect(script).toContain(`state=${STATE_DIR}`);
    expect(script).toContain(`rundir=${RUN_DIR}`);
    expect(script).toContain(`install -m 0600 "$rundir/token.enc.b64" "$ct" && install -m 0700 "$state/trustiot-host-install" "$installer"`);
    expect(script).toContain(`-inkey "$rundir/handoff.pem"`);
    expect(script).toContain(`| "$installer" "$@"`);
    expect(script).toContain('> "$state/$run.log" 2>&1');
    expect(script).toContain("umask 077");
    expect(() => runnerScript({ state: "relative/dir", run: RUN_DIR, root: "" })).toThrow(/invalid directory/);
    expect(() => runnerScript({ state: STATE_DIR, run: "/tmp/x; rm -rf /", root: "" })).toThrow(/invalid directory/);
    writeFileSync(join(dir, "r"), script);
    const syntax = Bun.spawnSync(["bash", "-n", join(dir, "r")]);
    expect(syntax.stderr.toString()).toBe("");
    expect(syntax.exitCode).toBe(0);
  });

  test("a token encrypted with WebCrypto RSA-OAEP reaches the installer's stdin, and only the confirmed one; the key's fingerprint is over its SPKI DER", async () => {
    // The coordinator encrypts with WebCrypto in Cloudflare's API sandbox; the demo machine
    // decrypts with openssl. This runs the real runner against a stand-in installer that prints
    // its arguments and the SHA-256 of its stdin.
    const p = setUp();
    const keygen = Bun.spawnSync(["openssl", "genpkey", "-algorithm", "RSA", "-pkeyopt", "rsa_keygen_bits:4096", "-out", join(p.run, "handoff.pem")]);
    expect(keygen.exitCode).toBe(0);
    const pub = Bun.spawnSync(["openssl", "pkey", "-in", join(p.run, "handoff.pem"), "-pubout"]).stdout.toString();
    const der = Buffer.from(pub.replace(/-----[^-]+-----|\s/g, ""), "base64");
    const key = await crypto.subtle.importKey("spki", der, { name: "RSA-OAEP", hash: "SHA-256" }, true, ["encrypt"]);
    // The sandbox computes the fingerprint of the key it encrypts for from exportKey("spki").
    expect(spkiFingerprint(pub)).toBe(sha(new Uint8Array(await crypto.subtle.exportKey("spki", key))));
    expect(spkiFingerprint(pub)).not.toBe(sha(pub));
    // Token-shaped values built at run time: base64 of {"a":…,"t":…,"s":…}.
    const makeToken = () => Buffer.from(JSON.stringify({ a: "0".repeat(32), t: crypto.randomUUID(), s: Buffer.from(crypto.getRandomValues(new Uint8Array(32))).toString("base64") })).toString("base64");
    const seal = async (token: string) => `${checkedCiphertext(Buffer.from(await crypto.subtle.encrypt({ name: "RSA-OAEP" }, key, new TextEncoder().encode(token))).toString("base64"))}\n`;
    const token = makeToken();
    const ciphertext = await seal(token);
    const installer = '#!/bin/sh\necho "args: $*"\nsha256sum | cut -d" " -f1\n';
    writeFileSync(join(p.run, "token.enc.b64"), ciphertext);
    writeFileSync(join(p.state, "trustiot-host-install"), installer, { mode: 0o700 });
    const read = (id: string) => ({ exit: readFileSync(join(p.state, `${id}.exit`), "utf8").trim(), output: readFileSync(join(p.state, `${id}.log`), "utf8") });

    expect(runner(p, ["run", "dry-run-1", sha(installer), sha(ciphertext), ...installerArgs(true)]).exitCode).toBe(0);
    expect(read("dry-run-1")).toEqual({ exit: "0", output: `args: ${installerArgs(true).join(" ")}\n${sha(token)}\n` });
    expect(read("dry-run-1").output).not.toContain(token);
    // The run's own copies are gone.
    expect([...readdirSync(p.state), ...readdirSync(p.run)].filter((name) => /\.(ciphertext|installer)$/.test(name))).toEqual([]);
    expect(statSync(join(p.state, "dry-run-1.log")).mode & 0o077).toBe(0);

    // Another invocation staged a different token or installer after the plan was confirmed: nothing runs.
    writeFileSync(join(p.run, "token.enc.b64"), await seal(makeToken()));
    expect(runner(p, ["run", "apply-1", sha(installer), sha(ciphertext)]).exitCode).toBe(3);
    expect(read("apply-1")).toEqual({ exit: "3", output: `${p.run}/token.enc.b64 is not the one in the confirmed plan; nothing ran\n` });
    writeFileSync(join(p.run, "token.enc.b64"), ciphertext);
    writeFileSync(join(p.state, "trustiot-host-install"), `${installer}# changed\n`, { mode: 0o700 });
    expect(runner(p, ["run", "apply-2", sha(installer), sha(ciphertext)]).exitCode).toBe(3);
    expect(read("apply-2").output).toBe(`${p.state}/trustiot-host-install is not the one in the confirmed plan; nothing ran\n`);
    expect(runner(p, ["run", "apply-3", "not-a-sum", sha(ciphertext)]).exitCode).toBe(2);
    expect(runner(p, ["run", "bad;id", sha(installer), sha(ciphertext)]).exitCode).toBe(2);
    // Generating an RSA-4096 key takes seconds, and much longer on a loaded runner; this bounds
    // only the wait for a hung openssl, nothing the test measures.
  }, 180_000);

  test("after an installer that deleted the link and the marker and failed, ExecStopPost puts both back as they were", () => {
    const p = setUp();
    const installer = `#!/bin/sh\nrm -f ${join(p.root, LOCAL_LINK)} ${join(p.root, PACKAGE_MARKER)}\necho "dnf failed"\nexit 1\n`;
    writeFileSync(join(p.state, "trustiot-host-install"), installer, { mode: 0o700 });
    writeFileSync(join(p.run, "token.enc.b64"), "AAAA\n");
    writeFileSync(join(p.state, "apply-1.runner"), "copy");
    expect(runner(p, ["run", "apply-1", sha(installer), sha("AAAA\n"), "--x"]).exitCode).toBe(1);
    expect(readFileSync(join(p.state, "apply-1.exit"), "utf8").trim()).toBe("1");
    expect(existsSync(join(p.root, LOCAL_LINK)) || lstatSync(join(p.root, LOCAL_LINK), { throwIfNoEntry: false }) !== undefined).toBe(false);

    const post = runner(p, ["post", "apply-1", "1", "1"], { SERVICE_RESULT: "exit-code", EXIT_CODE: "exited", EXIT_STATUS: "1" });
    expect(post.exitCode).toBe(0);
    expect(parsePost(readFileSync(join(p.state, "apply-1.post"), "utf8"))).toEqual({ service_result: "exit-code", exit_code: "exited", exit_status: "1", link: "restored", marker: "restored" });
    expect(readlinkSync(join(p.root, LOCAL_LINK))).toBe("/usr/bin/cloudflared");
    expect(statSync(join(p.root, PACKAGE_MARKER)).isFile()).toBe(true);
    expect(statSync(join(p.root, PACKAGE_MARKER)).size).toBe(0);
    // The run's copy of the runner goes with the post step.
    expect(existsSync(join(p.state, "apply-1.runner"))).toBe(false);
  });

  test("ExecStopPost also restores after a run that was stopped or killed, and never overwrites what someone else put there", () => {
    const p = setUp();
    rmSync(join(p.root, LOCAL_LINK));
    rmSync(join(p.root, PACKAGE_MARKER));
    // Killed before it wrote .exit: the post step alone.
    expect(runner(p, ["post", "apply-2", "1", "1"], { SERVICE_RESULT: "signal", EXIT_CODE: "killed", EXIT_STATUS: "KILL" }).exitCode).toBe(0);
    expect(parsePost(readFileSync(join(p.state, "apply-2.post"), "utf8"))).toMatchObject({ service_result: "signal", link: "restored", marker: "restored" });
    expect(existsSync(join(p.state, "apply-2.exit"))).toBe(false);
    // A regular file where the link was: left alone and reported.
    rmSync(join(p.root, LOCAL_LINK));
    writeFileSync(join(p.root, LOCAL_LINK), "someone's file");
    expect(runner(p, ["post", "apply-3", "1", "0"]).exitCode).toBe(0);
    expect(parsePost(readFileSync(join(p.state, "apply-3.post"), "utf8"))).toMatchObject({ link: "conflict", marker: "not-asked" });
    expect(readFileSync(join(p.root, LOCAL_LINK), "utf8")).toBe("someone's file");
    // Nothing asked (a dry run): nothing touched.
    rmSync(join(p.root, LOCAL_LINK));
    expect(runner(p, ["post", "dry-run-1", "0", "0"]).exitCode).toBe(0);
    expect(lstatSync(join(p.root, LOCAL_LINK), { throwIfNoEntry: false })).toBeUndefined();
    expect(runner(p, ["post", "dry-run-2", "1", "2"]).exitCode).toBe(2);
  });

  test("a run starts as a transient unit with the plan's checksums and restore flags, and no token on the command line", () => {
    const id = runId("dry-run", new Date("2026-10-20T08:01:02.345Z"));
    expect(id).toMatch(/^dry-run-20261020t080102z-[0-9a-f]{8}$/);
    expect(runId("apply", new Date())).not.toBe(runId("apply", new Date()));
    const staged = { installer: "a".repeat(64), runner: "b".repeat(64), ciphertext: "c".repeat(64) };
    const line = startCommand(id, staged, installerArgs(false), { restoreLink: true, restoreMarker: false });
    expect(line).toStartWith("sudo sh -c ");
    expect(line).toEndWith(" < /dev/null");
    expect(line).toContain(`install -m 0700 ${STATE_DIR}/run-installer ${STATE_DIR}/${id}.runner || `);
    expect(line).toContain(`printf '\\''%s  %s\\n'\\'' ${"b".repeat(64)} ${STATE_DIR}/${id}.runner | sha256sum -c --status -`);
    expect(line).toContain(`exec systemd-run --unit=${RUN_UNIT_PREFIX}${id} --collect --quiet --setenv=HOME=/root -p '\\''ExecStopPost=/bin/bash ${STATE_DIR}/${id}.runner post ${id} 1 0'\\'' /bin/bash ${STATE_DIR}/${id}.runner run ${id} ${"a".repeat(64)} ${"c".repeat(64)} ${installerArgs(false).join(" ")}`);
    expect(() => startCommand("x; rm -rf /", staged, [], { restoreLink: false, restoreMarker: false })).toThrow(/invalid run id/);
    expect(() => startCommand(id, { ...staged, runner: "$(reboot)" }, [], { restoreLink: false, restoreMarker: false })).toThrow(/invalid sha256/);
    // Without the confirmed runner at its place, nothing starts (run here without sudo).
    const refused = Bun.spawnSync(["sh", "-c", line.replace(/^sudo /, "")]);
    expect(refused.exitCode).toBe(START_REFUSED);
    expect(refused.stderr.toString()).toContain("is not the one in the confirmed plan; nothing ran");
  });

  describe("an unclear answer to the start command", () => {
    const staged = { installer: "a".repeat(64), runner: "b".repeat(64), ciphertext: "c".repeat(64) };
    const restore = { restoreLink: true, restoreMarker: true };
    /** The demo machine after the start command: `start` is its answer, then the unit and the run's files. */
    const host = (start: RunResult, unit: string, files: string) =>
      fakeBox((command) => {
        if (command.startsWith("systemctl list-units")) return ok("");
        if (command.startsWith("sudo sh -c ") && command.includes("systemd-run")) return start;
        if (command.startsWith(`systemctl show ${RUN_UNIT_PREFIX}`)) return ok(unit);
        if (command.startsWith("sudo sh -c ") && command.includes(".log")) return ok(files);
        throw new Error(`unexpected command ${command}`);
      });
    const dropped: RunResult = { stdout: "", stderr: "Connection closed by remote host", exitCode: 255 };
    const now = new Date("2026-10-20T08:00:00Z");

    test("ssh dropped and the demo machine shows nothing: the run may have started, so StillRunningError keeps the lock", async () => {
      const error = await startRun(host(dropped, "LoadState=not-found\n", ""), log, false, staged, restore, now).catch((e: unknown) => e);
      expect(error).toBeInstanceOf(StillRunningError);
      expect(String(error)).toMatch(/failed with exit 255 .*could not show whether it started/);
      // And install --apply keeps the lab lock on it.
      const calls: string[] = [];
      const lock = async () => async () => void calls.push("release");
      const box = host(dropped, "LoadState=not-found\n", "");
      await expect(
        applyInstall({
          lock,
          plan: async () => ({}) as InstallPlan,
          confirm: () => {},
          backupZone: async () => {},
          record: async () => {},
          run: async () => waitForRun(box, await startRun(box, log, false, staged, restore, now), log),
          cleanup: async () => void calls.push("cleanup"),
          note: () => {},
        }),
      ).rejects.toBeInstanceOf(StillRunningError);
      expect(calls).toEqual([]);
    });

    test("ssh dropped but the unit or its files are there: the run is followed", async () => {
      await expect(startRun(host(dropped, "LoadState=loaded\nActiveState=active\n", ""), log, false, staged, restore, now)).resolves.toMatch(/^apply-20261020t080000z-[0-9a-f]{8}$/);
      await expect(startRun(host(dropped, "LoadState=not-found\n", "/var/lib/trustiot-demo/x.log\n"), log, false, staged, restore, now)).resolves.toMatch(/^apply-/);
    });

    test("only the explicit refusal means nothing started", async () => {
      const refusal: RunResult = { stdout: "", stderr: "/var/lib/trustiot-demo/run-installer is not the one in the confirmed plan; nothing ran", exitCode: START_REFUSED };
      const error = await startRun(host(refusal, "LoadState=not-found\n", ""), log, false, staged, restore, now).catch((e: unknown) => e);
      expect(error).not.toBeInstanceOf(StillRunningError);
      expect(String(error)).toContain("starting the installer was refused");
    });
  });

  test("only installer runs' units count as a run in progress", () => {
    const units = [
      `${RUN_UNIT_PREFIX}apply-20261020t080102z-0a1b2c3d.service loaded active running /bin/bash /var/lib/trustiot-demo/x.runner run`,
      "cloudflared.service loaded active running cloudflared",
      "trustiot-demo-other.service loaded inactive dead x",
      "",
    ].join("\n");
    expect(runningInstallUnits(units)).toEqual([`${RUN_UNIT_PREFIX}apply-20261020t080102z-0a1b2c3d.service`]);
    expect(runningInstallUnits("")).toEqual([]);
  });
});

describe("tunnel.ts: following a run and the lab lock", () => {
  /** A fake clock: sleeping moves time on; the run never ends. */
  const clock = () => {
    let now = 0;
    return { now: () => now, sleep: async (ms: number) => void (now += ms) };
  };

  test("a run that outlives the wait throws StillRunningError naming the unit, the log and the lock", async () => {
    const box = fakeBox((command) => {
      if (command.includes(".post") || command.includes(".log")) return ok("");
      if (command.startsWith("systemctl show")) return ok("LoadState=loaded\nActiveState=active\nSubState=running\n");
      throw new Error(`unexpected command ${command}`);
    });
    const error = await waitForRun(box, "apply-1", log, clock()).catch((e: unknown) => e);
    expect(error).toBeInstanceOf(StillRunningError);
    expect(String(error)).toContain(`systemctl status ${RUN_UNIT_PREFIX}apply-1`);
    expect(String(error)).toContain(`cat ~/${LOCK_DIR}/owner`);
    expect(String(error)).toContain(`sudo systemctl stop ${RUN_UNIT_PREFIX}apply-1`);
  });

  test("a run whose unit is gone without a .post file is not called finished", async () => {
    const box = fakeBox((command) => (command.startsWith("systemctl show") ? ok("LoadState=not-found\nActiveState=inactive\n") : ok("")));
    const time = clock();
    const error = await waitForRun(box, "apply-2", log, time).catch((e: unknown) => e);
    expect(error).toBeInstanceOf(StillRunningError);
    expect(String(error)).toContain("its unit is gone but it wrote no /var/lib/trustiot-demo/apply-2.post");
    // Found out within a few polls, not at the end of the hour.
    expect(time.now()).toBeLessThan(60_000);
  });

  test("a finished run returns its exit status, its log and what ExecStopPost found", async () => {
    const box = fakeBox((command) => {
      if (command.includes("apply-3.post")) return ok("service_result=success\nexit_code=exited\nexit_status=0\nlink=restored\nmarker=present\n");
      if (command.includes("apply-3.log")) return ok("+ write x\n");
      if (command.includes("apply-3.exit")) return ok("0\n");
      throw new Error(`unexpected command ${command}`);
    });
    expect(await waitForRun(box, "apply-3", log, clock())).toEqual({ id: "apply-3", exitCode: 0, output: "+ write x\n", post: { service_result: "success", exit_code: "exited", exit_status: "0", link: "restored", marker: "present" } });
  });

  function steps(overrides: Partial<ApplySteps>, calls: string[]): ApplySteps {
    const outcome: RunOutcome = { id: "apply-1", exitCode: 0, output: "", post: { link: "restored", marker: "restored" } };
    return {
      lock: async () => {
        calls.push("lock");
        return async () => void calls.push("release");
      },
      plan: async () => {
        calls.push("plan");
        return { cloudflared: { restoreLink: true, restoreMarker: true } } as InstallPlan;
      },
      confirm: () => void calls.push("confirm"),
      backupZone: async () => void calls.push("backup"),
      record: async (name) => void calls.push(name),
      run: async () => {
        calls.push("run");
        return outcome;
      },
      cleanup: async () => void calls.push("cleanup"),
      note: (line) => void calls.push(`note: ${line}`),
      ...overrides,
    };
  }

  test("install --apply plans again under the lock and deletes the hand-off files only after a run that succeeded", async () => {
    const calls: string[] = [];
    await applyInstall(steps({}, calls));
    expect(calls).toEqual(["lock", "plan", "confirm", "backup", "host-before", "run", "host-after", "cleanup", "release"]);

    const failed: string[] = [];
    const run = async () => ({ id: "apply-2", exitCode: 1, output: "dnf failed", post: { link: "restored", marker: "restored" } });
    await expect(applyInstall(steps({ run }, failed))).rejects.toThrow(/the installer failed \(exit 1\).*hand-off key and the ciphertext stay/);
    expect(failed).not.toContain("cleanup");
    expect(failed.at(-1)).toBe("release");

    const killed: string[] = [];
    const killedRun = async () => ({ id: "apply-3", exitCode: null, output: "", post: { service_result: "signal", link: "conflict", marker: "restored" } });
    await expect(applyInstall(steps({ run: killedRun }, killed))).rejects.toThrow(/exit none, signal/);
    expect(killed).toContain(`note: FAIL the run's ExecStopPost did not put back: ${LOCAL_LINK} (link=conflict); see docs/runbooks/demo-environment.md 「cloudflared 的更新與退回」`);
    expect(killed).not.toContain("cleanup");

    // The installer succeeded, but the link or the marker is not back: not done, the hand-off files stay.
    for (const post of [
      { link: "conflict", marker: "restored" } as Record<string, string>,
      { link: "restored", marker: "failed" },
      { link: "present", marker: "not-asked" },
      { link: "restored" },
    ]) {
      const notBack: string[] = [];
      const done = async () => ({ id: "apply-4", exitCode: 0, output: "", post: { service_result: "success", ...post } });
      await expect(applyInstall(steps({ run: done }, notBack))).rejects.toThrow(/the installer finished, but .* was not put back: the install is not done/);
      expect(notBack).not.toContain("cleanup");
      expect(notBack.at(-1)).toBe("release");
    }
    expect(restoreProblems({ link: "present", marker: "restored" }, { link: true, marker: true })).toEqual([]);
    expect(restoreProblems({ link: "not-asked", marker: "not-asked" }, { link: false, marker: false })).toEqual([]);
    expect(restoreProblems({}, { link: true, marker: false })).toEqual([`${LOCAL_LINK} (link=missing)`]);

    const changed: string[] = [];
    const confirm = () => {
      throw new Error("the plan changed");
    };
    await expect(applyInstall(steps({ confirm }, changed))).rejects.toThrow(/plan changed/);
    expect(changed).toEqual(["lock", "plan", "release"]);
  });

  test("a run that may still be going keeps the lab lock: the timeout path never releases it", async () => {
    const calls: string[] = [];
    const box = fakeBox((command) => (command.startsWith("systemctl show") ? ok("LoadState=loaded\nActiveState=active\n") : ok("")));
    const run = () => waitForRun(box, "apply-4", log, clock());
    await expect(applyInstall(steps({ run }, calls))).rejects.toBeInstanceOf(StillRunningError);
    expect(calls).not.toContain("release");
    expect(calls).not.toContain("cleanup");
    expect(calls.at(-1)).toBe(`note: the lab lock stays held for ${ISSUE}: the installer run may still be going`);

    // The same for the dry run that --apply makes again under the lock.
    const planCalls: string[] = [];
    const plan = async () => {
      await waitForRun(box, "dry-run-5", log, clock());
      return {} as InstallPlan;
    };
    await expect(applyInstall(steps({ plan }, planCalls))).rejects.toBeInstanceOf(StillRunningError);
    expect(planCalls).toEqual(["lock", `note: the lab lock stays held for ${ISSUE}: the installer run may still be going`]);
  });

  test("--lock-held accepts only this issue's exact owner line", async () => {
    const owned = (owner: string) => fakeBox((command) => (command.startsWith("cat ") ? ok(`${owner}\n`) : ok("")));
    await expect(takeLock(owned(`${ISSUE} 2026-10-20T08:00:00Z demo tunnel-install`), ISSUE, "tunnel-install", true)).resolves.toBeFunction();
    for (const other of ["Laysi/trustiot-v2#4660 2026-10-20T08:00:00Z demo x", "Laysi/trustiot-v2#465 2026-10-20T08:00:00Z demo cleanup", "", `${ISSUE}`]) {
      await expect(takeLock(owned(other), ISSUE, "tunnel-install", true)).rejects.toThrow(/--lock-held/);
    }
  });

  test("the dry run refuses under someone else's lab lock and runs under a free one or this issue's", () => {
    expect(() => dryRunLockCheck({ kind: "free" })).not.toThrow();
    expect(() => dryRunLockCheck({ kind: "held", owner: `${ISSUE} 2026-10-20T08:00:00Z demo tunnel-install` })).not.toThrow();
    for (const other of ["Laysi/trustiot-v2#4660 2026-10-20T08:00:00Z x", "Laysi/trustiot-v2#465 2026-10-20T08:00:00Z demo cleanup", "e2e-ingress 2026-10-20"]) {
      expect(() => dryRunLockCheck({ kind: "held", owner: other })).toThrow(/the dry run waits for it/);
    }
    expect(() => dryRunLockCheck({ kind: "unreadable" })).toThrow(/its owner cannot be read/);
  });

  test("a lock directory without a readable owner keeps the dry run out", () => {
    expect(parseLockState("LOCK_FREE\n")).toEqual({ kind: "free" });
    expect(parseLockState(`LOCK_HELD ${ISSUE} 2026-10-20T08:00:00Z demo tunnel-install\n`)).toEqual({ kind: "held", owner: `${ISSUE} 2026-10-20T08:00:00Z demo tunnel-install` });
    for (const out of ["LOCK_UNREADABLE\n", "", "LOCK_HELD \n", "LOCK_HELD a\nb", "LOCK_FREE\nLOCK_HELD x", "something else"]) {
      expect(parseLockState(out)).toEqual({ kind: "unreadable" });
    }
  });

  test("the lock command tells a missing lock from an ownerless one", () => {
    // Runs the real command with sh against a throwaway home directory.
    const home = mkdtempSync(join(tmpdir(), "lock-"));
    const run = () => parseLockState(Bun.spawnSync(["sh", "-c", LOCK_STATE_COMMAND], { env: { HOME: home, PATH: process.env.PATH ?? "/usr/bin:/bin" } }).stdout.toString());
    try {
      expect(run()).toEqual({ kind: "free" });
      mkdirSync(join(home, LOCK_DIR));
      expect(run()).toEqual({ kind: "unreadable" });
      writeFileSync(join(home, LOCK_DIR, "owner"), "");
      expect(run()).toEqual({ kind: "unreadable" });
      writeFileSync(join(home, LOCK_DIR, "owner"), "Laysi/trustiot-v2#465 2026-10-20T08:00:00Z demo cleanup\n");
      expect(run()).toEqual({ kind: "held", owner: "Laysi/trustiot-v2#465 2026-10-20T08:00:00Z demo cleanup" });
    } finally {
      rmSync(home, { recursive: true, force: true });
    }
  });

  test("the dry run reads the lock itself, also with --lock-held", async () => {
    const owned = (out: string) => fakeBox((command) => (command === LOCK_STATE_COMMAND ? ok(out) : ok("")));
    await expect(checkLockForDryRun(owned("LOCK_HELD Laysi/trustiot-v2#465 2026-10-20T08:00:00Z demo cleanup\n"))).rejects.toThrow(/held by: Laysi\/trustiot-v2#465/);
    await expect(checkLockForDryRun(owned(`LOCK_HELD ${ISSUE} 2026-10-20T08:00:00Z demo tunnel-install\n`))).resolves.toBeUndefined();
    await expect(checkLockForDryRun(owned("LOCK_FREE\n"))).resolves.toBeUndefined();
    await expect(checkLockForDryRun(owned("LOCK_UNREADABLE\n"))).rejects.toThrow(/its owner cannot be read/);
    // install's dry-run branch calls it with nothing that --lock-held could switch off.
    const source = readFileSync(join(import.meta.dir, "tunnel.ts"), "utf8");
    const dryRun = /if \(!options\.apply\) \{\n([\s\S]*?)\n {4}return;\n {2}\}/.exec(source.slice(source.indexOf("async function install(")))?.[1] ?? "";
    expect(dryRun.split("\n")[0]).toBe("    await checkLockForDryRun(box);");
    expect(dryRun).not.toContain("lockHeld");
  });
});

describe("tunnel.ts: the plan", () => {
  test("names the tunnel the installer read from the token, exactly once", () => {
    expect(tunnelIdFromInstallerOutput(dryRunOutput())).toBe(TUNNEL);
    expect(tunnelIdFromInstallerOutput(`  tunnel token: kept the one in ${CREDENTIAL} (tunnel ${TUNNEL})\n`)).toBe(TUNNEL);
    expect(() => tunnelIdFromInstallerOutput("+ write x\n")).toThrow(/named 0 tunnel/);
    expect(() => tunnelIdFromInstallerOutput(`  tunnel token: tunnel ${TUNNEL}\n  tunnel token: tunnel ${TUNNEL}\n`)).toThrow(/named 2 tunnel/);
    // Only the installer's info line counts, not a change or a warning that mentions a tunnel.
    expect(() => tunnelIdFromInstallerOutput(`! tunnel token: tunnel ${TUNNEL}\n`)).toThrow(/named 0/);
  });

  test("the change lines of the 2026-10-11 lab run are all on the allow-list", () => {
    const output = readFileSync(join(ROOT, "docs/test-reports/data/2026-10-11-64-saas-host/lab-dryrun-3.txt"), "utf8");
    const changes = installerChanges(output);
    expect(changes).toHaveLength(12);
    expect(() => checkChanges(changes, [], "cloudflared-e2e-64")).not.toThrow();
    expect(() => checkChanges(installerChanges(dryRunOutput()), [])).not.toThrow();
  });

  test("any other change stops the plan unless it is named with --allow-change", () => {
    for (const other of ["enable and start firewalld", "remove service https from zone FedoraServer", "firewalld (permanent): remove service https from FedoraServer", "create /etc/credstore (root only)", "install k3s v1.36.5+k3s1 with the official installer (SHA-256 pinned)"]) {
      expect(() => checkChanges([...installerChanges(dryRunOutput()), other], [])).toThrow(/outside what this script runs.*--allow-change/);
      expect(() => checkChanges([...installerChanges(dryRunOutput()), other], [other])).not.toThrow();
    }
    // Another unit's files are not this unit's.
    expect(() => checkChanges(["write /etc/credstore/cloudflared.tunnel-token: tunnel token (root only)"], [])).toThrow(/outside/);
    expect(allowedChanges()).toContain(`firewalld (runtime): in the trusted zone, ${POD_SSH_RULE}`);
    expect(allowedChanges().some((line) => line.includes("cloudflared.service"))).toBe(false);
  });

  test("the plan and the warnings are the installer's own lines", () => {
    const output = dryRunOutput(TUNNEL, []) + "! /etc/systemd/system/cloudflared-demo.service.d exists\n";
    expect(installerChanges(output)[0]).toBe(`firewalld (runtime): in the trusted zone, ${POD_SSH_RULE}`);
    expect(installerWarnings(output)).toEqual(["/etc/systemd/system/cloudflared-demo.service.d exists"]);
  });

  test("a checkout with uncommitted changes under scripts/demo or deploy/host cannot plan", async () => {
    const repo = join(dir, "repo");
    mkdirSync(join(repo, "scripts/demo"), { recursive: true });
    mkdirSync(join(repo, "docs"), { recursive: true });
    writeFileSync(join(repo, "scripts/demo/tunnel.ts"), "// reviewed\n");
    const git = (...args: string[]) => {
      const run = Bun.spawnSync(["git", "-C", repo, "-c", "user.name=test", "-c", "user.email=test@example.invalid", "-c", "commit.gpgsign=false", "-c", "core.hooksPath=/dev/null", ...args]);
      expect(run.exitCode).toBe(0);
      return run.stdout.toString().trim();
    };
    git("init", "-q");
    git("add", ".");
    git("commit", "-q", "-m", "reviewed");
    const head = git("rev-parse", "HEAD");
    expect(await codeState(repo)).toEqual({ head });
    // Elsewhere in the repository: not what the plan runs.
    writeFileSync(join(repo, "docs/notes.md"), "draft\n");
    expect(await codeState(repo)).toEqual({ head });
    writeFileSync(join(repo, "scripts/demo/tunnel.ts"), "// changed after the review\n");
    await expect(codeState(repo)).rejects.toThrow(/uncommitted changes/);
    git("checkout", "--", "scripts/demo/tunnel.ts");
    writeFileSync(join(repo, "scripts/demo/new.ts"), "// untracked\n");
    await expect(codeState(repo)).rejects.toThrow(/uncommitted changes/);
  });

  /**
   * The demo machine as makePlan sees it: the update units, the link and the marker, the metrics
   * port, staging, the run's unit and its files. Returns the commands it was sent.
   */
  function planHost(options: { output?: string; timer?: boolean; link?: string; portHolder?: string; candidate?: string } = {}) {
    const commands: string[] = [];
    let stored: string | Uint8Array = "";
    let id = "";
    const box = fakeBox((command, stdin) => {
      commands.push(command);
      if (command.startsWith("systemctl show cloudflared-update.timer")) {
        return ok(
          options.timer
            ? "Id=cloudflared-update.timer\nLoadState=loaded\nUnitFileState=enabled\nActiveState=active\n\nId=cloudflared-update.service\nLoadState=loaded\nUnitFileState=static\nActiveState=inactive\n"
            : "Id=cloudflared-update.timer\nLoadState=not-found\nUnitFileState=\nActiveState=inactive\n\nId=cloudflared-update.service\nLoadState=not-found\nUnitFileState=\nActiveState=inactive\n",
        );
      }
      if (command === fileTypeCommand(LOCAL_LINK)) return ok(`${options.link ?? "symbolic link"}\n`);
      if (command.startsWith(`readlink ${LOCAL_LINK}`)) return ok("/usr/bin/cloudflared\n");
      if (command.startsWith(`if [ -L ${PACKAGE_MARKER}`)) return ok("file\n");
      if (command.startsWith(`systemctl show ${DEMO_TUNNEL_UNIT}.service -p MainPID`)) return ok("MainPID=0\n");
      if (command.startsWith("sudo ss -Htlnp")) return ok(options.portHolder ?? "");
      if (command.startsWith("sudo install -d")) return ok();
      if (command.startsWith("mktemp -p")) return ok("/run/user/1000/trustiot-demo.x\n");
      if (command.startsWith("cat > ")) {
        stored = stdin ?? "";
        return ok();
      }
      if (command.startsWith("sudo install -m")) return ok(`${sha(stored)}  target\n`);
      if (command.startsWith("rm -f ")) return ok();
      if (command.startsWith("systemctl list-units")) return ok("");
      if (command.startsWith("sudo sh -c ") && command.includes("systemd-run")) {
        id = /--unit=trustiot-demo-install-([a-z0-9-]+)/.exec(command)?.[1] ?? "";
        return ok();
      }
      if (command.includes(`${id}.post`)) return ok("service_result=success\nexit_code=exited\nexit_status=0\nlink=not-asked\nmarker=not-asked\n");
      if (command.includes(`${id}.log`)) return ok(options.output ?? dryRunOutput());
      if (command.includes(`${id}.exit`)) return ok("0\n");
      if (command.startsWith("d=$(mktemp -d")) return ok(`${options.candidate ?? "2026.10.1-1"}\n`);
      if (command === INSTALLED_VERSION_COMMAND) return ok("2026.10.0-1\n");
      if (command.startsWith("dnf -q repoquery --installed")) return ok("@commandline\n");
      throw new Error(`unexpected command ${command}`);
    });
    return { box, commands };
  }
  const inputs = { tunnelId: TUNNEL, allowedExtra: [], installer: new TextEncoder().encode("installer"), ciphertext: Buffer.alloc(512, 7).toString("base64") };
  const env = { code: async () => ({ head: "f".repeat(40) }), clock: { now: () => 0, sleep: async () => {} } };

  test("makePlan binds the commit, the tunnel, the files and the cloudflared swap", async () => {
    const { box, commands } = planHost();
    const plan = await makePlan(box, log, inputs, { pid: "1005" }, env);
    expect(plan).toMatchObject({
      unit: DEMO_TUNNEL_UNIT,
      tunnelId: TUNNEL,
      head: "f".repeat(40),
      installer: sha("installer"),
      ciphertext: sha(`${inputs.ciphertext}\n`),
      args: installerArgs(false),
      allowedExtra: [],
      metricsPort: DEMO_METRICS_PORT,
      cloudflared: { swap: true, installed: "2026.10.0-1", fromRepo: "@commandline", candidate: "2026.10.1-1", restoreLink: true, restoreMarker: true },
    });
    expect(plan.changes).toEqual(installerChanges(dryRunOutput()));
    expect(plan.updateUnits.map((u) => u.load)).toEqual(["not-found", "not-found"]);
    // The dry run's own unit asks ExecStopPost to restore nothing.
    expect(commands.find((c) => c.includes("systemd-run"))).toContain(" post dry-run-");
    expect(commands.find((c) => c.includes("systemd-run"))).toMatch(/ post dry-run-[a-z0-9-]+ 0 0/);
  });

  test("makePlan stops when the token belongs to another tunnel", async () => {
    const other = "0b6c1c3e-4a59-4f33-9d55-5b8a2d7f9e10";
    await expect(makePlan(planHost({ output: dryRunOutput(other) }).box, log, inputs, { pid: "1005" }, env)).rejects.toThrow(`the token belongs to tunnel ${other}, not --tunnel-id ${TUNNEL}`);
  });

  test("makePlan stops on a change outside the allow-list, on an update timer, a link that is a file, or a taken metrics port", async () => {
    await expect(makePlan(planHost({ output: dryRunOutput(TUNNEL, ["enable and start firewalld"]) }).box, log, inputs, { pid: "1" }, env)).rejects.toThrow(/outside what this script runs/);
    await expect(makePlan(planHost({ output: dryRunOutput(TUNNEL, ["enable and start firewalld"]) }).box, log, { ...inputs, allowedExtra: ["enable and start firewalld"] }, { pid: "1" }, env)).resolves.toMatchObject({ allowedExtra: ["enable and start firewalld"] });
    const timer = planHost({ timer: true });
    await expect(makePlan(timer.box, log, inputs, { pid: "1" }, env)).rejects.toThrow(/cloudflared-update.timer \(loaded, enabled, active\).*self-update/);
    // Refused before anything was staged.
    expect(timer.commands.some((c) => c.startsWith("cat > "))).toBe(false);
    await expect(makePlan(planHost({ link: "regular file" }).box, log, inputs, { pid: "1" }, env)).rejects.toThrow(/is a regular file/);
    await expect(makePlan(planHost({ portHolder: 'LISTEN 0 4096 127.0.0.1:20250 0.0.0.0:* users:(("x",pid=77,fd=3))' }).box, log, inputs, { pid: "1" }, env)).rejects.toThrow(/metrics port, is taken/);
    await expect(makePlan(planHost().box, log, inputs, { pid: "1" }, { ...env, code: async () => Promise.reject(new Error("uncommitted changes")) })).rejects.toThrow(/uncommitted/);
  });

  test("no swap, no repository query: an unreachable repository does not block a token rotation", async () => {
    const rotation = [`  tunnel token: tunnel ${TUNNEL}`, "+ would replace /etc/credstore/cloudflared-demo.tunnel-token: tunnel token (root only)", "+ would restart cloudflared-demo.service (open requests get up to 30 seconds to finish, then about 4 seconds without a connection)", ""].join("\n");
    const { box, commands } = planHost({ output: rotation, candidate: "Error: Failed to download metadata" });
    const plan = await makePlan(box, log, inputs, { pid: "1" }, env);
    expect(plan.cloudflared).toMatchObject({ swap: false, candidate: "2026.10.0-1", restoreLink: false, restoreMarker: false });
    expect(commands.some((c) => c.includes("--repofrompath"))).toBe(false);
    // With a swap the repository's version is required.
    await expect(makePlan(planHost({ candidate: "Error: Failed to download metadata" }).box, log, inputs, { pid: "1" }, env)).rejects.toThrow(/could not read the cloudflared version/);
  });

  test("readCloudflaredFacts reads the repository only for a swap", async () => {
    const { box, commands } = planHost();
    expect((await readCloudflaredFacts(box, false)).candidate).toBe("");
    expect(commands.some((c) => c.includes("--repofrompath"))).toBe(false);
    expect((await readCloudflaredFacts(box, true)).candidate).toBe("2026.10.1-1");
  });
});

describe("tunnel.ts: the shared cloudflared binary", () => {
  test("the link type and the installed version read the same in any session language", () => {
    // Stand-ins that answer like coreutils' stat and rpm do (checked 2026-10-20 with LANG=zh_TW.UTF-8):
    // stat -c %F names the type in the session's language; rpm -q prints "not installed" on stdout,
    // translated, and exits 1; rpm -q --quiet prints nothing.
    const bin = mkdtempSync(join(tmpdir(), "locale-"));
    try {
      writeFileSync(
        join(bin, "stat"),
        `#!/bin/sh\nloc=\${LC_ALL:-\${LC_MESSAGES:-\${LANG:-C}}}\ncase "$loc" in C | C.* | POSIX) echo "symbolic link" ;; *) echo "符號連結" ;; esac\n`,
        { mode: 0o755 },
      );
      writeFileSync(
        join(bin, "rpm"),
        `#!/bin/sh\nif [ "$FAKE_INSTALLED" = 1 ]; then case "$*" in *--quiet*) exit 0 ;; *) echo 2026.10.0-1; exit 0 ;; esac; fi\ncase "$*" in *--quiet*) exit 1 ;; esac\necho "軟體包 cloudflared 尚未安裝"\nexit 1\n`,
        { mode: 0o755 },
      );
      const sh = (command: string, installed = "1") =>
        Bun.spawnSync(["sh", "-c", command], { env: { PATH: `${bin}:/usr/bin:/bin`, LANG: "zh_TW.UTF-8", FAKE_INSTALLED: installed } }).stdout.toString().trim();
      expect(linkFacts(sh(fileTypeCommand(LOCAL_LINK)), "/usr/bin/cloudflared\n")).toEqual({ type: "symlink", target: "/usr/bin/cloudflared" });
      expect(sh(INSTALLED_VERSION_COMMAND)).toBe("2026.10.0-1");
      expect(sh(INSTALLED_VERSION_COMMAND, "0")).toBe("");
    } finally {
      rmSync(bin, { recursive: true, force: true });
    }
  });

  const facts: CloudflaredFacts = { installed: "2026.10.0-1", fromRepo: "@commandline", candidate: "2026.10.1-1", link: { type: "symlink", target: "/usr/bin/cloudflared" }, marker: "file" };

  test("a package from elsewhere is swapped for the repository's version, said explicitly", () => {
    const plan = cloudflaredPlan(facts, true);
    expect(plan).toMatchObject({ swap: true, installed: "2026.10.0-1", fromRepo: "@commandline", candidate: "2026.10.1-1", restoreLink: true, restoreMarker: true });
    const lines = describeCloudflared(plan, { pid: "1005" }).join("\n");
    expect(lines).toContain("replaces /usr/bin/cloudflared 2026.10.0-1 (from @commandline) with 2026.10.1-1");
    expect(lines).toContain("a version change, 2026.10.0-1 -> 2026.10.1-1");
    expect(lines).toContain(`the ops tunnel ${OPS_TUNNEL_UNIT} runs the same /usr/bin/cloudflared; its running process (PID 1005)`);
    expect(lines).toContain(`${LOCAL_LINK} and ${PACKAGE_MARKER}; the run's ExecStopPost puts them back`);
  });

  test("the same version from elsewhere is still a swap, without a version change", () => {
    expect(describeCloudflared(cloudflaredPlan({ ...facts, candidate: "2026.10.0-1" }, true), { pid: "1005" }).join("\n")).toContain("same version, now Cloudflare's signed package");
  });

  test("without a swap nothing is put back, whatever the repository offers", () => {
    const plan = cloudflaredPlan({ ...facts, fromRepo: "cloudflared-stable", candidate: "" }, false);
    expect(plan).toMatchObject({ swap: false, candidate: "2026.10.0-1", restoreLink: false, restoreMarker: false });
    expect(describeCloudflared(plan, { pid: "1" })).toEqual(["cloudflared: 2026.10.0-1 from cloudflared-stable stays as it is (the installer does not replace it)"]);
  });

  test("only what exists now is put back", () => {
    expect(cloudflaredPlan({ ...facts, link: { type: "absent", target: "" }, marker: "absent" }, true)).toMatchObject({ restoreLink: false, restoreMarker: false });
  });

  test("a link that is a file, another type, or points elsewhere stops the plan", () => {
    expect(linkFacts("symbolic link\n", "/usr/bin/cloudflared\n")).toEqual({ type: "symlink", target: "/usr/bin/cloudflared" });
    expect(linkFacts("regular file", "")).toEqual({ type: "file", target: "" });
    expect(linkFacts("regular empty file", "")).toEqual({ type: "file", target: "" });
    expect(linkFacts("absent", "")).toEqual({ type: "absent", target: "" });
    expect(linkFacts("directory", "")).toEqual({ type: "other", target: "" });
    expect(() => cloudflaredPlan({ ...facts, link: { type: "file", target: "" } }, true)).toThrow(`${LOCAL_LINK} is a regular file`);
    expect(() => cloudflaredPlan({ ...facts, link: { type: "file", target: "" } }, false)).toThrow(/regular file/);
    expect(() => cloudflaredPlan({ ...facts, link: { type: "other", target: "" } }, false)).toThrow(/unexpected type/);
    expect(() => cloudflaredPlan({ ...facts, link: { type: "symlink", target: "/opt/cloudflared" } }, false)).toThrow("points to /opt/cloudflared");
    expect(() => cloudflaredPlan({ ...facts, marker: "other" }, false)).toThrow(/not a regular file/);
  });

  test("without the repository's version there is no swap plan", () => {
    expect(() => cloudflaredPlan({ ...facts, candidate: "" }, true)).toThrow(/could not read the cloudflared version/);
    expect(() => cloudflaredPlan({ ...facts, candidate: "Error: Failed to download metadata" }, true)).toThrow(/could not read/);
    expect(() => cloudflaredPlan({ ...facts, installed: "" }, false)).toThrow(/not installed/);
  });

  test("cloudflared's update timer or service stops the plan", () => {
    const none = unitStates("Id=cloudflared-update.timer\nLoadState=not-found\nUnitFileState=\nActiveState=inactive\n\nId=cloudflared-update.service\nLoadState=not-found\nUnitFileState=\nActiveState=inactive\n");
    expect(none.map((u) => u.name)).toEqual(["cloudflared-update.timer", "cloudflared-update.service"]);
    expect(() => requireNoUpdateTimer(none)).not.toThrow();
    const timer = [{ ...none[0]!, load: "loaded", unitFile: "enabled", active: "active" }, none[1]!];
    expect(() => requireNoUpdateTimer(timer)).toThrow(/cloudflared-update.timer \(loaded, enabled, active\)/);
    const disabledService = [none[0]!, { ...none[1]!, load: "loaded", unitFile: "static" }];
    expect(() => requireNoUpdateTimer(disabledService)).toThrow(/cloudflared-update.service/);
    expect(() => requireNoUpdateTimer([])).toThrow(/could not be read/);
  });

  test("status reports the link, the marker and the timer with OK, WARN and FAIL", () => {
    const none = unitStates("Id=cloudflared-update.timer\nLoadState=not-found\nUnitFileState=\nActiveState=inactive\n");
    const link = { type: "symlink" as const, target: "/usr/bin/cloudflared" };
    expect(linkReport(link, "file", none)).toEqual([`OK ${LOCAL_LINK} -> /usr/bin/cloudflared`, `OK ${PACKAGE_MARKER} present`, "OK cloudflared-update.timer not installed"]);
    const noLink = linkReport({ type: "absent", target: "" }, "file", none);
    expect(noLink[0]).toStartWith(`WARN ${LOCAL_LINK} is absent: after a dnf upgrade of cloudflared, put it back`);
    const noMarker = linkReport(link, "absent", none);
    expect(noMarker[1]).toStartWith(`WARN ${PACKAGE_MARKER} is absent`);
    const timerOn = [{ name: "cloudflared-update.timer", load: "loaded", unitFile: "enabled", active: "active" }];
    expect(linkReport(link, "absent", timerOn)[2]).toStartWith("FAIL cloudflared-update.timer is enabled/active and the marker is missing");
    expect(linkReport(link, "file", timerOn)[2]).toStartWith("WARN cloudflared-update.timer is loaded/enabled/active");
  });

  test("the metrics port is free or the demo connector's own", () => {
    const line = (pid: string) => `LISTEN 0 4096 127.0.0.1:20250 0.0.0.0:* users:(("cloudflared",pid=${pid},fd=7))`;
    expect(metricsPortHolder("", "0")).toBe("");
    expect(metricsPortHolder(line("4242"), "4242")).toBe("");
    expect(metricsPortHolder(line("1005"), "4242")).toContain("pid=1005");
    expect(metricsPortHolder(line("424"), "4242")).toContain("pid=424");
    expect(metricsPortHolder(line("1005"), "0")).toContain("pid=1005");
  });
});

describe("tunnel.ts: status", () => {
  test("reads the connector's ready connections", () => {
    expect(readyConnections('{"status":200,"readyConnections":4,"connectorId":"x"}')).toBe(4);
    expect(readyConnections("not json")).toBeUndefined();
  });

  test("the token audit passes the token and its secret to grep only as a 0600 pattern file and returns counts", () => {
    const secret = Buffer.from(crypto.getRandomValues(new Uint8Array(32))).toString("base64");
    const token = Buffer.from(JSON.stringify({ a: "0".repeat(32), t: TUNNEL, s: secret })).toString("base64");
    const credential = join(dir, "token");
    writeFileSync(credential, token, { mode: 0o600 });
    const searched = join(dir, "state");
    mkdirSync(join(searched, "sub"), { recursive: true });
    writeFileSync(join(searched, "clean.log"), "nothing here\n");
    writeFileSync(join(searched, "sub", "leak.log"), `run output ${token}\n`);
    const bin = join(dir, "bin");
    mkdirSync(bin);
    // The journal holds the secret once (in a command line field) and the token nowhere.
    writeFileSync(join(bin, "journalctl"), `#!/bin/sh\necho "MESSAGE=started"\necho "_CMDLINE=cloudflared --secret ${secret}"\necho "MESSAGE=done"\n`, { mode: 0o755 });
    const patterns = join(dir, "patterns");
    mkdirSync(patterns, { mode: 0o700 });
    const script = auditScript({ credential, dirs: [searched], patternDir: patterns });
    const run = Bun.spawnSync(["sh", "-c", script], { env: { PATH: `${bin}:/usr/bin:/bin` } });
    const out = run.stdout.toString();
    expect(run.exitCode).toBe(0);
    expect(out).toContain("patterns: 2 (the token, and its secret when it could be read)");
    expect(out).toContain("journal lines with the token or its secret: 1");
    expect(out).toContain(`with the token or its secret: 1`);
    expect(out + run.stderr.toString()).not.toContain(secret);
    expect(out + run.stderr.toString()).not.toContain(token);
    // The pattern file is gone.
    expect(readdirSync(patterns)).toEqual([]);
    // Every grep reads its patterns from the file; the token file is only ever stdin.
    const greps = [...script.matchAll(/grep [^|);]*/g)].map((m) => m[0].trim());
    expect(greps).toEqual([`grep -qx '' "$p"`, 'grep -c . "$p"', 'grep -cFf "$p"', `grep -rlFf "$p" ${searched} 2>/dev/null`]);
    expect(script).not.toMatch(/grep\s+(-\w*e|--regexp)/);
    expect(script.match(/"\$f"/g)?.length).toBe(3);
    expect(script).toContain(`< "$f"`);
    expect(script).not.toMatch(/\$\(cat|`cat/);
    // On the demo machine: the whole journal (every field) and both directories.
    expect(auditScript()).toContain("journalctl --no-pager -o verbose");
    expect(auditScript()).toContain(`grep -rlFf "$p" ${STATE_DIR} ${RUN_DIR}`);
    expect(auditScript()).toContain("mktemp -p /run trustiot-demo-audit.");
  });

  test("an empty token file is reported, never searched with an empty pattern", () => {
    const credential = join(dir, "token");
    writeFileSync(credential, "");
    const run = Bun.spawnSync(["sh", "-c", auditScript({ credential, dirs: [dir], patternDir: dir })]);
    expect(run.stdout.toString().trim()).toBe("no token file");
  });

  test("finds files on this workstation that hold a tunnel token's prefix", () => {
    mkdirSync(join(dir, "w", "sub"), { recursive: true });
    writeFileSync(join(dir, "w", "clean.log"), "nothing here\n");
    writeFileSync(join(dir, "w", "sub", "leak.json"), `{"t":"${TOKEN_PREFIX}xyz"}`);
    expect(filesWith(join(dir, "w"), TOKEN_PREFIX)).toEqual([join(dir, "w", "sub", "leak.json")]);
    expect(TOKEN_PREFIX).toBe("eyJhIjoi");
    expect(Buffer.from(TOKEN_PREFIX, "base64").toString()).toBe('{"a":"');
  });

  test("local paths are printed without the home directory, and a pasted ~/ path is read", () => {
    const home = process.env.HOME;
    try {
      process.env.HOME = dir;
      writeFileSync(join(dir, "ct.b64"), "AAAA\n");
      expect(readCiphertextFile("~/ct.b64")).toBe("AAAA\n");
      expect(readCiphertextFile(join(dir, "ct.b64"))).toBe("AAAA\n");
      expect(expandHome("~")).toBe(dir);
      expect(expandHome("/var/tmp/x")).toBe("/var/tmp/x");
      let message = "";
      try {
        readCiphertextFile(join(dir, "missing.b64"));
      } catch (error) {
        message = String(error);
      }
      expect(message).toContain("could not read --ciphertext ~/missing.b64");
      expect(message).not.toContain(dir);
    } finally {
      process.env.HOME = home;
    }
  });
});

describe("tunnel.ts: check", () => {
  const response = (status: number, headers: Record<string, string> = {}, body = "") => ({ status, headers, body });
  const accessRedirect = (host: string) => response(302, { location: `https://team-x.cloudflareaccess.com/cdn-cgi/access/login/${host}?kid=abc&redirect_url=%2F`, "cf-ray": "9a0-TPE" });

  test("parses curl -i output, skipping 100 Continue", () => {
    const out = "HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 418 I'm a teapot\r\nServer: cloudflare\r\nCF-RAY: abc-TPE\r\n\r\nbody";
    expect(parseCurlOutput(out)).toEqual({ status: 418, headers: { server: "cloudflare", "cf-ray": "abc-TPE" }, body: "body" });
    expect(parseCurlOutput("").status).toBe(0);
    expect(parseCurlOutput("HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\n").status).toBe(101);
  });

  test("tells the route's answer from Access, a challenge, a block, the connector's refusal and a missing connector", () => {
    for (const stage of [1, 2] as const) {
      expect(classify(response(STAGE1_STATUS, { server: "cloudflare" }), stage)).toBe("origin");
      expect(classify(response(101, { upgrade: "websocket" }), stage)).toBe("origin");
      // Access answers a request without a session with a 302, browser or not.
      expect(classify(accessRedirect(HOSTS.gateway), stage)).toBe("access");
      expect(classify(response(403, { "cf-mitigated": "challenge" }, "<html>"), stage)).toBe("challenge");
      expect(classify(response(503, {}, "<title>Just a moment...</title>"), stage)).toBe("challenge");
      expect(classify(response(403, { server: "cloudflare" }, "<span>Error 1010</span> Access denied"), stage)).toBe("block");
      expect(classify(response(403, {}, "Sorry, you have been blocked"), stage)).toBe("block");
      // The connector's Access JWT check writes the status only.
      expect(classify(response(403, { server: "cloudflare", "content-length": "0", "cf-ray": "1-TPE" }), stage)).toBe("refused");
      expect(classify(response(530, {}, "error code: 1033"), stage)).toBe("tunnel-down");
      expect(classify(response(418, { "cf-mitigated": "challenge" }), stage)).toBe("challenge");
      expect(classify(response(301, { location: "https://gw.example.com/mqtt" }), stage)).toBe("unexpected");
      expect(classify(response(502), stage)).toBe("unexpected");
      expect(classify(response(404), stage)).toBe("unexpected");
    }
  });

  test("a 403 from the demo ingress is the origin in stage 2, not the connector's refusal", () => {
    const envoy = response(403, { server: "cloudflare", "content-type": "text/plain" }, "RBAC: access denied");
    const service = response(403, { "content-type": "application/json" }, '{"error":"forbidden"}');
    const typedEmpty = response(403, { "content-type": "text/plain" });
    const bodyOnly = response(403, {}, "Forbidden");
    for (const r of [envoy, service, typedEmpty, bodyOnly]) {
      expect(classify(r, 2)).toBe("origin");
      // In stage 1 every route is an http_status service, so only the connector can send a 403.
      expect(classify(r, 1)).toBe("refused");
    }
    // A bypass path the ingress answers with 403 has reached it; a sub-path refusal must be the connector's.
    expect(probePasses("reached", classify(envoy, 2), 2)).toBe(true);
    expect(probePasses("refused", classify(envoy, 2), 2)).toBe(false);
    expect(probePasses("refused", classify(response(403, { server: "cloudflare" }, " \n"), 2), 2)).toBe(true);
  });

  test("a bypass path must reach the tunnel: the route's 418 in stage 1, anything Cloudflare and the connector let through in stage 2", () => {
    expect(probePasses("reached", "origin", 1)).toBe(true);
    for (const outcome of ["unexpected", "access", "challenge", "block", "refused", "tunnel-down"] as const) expect(probePasses("reached", outcome, 1)).toBe(false);
    for (const outcome of ["origin", "unexpected"] as const) expect(probePasses("reached", outcome, 2)).toBe(true);
    for (const outcome of ["access", "challenge", "block", "refused", "tunnel-down"] as const) expect(probePasses("reached", outcome, 2)).toBe(false);
    // Paths behind Access must be stopped by Access in both stages, never reach the tunnel.
    for (const stage of [1, 2] as const) {
      expect(probePasses("access", "access", stage)).toBe(true);
      for (const outcome of ["origin", "unexpected", "refused", "challenge"] as const) expect(probePasses("access", outcome, stage)).toBe(false);
      expect(probePasses("refused", "refused", stage)).toBe(true);
      expect(probePasses("refused", "origin", stage)).toBe(false);
    }
  });

  test("keeps the status and cf-ray, and never a host name, the team domain or a query string", () => {
    expect(JSON.stringify(summary({ ...accessRedirect(HOSTS["app"]), headers: { ...accessRedirect(HOSTS["app"]).headers, server: "cloudflare" } }, HOSTS))).toBe(
      '{"status":302,"body-bytes":0,"server":"cloudflare","cf-ray":"9a0-TPE","location":"https://<team>.cloudflareaccess.com/cdn-cgi/access/login/<app>"}',
    );
    // The body's size in bytes, never the body: it shows a refusal's shape in stage 1.
    expect(summary(response(403, { "content-type": "text/plain" }, "RBAC: access denied é"), HOSTS)).toEqual({ status: 403, "body-bytes": 22, "content-type": "text/plain" });
    expect(summary(response(403, { "cf-mitigated": "challenge", "cf-ray": "8f1-TPE" }), HOSTS)["cf-ray"]).toBe("8f1-TPE");
    expect(mask(`https://${HOSTS.gateway}/mqtt ${encodeURIComponent(HOSTS.ota)}`, HOSTS)).toBe("https://<gateway>/mqtt <ota>");
  });

  test("the probes: every host name behind Access, only the gateway client's exact paths bypass it", () => {
    const all = probes();
    const ws = all.find((p) => p.host === "gateway" && p.path === "/mqtt")!;
    const args = curlArgs(ws, HOSTS.gateway);
    expect(args.slice(0, 5)).toEqual(["curl", "-sS", "-i", "--http1.1", "--max-time"]);
    expect(args).toContain("User-Agent:");
    expect(args).toContain("Upgrade: websocket");
    expect(args).toContain("Sec-WebSocket-Protocol: mqtt");
    expect(args.find((a) => a.startsWith("Sec-WebSocket-Key: "))).toMatch(/^Sec-WebSocket-Key: [A-Za-z0-9+/]{22}==$/);
    expect(args.at(-1)).toBe(`https://${HOSTS.gateway}/mqtt`);
    // The bypass paths are exactly the chart's cookie-free gateway paths, and nothing on ota.
    expect(BYPASS_PATHS).toEqual({ gateway: ["/certs/renew", "/mqtt"], ota: [] });
    expect(all.filter((p) => p.expect === "reached").map((p) => `${p.host} ${p.path}`).sort()).toEqual(["gateway /certs/renew", "gateway /mqtt"]);
    // App paths on the other host names, and their roots, are stopped by Access, from a non-browser client.
    const stopped = all.filter((p) => p.expect === "access" && p.host !== "app");
    expect(stopped.map((p) => `${p.host} ${p.path}`).sort()).toEqual(["gateway /", "gateway /auth/login", "gateway /healthz", "ota /", "ota /auth/login", "ota /healthz"]);
    expect(stopped.every((p) => p.args.includes("User-Agent:"))).toBe(true);
    expect(all.filter((p) => p.host === "app").map((p) => p.expect)).toEqual(["access"]);
    // Below a bypass path, the connector must refuse.
    expect(all.filter((p) => p.expect === "refused").map((p) => `${p.host} ${p.path}`)).toEqual(["gateway /mqtt/x"]);
    // No bypass path is an app path or a prefix of one.
    for (const bypass of [...BYPASS_PATHS.gateway, ...BYPASS_PATHS.ota]) for (const app of APP_ONLY_PATHS) expect(app === bypass || app.startsWith(`${bypass}/`)).toBe(false);
  });

  test("the chart's cookie-free paths are the ones the probes treat as bypass or app paths", () => {
    const values = readFileSync(join(ROOT, "deploy/chart/values.yaml"), "utf8");
    const table = readFileSync(join(ROOT, "deploy/chart/templates/_ingress.tpl"), "utf8");
    const path = (key: string) => new RegExp(`^ {4}${key}: (\\S+)$`, "m").exec(values)?.[1];
    const host = (key: string) => new RegExp(`"key" "${key}" "host" "(\\w+)"`).exec(table)?.[1];
    expect(["certRenewal", "mqtt"].map((k) => [host(k), path(k)])).toEqual([
      ["gateway", "/certs/renew"],
      ["gateway", "/mqtt"],
    ]);
    expect(["login", "healthCheck"].map((k) => [host(k), path(k)])).toEqual([
      ["app", "/auth/login"],
      ["app", "/healthz"],
    ]);
    // The table has these four keys and no other: an ota entry would need a bypass here.
    expect([...table.matchAll(/\(dict "key" "(\w+)" "host" "(\w+)"/g)].map((m) => `${m[1]} ${m[2]}`)).toEqual(["login app", "healthCheck app", "certRenewal gateway", "mqtt gateway"]);
  });

  test("check records the outcomes per stage, and prints no host name, team domain or home directory, also from curl's errors", async () => {
    const home = process.env.HOME ?? "/home/someone";
    const answer = (stage: 1 | 2) => (args: string[]): RunResult => {
      const url = args.at(-1) ?? "";
      const host = Object.values(HOSTS).find((h) => url.includes(h)) ?? "";
      const path = url.replace(/^https:\/\/[^/]+/, "");
      if (host === HOSTS.ota && path === "/healthz") return { stdout: "", stderr: `curl: (77) error setting certificate file: ${home}/.config/ca.pem\ncurl: (6) Could not resolve host: ${HOSTS.ota}; also /home/other/x and /root/y`, exitCode: 6 };
      if (host === HOSTS.gateway && path === "/mqtt") return ok(stage === 1 ? "HTTP/1.1 418 I'm a teapot\r\nCF-RAY: 1-TPE\r\n\r\n" : "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\n\r\n");
      if (host === HOSTS.gateway && path === "/certs/renew") return ok(stage === 1 ? "HTTP/1.1 418 I'm a teapot\r\n\r\n" : "HTTP/1.1 400 Bad Request\r\nServer: istio-envoy\r\n\r\n");
      // The connector's refusal: the status only, Cloudflare's edge adds server and cf-ray.
      if (host === HOSTS.gateway && path === "/mqtt/x") return ok("HTTP/1.1 403 Forbidden\r\nServer: cloudflare\r\nContent-Length: 0\r\nCF-RAY: 2-TPE\r\n\r\n");
      // An app path that got through on the gateway host name: the failure the check exists for.
      if (host === HOSTS.gateway && path === "/auth/login") return ok(`HTTP/1.1 200 OK\r\nServer: istio-envoy\r\n\r\n<a href="https://${HOSTS["app"]}/">`);
      return ok(`HTTP/1.1 302 Found\r\nLocation: https://team-x.cloudflareaccess.com/cdn-cgi/access/login/${host}?kid=1\r\nCF-RAY: 3-TPE\r\n\r\n`);
    };
    for (const stage of [1, 2] as const) {
      const lines: string[] = [];
      const { results, failed } = await runProbes({ workstation: async (a) => answer(stage)(a), demoMachine: async (a) => answer(stage)(a), note: (line) => void lines.push(line) }, HOSTS, stage);
      const everything = JSON.stringify(results) + lines.join("\n");
      for (const secret of [HOSTS["app"], HOSTS.gateway, HOSTS.ota, "team-x", home, "/home/other", "/root/"]) expect(everything).not.toContain(secret);
      expect(everything).toContain("<team>.cloudflareaccess.com");
      expect(everything).toContain("~/.config/ca.pem");
      expect(everything).toContain("Could not resolve host: <ota>");
      const by = (name: string) => results.filter((r) => String(r.probe).startsWith(name));
      expect(by("gateway, WebSocket upgrade to /mqtt").map((r) => r.pass)).toEqual([true, true]);
      expect(by("gateway, POST /certs/renew").map((r) => r.pass)).toEqual([true, true]);
      expect(by("gateway, app path /auth/login").map((r) => [r.outcome, r.pass])).toEqual([
        ["unexpected", false],
        ["unexpected", false],
      ]);
      expect(by("ota, app path /auth/login").map((r) => [r.outcome, r.pass, r["cf-ray"]])).toEqual([
        ["access", true, "3-TPE"],
        ["access", true, "3-TPE"],
      ]);
      expect(by("gateway, /mqtt/x").map((r) => [r.outcome, r.pass, r["cf-ray"]])).toEqual([
        ["refused", true, "2-TPE"],
        ["refused", true, "2-TPE"],
      ]);
      // /auth/login on the gateway host name and the unreachable ota /healthz fail, from both places.
      expect(failed).toBe(4);
    }
  });
});

describe("tunnel.ts: the token's path", () => {
  test("a step that carries the ciphertext is sent once, also when ssh drops", async () => {
    const commands: string[] = [];
    const box = fakeBox((command) => {
      commands.push(command);
      if (command.startsWith("mktemp")) return ok("/run/user/1000/trustiot-demo.x\n");
      if (command.startsWith("cat > ")) return { stdout: "", stderr: "Connection closed by remote host", exitCode: 255 };
      return ok();
    });
    const ciphertext = Buffer.alloc(512, 7).toString("base64");
    await expect(stage(box, `${ciphertext}\n`, `${RUN_DIR}/token.enc.b64`, "0600")).rejects.toThrow(/ssh dropped; not sent again/);
    expect(commands.filter((c) => c.startsWith("cat > "))).toHaveLength(1);
    // The temporary file is still removed.
    expect(commands.at(-1)).toStartWith("rm -f ");
  });

  test("the start command is sent once when ssh drops", async () => {
    const commands: string[] = [];
    const box = fakeBox((command) => {
      commands.push(command);
      if (command.includes("systemd-run")) return { stdout: "", stderr: "Connection closed by remote host", exitCode: 255 };
      if (command.startsWith(`systemctl show ${RUN_UNIT_PREFIX}`)) return ok("LoadState=loaded\n");
      return ok("");
    });
    await startRun(box, log, false, { installer: "a".repeat(64), runner: "b".repeat(64), ciphertext: "c".repeat(64) }, { restoreLink: true, restoreMarker: true });
    expect(commands.filter((c) => c.includes("systemd-run"))).toHaveLength(1);
  });

  test("token-shaped and ciphertext-shaped text in remote output never reaches the log or an error", async () => {
    const token = Buffer.from(JSON.stringify({ a: "0".repeat(32), t: TUNNEL, s: Buffer.from(crypto.getRandomValues(new Uint8Array(32))).toString("base64") })).toString("base64");
    const ciphertext = Buffer.from(crypto.getRandomValues(new Uint8Array(512))).toString("base64");
    expect(redactSecrets(`a ${token} b ${ciphertext} c ${"f".repeat(64)}`)).toBe(`a <redacted> b <redacted> c ${"f".repeat(64)}`);
    // A failed run whose output carries both.
    const box = fakeBox((command) => {
      if (command.includes("apply-9.post")) return ok("service_result=exit-code\nexit_code=exited\nexit_status=1\nlink=present\nmarker=present\n");
      if (command.includes("apply-9.log")) return ok(`error: bad token ${token}\nstdin was ${ciphertext}\n`);
      if (command.includes("apply-9.exit")) return ok("1\n");
      throw new Error(`unexpected command ${command}`);
    });
    const outcome = await waitForRun(box, "apply-9", log, { now: () => 0, sleep: async () => {} });
    const calls: string[] = [];
    const error = await applyInstall({
      lock: async () => async () => {},
      plan: async () => ({}) as InstallPlan,
      confirm: () => {},
      backupZone: async () => {},
      record: async () => {},
      run: async () => outcome,
      cleanup: async () => void calls.push("cleanup"),
      note: () => {},
    }).catch((e: unknown) => String(e));
    const logged = readFileSync(log.file, "utf8");
    for (const text of [logged, String(error)]) {
      expect(text).not.toContain(token.slice(0, 24));
      expect(text).not.toContain(ciphertext.slice(0, 24));
    }
    expect(logged).toContain("  | error: bad token <redacted>");
    expect(String(error)).toContain("<redacted>");
    // A failing step on the ciphertext's path does not quote its output either.
    const failing = fakeBox(() => ({ stdout: "", stderr: `install: cannot write ${ciphertext}`, exitCode: 1 }));
    const quiet = await quietSh(failing, "sudo install x y", "installing x").catch((e: unknown) => String(e));
    expect(quiet).toContain("installing x failed (exit 1): install: cannot write <redacted>");
    expect(quiet).not.toContain(ciphertext.slice(0, 24));
  });
});

describe("tunnel.ts: remove and the trusted zone", () => {
  const info = (sources: string, rules: string[] = [], annotation = " (active)") =>
    [
      `trusted${annotation}`,
      "  target: ACCEPT",
      "  icmp-block-inversion: no",
      "  interfaces: ",
      `  sources: ${sources}`,
      "  services: ",
      "  ports: ",
      "  forward: yes",
      "  masquerade: no",
      "  rich rules: ",
      ...rules.map((rule) => `\t${rule}`),
      "",
    ].join("\n");
  const before = info("10.42.0.0/16 10.43.0.0/16");
  const after = info("10.42.0.0/16", [POD_SSH_RULE]);

  test("reads firewalld's view of the zone", () => {
    expect(parseZoneInfo(after)).toMatchObject({ zone: ["trusted"], sources: ["10.42.0.0/16"], "rich rules": [POD_SSH_RULE], interfaces: [] });
    expect(zoneItems(parseZoneInfo(before))).toEqual({ rule: false, serviceNetwork: true });
    expect(zoneItems(parseZoneInfo(after))).toEqual({ rule: true, serviceNetwork: false });
    // Only the installer's exact rule counts.
    expect(zoneItems(parseZoneInfo(info("10.42.0.0/16", [POD_SSH_RULE.replace("reject", "accept")]))).rule).toBe(false);
    // The permanent view has no "(active)".
    expect(parseZoneInfo(info("10.42.0.0/16", [], "")).zone).toEqual(["trusted"]);
  });

  test("reads rich rules indented with spaces as well as tabs, and fails on output it cannot read", () => {
    const spaced = info("10.42.0.0/16", [POD_SSH_RULE]).replace(`\t${POD_SSH_RULE}`, `    ${POD_SSH_RULE}`);
    expect(spaced).not.toContain("\t");
    expect(parseZoneInfo(spaced)["rich rules"]).toEqual([POD_SSH_RULE]);
    expect(zoneItems(parseZoneInfo(spaced))).toEqual({ rule: true, serviceNetwork: false });
    // A space-indented rule left behind keeps the backup and asks for the restore.
    const backup: ZoneBackup = { fileExisted: true, runtime: before, permanent: before };
    expect(backupDecision(backup, { runtime: spaced, permanent: before })).toEqual({ delete: false, differences: ["runtime rich rules", "runtime sources"] });
    expect(zoneRestore(backup, { runtime: spaced, permanent: spaced }).runtime.revert).toEqual({ removeRule: true, addServiceNetwork: true });
    // Two rules, one per line, any indentation.
    const other = 'rule family="ipv4" source address="10.42.0.0/16" port port="80" protocol="tcp" reject';
    expect(parseZoneInfo(`${info("10.42.0.0/16")}  ${POD_SSH_RULE}\n\t ${other}\n`)["rich rules"]).toEqual([other, POD_SSH_RULE].sort());
    // What it cannot read stops it, so remove deletes nothing.
    expect(() => parseZoneInfo("")).toThrow(/no zone name/);
    expect(() => parseZoneInfo(info("10.42.0.0/16").replace("  rich rules: \n", ""))).toThrow(/"rich rules:" line/);
    expect(() => parseZoneInfo(info("10.42.0.0/16").replace("  sources: 10.42.0.0/16\n", ""))).toThrow(/"sources:" line/);
    expect(() => parseZoneInfo(`${info("10.42.0.0/16")}\tsomething else\n`)).toThrow(/cannot read this line/);
    expect(() => parseZoneInfo(info("10.42.0.0/16").replace("  rich rules: ", `  rich rules: ${POD_SSH_RULE}`))).toThrow(/cannot read this line/);
    expect(() => parseZoneInfo(`trusted\n\t${POD_SSH_RULE}\n`)).toThrow(/cannot read this line/);
    // Forward ports also list one entry per line; they are kept as written.
    const forwarded = info("10.42.0.0/16").replace("  masquerade: no\n", "  masquerade: no\n  forward-ports: \n\tport=80:proto=tcp:toport=8080:toaddr=\n");
    expect(parseZoneInfo(forwarded)["forward-ports"]).toEqual(["port=80:proto=tcp:toport=8080:toaddr="]);
    expect(zoneDiff(parseZoneInfo(forwarded), parseZoneInfo(info("10.42.0.0/16")))).toEqual(["forward-ports"]);
    expect(() => parseZoneInfo(`${info("10.42.0.0/16")}${info("10.42.0.0/16")}`)).toThrow(/second zone/);
  });

  test("undoes exactly the installer's two items where the zone differs from the backup in them, per scope", () => {
    const backup: ZoneBackup = { fileExisted: true, runtime: before, permanent: before };
    expect(zoneRestore(backup, { runtime: after, permanent: after })).toEqual({
      runtime: { revert: { removeRule: true, addServiceNetwork: true }, want: { rule: false, serviceNetwork: true } },
      permanent: { revert: { removeRule: true, addServiceNetwork: true }, want: { rule: false, serviceNetwork: true } },
    });
    // Runtime already back (a reload, say), permanent not.
    expect(zoneRestore(backup, { runtime: before, permanent: after }).runtime.revert).toEqual({ removeRule: false, addServiceNetwork: false });
    // The rule was there before the install: it stays.
    const hadRule: ZoneBackup = { fileExisted: true, runtime: after, permanent: after };
    expect(zoneRestore(hadRule, { runtime: after, permanent: after }).permanent.revert).toEqual({ removeRule: false, addServiceNetwork: false });
    expect(removePlan("LoadState=loaded\n", null, { runtime: after, permanent: after })).toEqual({
      unit: `stop, disable and delete ${DEMO_TUNNEL_UNIT}.service, its token, its config and ${RUN_DIR}`,
      trustedZone: "no backup: left as it is",
    });
  });

  test("the backup goes only when the whole zone equals it again, in both views", () => {
    const backup: ZoneBackup = { fileExisted: false, runtime: before, permanent: before };
    expect(backupDecision(backup, { runtime: before, permanent: info("10.43.0.0/16 10.42.0.0/16", [], "") })).toEqual({ delete: true, differences: [] });
    expect(backupDecision(backup, { runtime: before, permanent: info("10.42.0.0/16 10.43.0.0/16 192.0.2.0/24") })).toEqual({ delete: false, differences: ["permanent sources"] });
    expect(backupDecision(backup, { runtime: after, permanent: before })).toEqual({ delete: false, differences: ["runtime rich rules", "runtime sources"] });
    expect(backupDecision(backup, { runtime: before.replace("masquerade: no", "masquerade: yes"), permanent: before }).differences).toEqual(["runtime masquerade"]);
  });

  test("the backup is taken once, outside firewalld's directory, also when the zone file did not exist", () => {
    const script = zoneBackupScript();
    expect(script).toContain(`d=${ZONE_BACKUP}`);
    expect(script).toContain("z=/etc/firewalld/zones/trusted.xml");
    expect(ZONE_BACKUP.startsWith("/etc/firewalld")).toBe(false);
    const bin = join(dir, "bin");
    mkdirSync(bin);
    strictFirewallCmd(bin, `case "$*" in *--permanent*) echo permanent-view ;; *) echo runtime-view ;; esac`);
    const run = (root: string, zoneFile: string) => Bun.spawnSync(["sh", "-c", zoneBackupScript(root, zoneFile)], { env: { PATH: `${bin}:/usr/bin:/bin` } });
    // The zone file exists: copied.
    const zoneFile = join(dir, "trusted.xml");
    writeFileSync(zoneFile, "<zone/>\n");
    const withFile = join(dir, "backup-1");
    expect(run(withFile, zoneFile).stdout.toString().trim()).toBe("saved");
    expect(readFileSync(join(withFile, "trusted.xml"), "utf8")).toBe("<zone/>\n");
    expect(existsSync(join(withFile, "trusted.xml.absent"))).toBe(false);
    expect(readFileSync(join(withFile, "runtime.txt"), "utf8")).toBe("runtime-view\n");
    expect(readFileSync(join(withFile, "permanent.txt"), "utf8")).toBe("permanent-view\n");
    expect(existsSync(join(withFile, "complete"))).toBe(true);
    // A complete backup is never overwritten.
    writeFileSync(join(withFile, "runtime.txt"), "from the first install\n");
    expect(run(withFile, zoneFile).stdout.toString().trim()).toBe("kept");
    expect(readFileSync(join(withFile, "runtime.txt"), "utf8")).toBe("from the first install\n");
    // No zone file before the install (firewalld's defaults): noted, and the views are still saved.
    const without = join(dir, "backup-2");
    const missing = run(without, join(dir, "no-such-zone.xml"));
    expect(missing.exitCode).toBe(0);
    expect(existsSync(join(without, "trusted.xml.absent"))).toBe(true);
    expect(existsSync(join(without, "trusted.xml"))).toBe(false);
    expect(existsSync(join(without, "complete"))).toBe(true);
  });

  test("every firewall-cmd line the scripts build passes firewall-cmd's usage rules", () => {
    const bin = join(dir, "strict-bin");
    mkdirSync(bin);
    strictFirewallCmd(bin, `case "$*" in *--permanent*) echo permanent-view ;; *) echo runtime-view ;; esac`);
    const sh = (script: string) => Bun.spawnSync(["sh", "-c", script], { env: { PATH: `${bin}:/usr/bin:/bin` } });
    // The stand-in refuses the combined form exactly as firewall-cmd 2.5.2 does.
    const combined = sh("firewall-cmd --zone=trusted --info-zone=trusted");
    expect(combined.exitCode).toBe(2);
    expect(combined.stderr.toString()).toContain(FIREWALL_WRONG_USAGE);
    // remove's two views, and the host records' script (its sha256sum finds no zone files here).
    for (const [script, out] of [
      [TRUSTED_ZONE_VIEWS.runtime, "runtime-view\n"],
      [TRUSTED_ZONE_VIEWS.permanent, "permanent-view\n"],
    ] as const) {
      const run = sh(`set -e; ${script}`);
      expect({ script, exit: run.exitCode, stderr: run.stderr.toString() }).toEqual({ script, exit: 0, stderr: "" });
      expect(run.stdout.toString()).toBe(out);
    }
    const record = sh(firewalldRecordScript());
    expect({ exit: record.exitCode, stderr: record.stderr.toString(), stdout: record.stdout.toString() }).toEqual({ exit: 0, stderr: "", stdout: "runtime-view\npermanent-view\n" });
    // The builder refuses what firewall-cmd refuses, and what would act on another zone.
    expect(() => firewallCmd("--zone=trusted", "--info-zone=trusted")).toThrow(/refuses --zone next to --info-zone=trusted/);
    expect(() => firewallCmd("--add-source=10.43.0.0/16")).toThrow(/needs --zone=trusted/);
    expect(() => firewallCmd("--zone=public", "--add-source=10.43.0.0/16")).toThrow(/do not use: --zone=public/);
    expect(() => firewallCmd("--zone=trusted", "--add-source=10.43.0.0/16", "--query-source=10.43.0.0/16")).toThrow(/exactly one action/);
    expect(() => firewallCmd("--reload")).toThrow(/do not use: --reload/);
    expect(firewallCmd("-q", "--zone=trusted", `--remove-rich-rule=${POD_SSH_RULE}`)).toBe(`firewall-cmd -q --zone=trusted '--remove-rich-rule=${POD_SSH_RULE}'`);
    // No firewall-cmd call in tunnel.ts bypasses the builder.
    const code = readFileSync(join(import.meta.dir, "tunnel.ts"), "utf8")
      .split("\n")
      .filter((line) => !/^\s*(\*|\/\/|\/\*)/.test(line));
    expect(code.filter((line) => /firewall-cmd\s+-/.test(line))).toEqual([]);
    expect(code.filter((line) => line.includes('"firewall-cmd"'))).toHaveLength(1);
  });

  /** Runs removeScript with stand-ins for systemctl and rm that only record their calls. */
  function runRemove(fake: { load: string; active: string; disableExit: number; reloadExit?: number; showExit?: number }): { exitCode: number; calls: string[]; stderr: string } {
    const bin = join(dir, `remove-${Math.random().toString(16).slice(2)}`);
    mkdirSync(bin);
    const calls = join(bin, "calls");
    writeFileSync(calls, "");
    const stub = (body: string) => `#!/bin/sh\necho "$(basename "$0") $*" >> "${calls}"\n${body}\n`;
    writeFileSync(
      join(bin, "systemctl"),
      stub(`case "$*" in
  *"-p LoadState"*) echo "$FAKE_LOAD" ;;
  *"-p ActiveState"*) echo "$FAKE_ACTIVE"; exit "$FAKE_SHOW_EXIT" ;;
  disable*) exit "$FAKE_DISABLE_EXIT" ;;
  daemon-reload*) exit "$FAKE_RELOAD_EXIT" ;;
esac
exit 0`),
      { mode: 0o755 },
    );
    writeFileSync(join(bin, "rm"), stub("exit 0"), { mode: 0o755 });
    const run = Bun.spawnSync(["sh", "-c", removeScript()], { env: { PATH: `${bin}:/usr/bin:/bin`, FAKE_LOAD: fake.load, FAKE_ACTIVE: fake.active, FAKE_DISABLE_EXIT: String(fake.disableExit), FAKE_RELOAD_EXIT: String(fake.reloadExit ?? 0), FAKE_SHOW_EXIT: String(fake.showExit ?? 0) } });
    return { exitCode: run.exitCode, calls: readFileSync(calls, "utf8").trim().split("\n"), stderr: run.stderr.toString() };
  }

  test("a connector that does not stop keeps its token and files", () => {
    const failed = runRemove({ load: "loaded", active: "active", disableExit: 1 });
    expect(failed.exitCode).toBe(1);
    expect(failed.stderr).toContain("could not stop cloudflared-demo.service; nothing deleted");
    expect(failed.calls.some((c) => c.startsWith("rm ") || c.includes("daemon-reload"))).toBe(false);
    const stillUp = runRemove({ load: "loaded", active: "deactivating", disableExit: 0 });
    expect(stillUp.exitCode).toBe(1);
    expect(stillUp.stderr).toContain("still deactivating; nothing deleted");
    expect(stillUp.calls.some((c) => c.startsWith("rm "))).toBe(false);
    // A state that cannot be read is not "stopped".
    for (const unread of [runRemove({ load: "loaded", active: "", disableExit: 0 }), runRemove({ load: "loaded", active: "inactive", disableExit: 0, showExit: 1 })]) {
      expect(unread.exitCode).toBe(1);
      expect(unread.calls.some((c) => c.startsWith("rm "))).toBe(false);
    }
  });

  test("a failed daemon-reload fails remove", () => {
    const reload = runRemove({ load: "loaded", active: "inactive", disableExit: 0, reloadExit: 1 });
    expect(reload.exitCode).toBe(1);
    expect(reload.stderr).toContain("systemctl daemon-reload failed: systemd may still have cloudflared-demo.service loaded");
    expect(reload.calls.at(-1)).toBe("systemctl daemon-reload");
  });

  test("a stopped or absent connector is removed: unit, token, config and the tmpfs hand-off directory", () => {
    const stopped = runRemove({ load: "loaded", active: "inactive", disableExit: 0 });
    expect(stopped.exitCode).toBe(0);
    expect(stopped.calls).toContain("systemctl disable --now cloudflared-demo.service");
    expect(stopped.calls.slice(-3)).toEqual([`rm -f ${CREDENTIAL} ${CONFIG} /etc/systemd/system/cloudflared-demo.service`, "systemctl daemon-reload", `rm -rf ${RUN_DIR}`]);
    const absent = runRemove({ load: "not-found", active: "inactive", disableExit: 1 });
    expect(absent.exitCode).toBe(0);
    expect(absent.calls.some((c) => c.startsWith("systemctl disable"))).toBe(false);
  });

  /** Runs restoreTrustedZoneScript against a stand-in firewall-cmd; FAKE_* are its exit codes. */
  function runRestore(plan: Parameters<typeof restoreTrustedZoneScript>[0], fake: Record<string, string>): { exitCode: number; calls: string[]; stderr: string } {
    const bin = join(dir, `restore-${Math.random().toString(16).slice(2)}`);
    mkdirSync(bin);
    const calls = join(bin, "calls");
    writeFileSync(calls, "");
    strictFirewallCmd(
      bin,
      `case "$*" in
  *--permanent*--query-rich-rule*) exit "\${FAKE_PERM_RULE:-$FAKE_RULE}" ;;
  *--query-rich-rule*) exit "$FAKE_RULE" ;;
  *--query-source*) exit "$FAKE_NET" ;;
  *) exit "$FAKE_CHANGE" ;;
esac`,
    );
    const run = Bun.spawnSync(["sh", "-c", restoreTrustedZoneScript(plan)], { env: { PATH: `${bin}:/usr/bin:/bin`, FAKE_CHANGE: "0", FAKE_CALLS: calls, ...fake } });
    return { exitCode: run.exitCode, calls: readFileSync(calls, "utf8").trim().split("\n"), stderr: run.stderr.toString() };
  }

  test("the trusted zone is restored in both scopes and checked before the backup may go", () => {
    const plan = zoneRestore({ fileExisted: true, runtime: before, permanent: before }, { runtime: after, permanent: after });
    // Rule absent (exit 1) and network present (exit 0) in both scopes: as the backup had it.
    const done = runRestore(plan, { FAKE_RULE: "1", FAKE_NET: "0" });
    expect(done.exitCode).toBe(0);
    expect(done.calls.slice(0, 4)).toEqual([
      `-q --zone=trusted --remove-rich-rule=${POD_SSH_RULE}`,
      "-q --zone=trusted --add-source=10.43.0.0/16",
      `-q --zone=trusted --permanent --remove-rich-rule=${POD_SSH_RULE}`,
      "-q --zone=trusted --permanent --add-source=10.43.0.0/16",
    ]);
    const changeFails = runRestore(plan, { FAKE_RULE: "1", FAKE_NET: "0", FAKE_CHANGE: "13" });
    expect(changeFails.exitCode).not.toBe(0);
    expect(changeFails.calls).toHaveLength(1);
    const permanentLeft = runRestore(plan, { FAKE_RULE: "1", FAKE_NET: "0", FAKE_PERM_RULE: "0" });
    expect(permanentLeft.exitCode).toBe(1);
    expect(permanentLeft.stderr).toContain("the trusted zone (permanent) is not as in the backup: pod-SSH rule present (want absent)");
    const queryFails = runRestore(plan, { FAKE_RULE: "2", FAKE_NET: "0" });
    expect(queryFails.exitCode).not.toBe(0);
    expect(queryFails.stderr).toContain("failed (exit 2)");
    // Nothing to change: only the checks run, against the backup's state.
    const nothing = runRestore(zoneRestore({ fileExisted: true, runtime: after, permanent: after }, { runtime: after, permanent: after }), { FAKE_RULE: "0", FAKE_NET: "1" });
    expect(nothing.exitCode).toBe(0);
    expect(nothing.calls.every((c) => c.includes("--query-"))).toBe(true);
  });
});

describe("tunnel.ts: the ops tunnel is never changed", () => {
  test("no command in the script or in what it runs on the demo machine stops, restarts, reloads, kills, disables or masks cloudflared.service", () => {
    const source = readFileSync(join(import.meta.dir, "tunnel.ts"), "utf8");
    const generated = [
      runnerScript(),
      removeScript(),
      auditScript(),
      zoneBackupScript(),
      startCommand("apply-x", { installer: "a".repeat(64), runner: "b".repeat(64), ciphertext: "c".repeat(64) }, installerArgs(false), { restoreLink: true, restoreMarker: true }),
    ];
    const mutating = /systemctl\s+(?:-\S+\s+)*(restart|stop|reload|kill|disable|mask|try-restart|reload-or-restart|try-reload-or-restart|enable|start|isolate|set-property|revert|edit|reenable|preset)\b([^\n;|&`]*)/g;
    for (const text of [source, ...generated]) {
      for (const match of text.matchAll(mutating)) {
        const target = match[2] ?? "";
        expect(`${match[0]}`).toMatch(/cloudflared-demo|\$\{unit\}|trustiot-demo-install-|\$\{RUN_UNIT_PREFIX\}/);
        expect(target).not.toMatch(/OPS_TUNNEL_UNIT|\bcloudflared\.service|\bcloudflared\b(?!-)/);
      }
      expect(text).not.toMatch(/\b(pkill|killall)\b|\bkill\s+-/);
    }
    // The ops unit is named only to read it (systemctl show) or in messages.
    for (const line of source.split("\n").filter((l) => l.includes("${OPS_TUNNEL_UNIT}") && /systemctl/.test(l))) {
      expect(line).toMatch(/systemctl show \$\{OPS_TUNNEL_UNIT\}/);
    }
    expect(OPS_TUNNEL_UNIT).toBe("cloudflared.service");
  });
});
