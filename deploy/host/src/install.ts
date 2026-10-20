// Converges a SaaS host to the state in desired.ts. Every step first reads the host and plans
// only the changes still needed, so a second run on the same host changes nothing, and
// --dry-run prints the plan without applying it. A file the installer did not write (no
// MARKER line) is never overwritten: the step stops with an explanation instead.
import { createHash } from "node:crypto";
import {
  CLOSED_PORTS,
  CLOSED_SERVICES,
  CLOUDFLARE_KEY,
  CLOUDFLARE_KEY_ID,
  CLOUDFLARE_KEY_PATH,
  CLOUDFLARED_BINARY,
  CLOUDFLARED_REPO,
  CLOUDFLARED_REPO_BASEURL,
  CLOUDFLARED_REPO_ID,
  CLOUDFLARE_REPO_PATTERN,
  REPO_DIRECTORY,
  CLOUDFLARED_REPO_PATH,
  CREDSTORE,
  FIREWALLD_CONFIG,
  TRUSTED_ZONE_RICH_RULES,
  FORBIDDEN_LISTEN_PORTS,
  K3S_BINARY,
  K3S_CONFIG,
  K3S_CONFIG_PATH,
  K3S_INSTALLER_SHA256,
  K3S_INSTALLER_URL,
  K3S_POD_NETWORK,
  K3S_SERVICE_NETWORK,
  K3S_VERSION,
  MARKER,
  MAX_EXPOSURE,
  checkUnitName,
  configPath,
  credentialPath,
  renderConfig,
  renderUnit,
  unitPath,
} from "./desired.ts";
import type { RunOptions, RunResult, System } from "./system.ts";
import { parseTunnelToken } from "./token.ts";

export interface InstallOptions {
  /** systemd unit name of cloudflared, without `.service`. */
  unit: string;
  /** Loopback port of cloudflared's metrics server; null lets cloudflared pick one of 20241-20245. */
  metricsPort: number | null;
  /** A new tunnel token (from stdin or a file); null keeps the token already on the host. */
  token: Uint8Array | null;
  /** Leave k3s alone, for hosts where it is managed separately. */
  skipK3s: boolean;
  /** Replace a k3s config that differs from the desired one (backed up) and restart k3s. */
  reconfigureK3s: boolean;
  /** Replace a cloudflared installed from anywhere but the pinned repository with Cloudflare's signed package. */
  reinstallCloudflared: boolean;
  dryRun: boolean;
}

export interface Logger {
  info(message: string): void;
  warn(message: string): void;
  /** A change applied to the host, or planned under --dry-run. */
  change(message: string): void;
}

/** A failure the operator has to resolve; the message says how. */
export class InstallError extends Error {}

export interface InstallResult {
  /** Changes applied, or planned under --dry-run. */
  changes: string[];
  /** The unit's `systemd-analyze security` exposure, when it was measured. */
  exposure: number | null;
}

interface Action {
  description: string;
  apply(): Promise<void>;
}

interface Context {
  sys: System;
  options: InstallOptions;
  log: Logger;
  credentialChanged: boolean;
  configChanged: boolean;
  unitChanged: boolean;
  k3sInstallerSha256: string;
}

interface Step {
  name: string;
  plan(ctx: Context): Promise<Action[]>;
}

const STEPS: Step[] = [
  { name: "preflight", plan: preflight },
  { name: "SELinux", plan: selinux },
  { name: "firewalld package", plan: firewalldPackage },
  { name: "firewalld", plan: firewalldConfig },
  { name: "k3s", plan: k3s },
  { name: "cloudflared package", plan: cloudflaredPackage },
  { name: "tunnel token", plan: tunnelCredential },
  { name: "cloudflared unit", plan: cloudflaredUnit },
];

export async function install(
  sys: System,
  options: InstallOptions,
  log: Logger,
  // Tests replace the pinned installer digest; nothing else does.
  pins: { k3sInstallerSha256?: string } = {},
): Promise<InstallResult> {
  checkUnitName(options.unit);
  if (options.skipK3s && options.reconfigureK3s) {
    throw new InstallError("--skip-k3s and --reconfigure-k3s exclude each other");
  }
  const ctx: Context = {
    sys,
    options,
    log,
    credentialChanged: false,
    configChanged: false,
    unitChanged: false,
    k3sInstallerSha256: pins.k3sInstallerSha256 ?? K3S_INSTALLER_SHA256,
  };
  const changes: string[] = [];
  for (const step of STEPS) {
    const actions = await step.plan(ctx);
    for (const action of actions) {
      changes.push(action.description);
      if (options.dryRun) {
        log.change(`would ${action.description}`);
      } else {
        log.change(action.description);
        await action.apply();
      }
    }
  }
  const exposure = await verify(ctx);
  return { changes, exposure };
}

// ---------------------------------------------------------------------------------------------
// Steps

async function preflight(ctx: Context): Promise<Action[]> {
  const { sys } = ctx;
  if (sys.uid() !== 0) throw new InstallError("run the installer as root (sudo)");
  if (!["x64", "arm64"].includes(sys.arch())) {
    throw new InstallError(`unsupported CPU architecture ${sys.arch()}; x86_64 and aarch64 are supported`);
  }
  const osRelease = parseOsRelease(await readText(sys, "/etc/os-release"));
  const family = [osRelease.ID ?? "", ...(osRelease.ID_LIKE ?? "").split(/\s+/)];
  if (!family.some((id) => ["fedora", "rhel", "centos"].includes(id))) {
    throw new InstallError(
      `unsupported OS ${osRelease.PRETTY_NAME ?? osRelease.ID ?? "(unknown)"}; the installer supports Fedora and RHEL-compatible hosts with dnf`,
    );
  }
  if ((await sys.stat("/usr/bin/dnf")) === null) throw new InstallError("dnf is missing at /usr/bin/dnf");
  const systemd = /systemd (\d+)/.exec((await mustRun(sys, ["systemctl", "--version"])).stdout);
  const systemdVersion = Number(systemd?.[1] ?? 0);
  if (systemdVersion < 252) {
    throw new InstallError(`systemd ${systemdVersion} is too old for the cloudflared sandbox; 252 (RHEL 9) or newer is needed`);
  }
  ctx.log.info(`host: ${osRelease.PRETTY_NAME ?? osRelease.ID}, ${sys.arch()}, systemd ${systemdVersion}`);
  // Every refusal a later step would make is checked here too, before anything changes, so a
  // refused run leaves the host as it was.
  await checkUnitFileOwned(ctx);
  if (!ctx.options.skipK3s) {
    checkK3sConfig(ctx, await readText(sys, K3S_CONFIG_PATH));
    await checkK3sSelinux(sys);
  }
  // The firewall trusts k3s's networks, so an existing cluster must use them; a new one is
  // checked once it is Ready.
  const k3sInstalled = (await sys.stat(K3S_BINARY)) !== null;
  const k3sRunning = (await sys.run(["systemctl", "is-active", "k3s"])).stdout.trim() === "active";
  if (ctx.options.skipK3s && !(k3sInstalled && k3sRunning)) {
    throw new InstallError("--skip-k3s is for hosts where k3s is installed and running; it is not");
  }
  if (k3sInstalled && k3sRunning) await checkK3sNetworks(sys);
  await checkKeyFile(sys);
  const managed = [CLOUDFLARE_KEY_PATH, CLOUDFLARED_REPO_PATH, unitPath(ctx.options.unit), credentialPath(ctx.options.unit), configPath(ctx.options.unit)];
  if (!ctx.options.skipK3s) managed.push(K3S_CONFIG_PATH);
  for (const path of managed) {
    const info = await sys.stat(path);
    if (info !== null && info.type !== "file") {
      throw new InstallError(`${path} is a ${info.type}, not a regular file; remove it and run again`);
    }
  }
  const store = await sys.stat(CREDSTORE);
  if (store !== null && store.type !== "directory") throw new InstallError(`${CREDSTORE} must be a directory`);
  if ((await sys.run(["rpm", "-q", "--quiet", "firewalld"])).code === 0) {
    // Planning reads only; it is run here for its refusals (trusted zone, port ranges).
    const running = (await sys.run(["firewall-cmd", "--state"])).code === 0;
    await checkTrustedZone(sys, running);
    for (const scope of firewallScopes(running)) await planFirewallScope(sys, scope);
  }
  await checkRepoFiles(sys);
  if ((await sys.run(["rpm", "-q", "--quiet", "cloudflared"])).code === 0 && !ctx.options.reinstallCloudflared) {
    await checkCloudflaredProvenance(sys);
    await checkCloudflaredSupportsTokenFile(sys);
  }
  await checkTokenAvailable(ctx);
  return [];
}

async function checkUnitFileOwned(ctx: Context): Promise<void> {
  const path = unitPath(ctx.options.unit);
  const current = await readText(ctx.sys, path);
  if (current !== null && !current.startsWith(MARKER)) {
    throw new InstallError(
      `${path} exists and was not written by this installer (for example by \`cloudflared service install\`). ` +
        "It is left untouched; migrate it as deploy/host/README.md describes, or choose another name with --unit",
    );
  }
}

function checkK3sConfig(ctx: Context, current: string | null): void {
  if (current !== null && current !== K3S_CONFIG && !ctx.options.reconfigureK3s) {
    throw new InstallError(
      `${K3S_CONFIG_PATH} differs from the config this installer writes (Traefik and servicelb off). ` +
        "Run again with --reconfigure-k3s to replace it (the old file is kept as a backup and k3s restarts), " +
        "or with --skip-k3s to leave k3s alone",
    );
  }
}

/**
 * cloudflared must come only from the repository the installer writes, which trusts nothing
 * but the pinned signing key. Any other definition of Cloudflare's repository (for example the
 * file from Cloudflare's own instructions, which fetches its key at install time) is refused:
 * the operator removes it and the installer writes the pinned one.
 */
async function checkRepoFiles(sys: System): Promise<void> {
  const others: string[] = [];
  for (const name of await sys.listDirectory(REPO_DIRECTORY)) {
    if (!name.endsWith(".repo")) continue;
    const path = `${REPO_DIRECTORY}/${name}`;
    const text = (await readText(sys, path)) ?? "";
    const foreign = path === CLOUDFLARED_REPO_PATH ? !text.startsWith(MARKER) : CLOUDFLARE_REPO_PATTERN.test(text);
    if (foreign) others.push(path);
  }
  if (others.length > 0) {
    throw new InstallError(
      `${others.join(", ")}: not written by this installer but defining Cloudflare's package repository. ` +
        "Remove them and run again, so cloudflared is only installed and updated with the pinned signing key",
    );
  }
}

async function checkTokenAvailable(ctx: Context): Promise<void> {
  if (ctx.options.token !== null) {
    parseTunnelToken(ctx.options.token);
    return;
  }
  const path = credentialPath(ctx.options.unit);
  const existing = await ctx.sys.readFile(path);
  if (existing === null) {
    throw new InstallError(
      `there is no tunnel token at ${path} yet; pass it with --token-stdin or --token-file (never as an argument)`,
    );
  }
  parseTunnelToken(existing);
}

async function selinux(ctx: Context): Promise<Action[]> {
  const mode = (await mustRun(ctx.sys, ["getenforce"])).stdout.trim();
  if (mode !== "Enforcing") {
    throw new InstallError(
      `SELinux is ${mode}; the SaaS host runs SELinux in enforcing mode. Set SELINUX=enforcing in /etc/selinux/config, relabel and reboot, then run the installer again`,
    );
  }
  return [];
}

async function firewalldPackage(ctx: Context): Promise<Action[]> {
  const { sys } = ctx;
  if ((await sys.run(["rpm", "-q", "--quiet", "firewalld"])).code === 0) return [];
  return [
    {
      description: "install firewalld (dnf)",
      apply: async () => void (await mustRun(sys, ["dnf", "install", "-y", "firewalld"], { inheritOutput: true })),
    },
  ];
}

interface FirewallScope {
  /** The command and options that address this configuration. */
  cmd: string[];
  label: string;
}

async function firewalldConfig(ctx: Context): Promise<Action[]> {
  const { sys } = ctx;
  if ((await sys.run(["rpm", "-q", "--quiet", "firewalld"])).code !== 0) {
    if (!ctx.options.dryRun) throw new InstallError("firewalld is not installed");
    // Only under --dry-run: firewalld is not installed yet, so its zones cannot be read.
    return [
      {
        description: `trust the pod network ${K3S_POD_NETWORK} except for SSH, close web ports in every zone, then start firewalld`,
        apply: async () => {},
      },
    ];
  }
  // A running firewalld gets its runtime and permanent configuration changed separately, so no
  // reload is needed and it never drops the connections it tracks. A stopped one is configured
  // offline and only started afterwards, so it never runs with a configuration this step would
  // refuse or change.
  const running = (await sys.run(["firewall-cmd", "--state"])).code === 0;
  await checkTrustedZone(sys, running);
  const actions: Action[] = [];
  for (const scope of firewallScopes(running)) actions.push(...(await planFirewallScope(sys, scope)));
  const enabled = (await sys.run(["systemctl", "is-enabled", "firewalld"])).stdout.trim() === "enabled";
  if (!enabled || !running) {
    actions.push({
      description: "enable and start firewalld",
      apply: async () => void (await mustRun(sys, ["systemctl", "enable", "--now", "firewalld"])),
    });
  }
  return actions;
}

function firewallScopes(running: boolean): FirewallScope[] {
  return running
    ? [
        { cmd: ["firewall-cmd"], label: "runtime" },
        { cmd: ["firewall-cmd", "--permanent"], label: "permanent" },
      ]
    : [{ cmd: ["firewall-offline-cmd"], label: "permanent" }];
}

async function planFirewallScope(sys: System, { cmd, label }: FirewallScope): Promise<Action[]> {
  const actions: Action[] = [];
  const change = (description: string, args: string[]): Action => ({
    description: `firewalld (${label}): ${description}`,
    apply: async () => void (await mustRun(sys, [...cmd, ...args])),
  });
  // The exceptions first, so the pods are never trusted without them.
  for (const rule of TRUSTED_ZONE_RICH_RULES) {
    if (!(await firewallQuery(sys, [...cmd, "--zone=trusted", `--query-rich-rule=${rule}`]))) {
      actions.push(change(`in the trusted zone, ${rule}`, ["--zone=trusted", `--add-rich-rule=${rule}`]));
    }
  }
  if (!(await firewallQuery(sys, [...cmd, "--zone=trusted", `--query-source=${K3S_POD_NETWORK}`]))) {
    actions.push(change(`add the pod network ${K3S_POD_NETWORK} to the trusted zone`, ["--zone=trusted", `--add-source=${K3S_POD_NETWORK}`]));
  }
  if (await firewallQuery(sys, [...cmd, "--zone=trusted", `--query-source=${K3S_SERVICE_NETWORK}`])) {
    actions.push(
      change(`remove the service network ${K3S_SERVICE_NETWORK} from the trusted zone (it is never a source)`, [
        "--zone=trusted",
        `--remove-source=${K3S_SERVICE_NETWORK}`,
      ]),
    );
  }
  // Every zone, not only the active ones: an interface or source can move to another zone
  // later (for example through NetworkManager), and the permanent configuration is what a
  // reboot or reload brings back.
  // Policies (firewalld 0.9 and later) can open ports between zones as well.
  const zones = words((await mustRun(sys, [...cmd, "--get-zones"])).stdout).filter((zone) => zone !== "trusted");
  const defaults = await defaultZones(sys, cmd[0] === "firewall-offline-cmd");
  const policies = await mustRun(sys, [...cmd, "--get-policies"]);
  const targets = [
    ...zones.map((zone) => ({ flag: `--zone=${zone}`, name: `zone ${zone}` })),
    ...words(policies.stdout).map((policy) => ({ flag: `--policy=${policy}`, name: `policy ${policy}` })),
  ];
  const servicePorts = new Map<string, string[]>();
  const portsOf = async (service: string): Promise<string[]> => {
    let ports = servicePorts.get(service);
    if (ports === undefined) {
      // `--service=<name> --get-ports` works only with --permanent; --info-service works everywhere.
      const info = (await mustRun(sys, [...cmd, `--info-service=${service}`])).stdout;
      ports = words(/^\s*ports:(.*)$/m.exec(info)?.[1] ?? "");
      servicePorts.set(service, ports);
    }
    return ports;
  };
  const refuse = (target: string, what: string): never => {
    throw new InstallError(
      `firewalld ${target} (${label}) ${what}, which can open TCP or UDP ${FORBIDDEN_LISTEN_PORTS.join(" or ")} on the host; ` +
        "the installer removes only the plain http/https services and 80/443 ports, so change this one yourself and run again",
    );
  };
  // Direct rules bypass zones entirely and cannot be audited here.
  for (const query of ["--get-all-rules", "--get-all-passthroughs"]) {
    const direct = (await mustRun(sys, [...cmd, "--direct", query])).stdout.trim();
    if (direct !== "") refuse("direct configuration", `has direct rules (${direct.split("\n")[0]})`);
  }
  for (const { flag, name } of targets) {
    // One --info-zone/--info-policy call per zone: firewall-offline-cmd needs seconds per call,
    // and a host has about twenty zones and policies, each audited before and during the change.
    const isZone = flag.startsWith("--zone=");
    const info = parseFirewallInfo((await mustRun(sys, [...cmd, flag.replace(/^--(zone|policy)=/, "--info-$1=")])).stdout);
    const field = (key: string): string[] => {
      const value = info.get(key);
      // A field that is missing means output this installer does not understand; never read
      // that as "nothing is open".
      if (value === undefined) throw new InstallError(`firewalld ${name} (${label}): its description has no "${key}" field`);
      return value;
    };
    // A zone with target ACCEPT accepts everything, like trusted, from the interfaces and
    // sources bound to it, and the default zone also takes every interface bound to no zone;
    // a policy with target ACCEPT into the host does the same for its ingress zones. Fedora
    // ships the zone nm-shared with target ACCEPT and nothing bound (NetworkManager binds it
    // only to connections it shares), so binding decides for the other zones.
    const target = field("target")[0] ?? "";
    const zoneName = flag.replace(/^--zone=/, "");
    const applies = isZone
      ? defaults.has(zoneName) || field("interfaces").length > 0 || field("sources").length > 0
      : field("egress-zones").some((zone) => zone === "HOST" || zone === "ANY");
    if (target === "ACCEPT" && applies) refuse(name, "has the target ACCEPT and applies to traffic into the host");
    for (const service of field("services")) {
      if (CLOSED_SERVICES.includes(service)) {
        actions.push(change(`remove service ${service} from ${name}`, [flag, `--remove-service=${service}`]));
      } else if ((await portsOf(service)).some(opensWebPort)) {
        refuse(name, `allows the service ${service}`);
      }
    }
    for (const port of field("ports")) {
      if (CLOSED_PORTS.includes(port)) {
        actions.push(change(`remove port ${port} from ${name}`, [flag, `--remove-port=${port}`]));
      } else if (opensWebPort(port)) {
        refuse(name, `opens the port range ${port}`);
      }
    }
    for (const forward of field("forward-ports")) {
      const port = /(?:^|:)port=([\d-]+):proto=(\w+)/.exec(forward);
      if (port !== null && opensWebPort(`${port[1]}/${port[2]}`)) refuse(name, `forwards ${forward}`);
    }
    for (const rule of field("rich rules")) {
      if (await richRuleOpensWebPort(rule, portsOf)) refuse(name, `has the rich rule '${rule}'`);
    }
  }
  return actions;
}

/**
 * The zones that take interfaces bound to no zone: the current default and the one in
 * firewalld.conf, which comes back after a reload or reboot (firewalld falls back to public
 * when the file names none). Both count in every scope, which errs on the strict side.
 */
async function defaultZones(sys: System, offline: boolean): Promise<Set<string>> {
  const current = (await mustRun(sys, [offline ? "firewall-offline-cmd" : "firewall-cmd", "--get-default-zone"])).stdout.trim();
  const configured = /^\s*DefaultZone\s*=\s*(\S+)\s*$/m.exec((await readText(sys, FIREWALLD_CONFIG)) ?? "")?.[1] ?? "public";
  return new Set([current, configured].filter((zone) => zone !== ""));
}

/** Fields of `--info-zone`/`--info-policy` whose entries are whole lines, not words. */
const LINE_FIELDS = new Set(["rich rules"]);

/**
 * Parses the output of `firewall-cmd --info-zone=<z>` or `--info-policy=<p>` (and the same
 * from firewall-offline-cmd). Each field is a line `  <key>: <values>`; the entries of the
 * multi-line fields (forward ports, rich rules) follow on lines indented with a tab:
 *
 *     work
 *       services: dhcpv6-client ssh
 *       forward-ports:
 *     \tport=8080:proto=tcp:toport=80:toaddr=
 *       rich rules:
 *     \trule family="ipv4" source address="1.2.3.4" accept
 */
export function parseFirewallInfo(text: string): Map<string, string[]> {
  const fields = new Map<string, string[]>();
  let key: string | null = null;
  const entries = (field: string, value: string): string[] =>
    LINE_FIELDS.has(field) ? (value.trim() === "" ? [] : [value.trim()]) : words(value);
  for (const line of text.split("\n")) {
    if (line.startsWith("\t")) {
      if (key === null) throw new InstallError(`unexpected line in firewalld's output: ${line.trim()}`);
      fields.get(key)?.push(...entries(key, line));
      continue;
    }
    const match = /^ +([a-z][a-z -]*?):(?: (.*))?$/.exec(line);
    key = match === null ? null : (match[1] ?? "");
    if (key !== null) fields.set(key, entries(key, match?.[2] ?? ""));
  }
  return fields;
}

/**
 * True when a firewalld rich rule can let traffic to TCP or UDP 80 or 443 in: a `forward-port`
 * element for those ports (whatever else the rule says), or a `port` or `service` element for
 * them whose action is not reject or drop. The action is read from the rule's words outside
 * quoted values, so a log prefix such as "drop" does not count.
 */
export async function richRuleOpensWebPort(rule: string, portsOf: (service: string) => Promise<string[]>): Promise<boolean> {
  const values = [...rule.matchAll(/\b(forward-port|port|service)\b((?:\s+[\w-]+="[^"]*")+)/g)].map((match) => ({
    element: match[1] ?? "",
    attributes: Object.fromEntries([...(match[2] ?? "").matchAll(/([\w-]+)="([^"]*)"/g)].map((pair) => [pair[1], pair[2]])),
  }));
  for (const { element, attributes } of values) {
    if (element === "forward-port" && opensWebPort(`${attributes.port}/${attributes.protocol}`)) return true;
  }
  const bare = rule.replace(/"[^"]*"/g, '""');
  const action = /\b(accept|reject|drop|mark)\b/.exec(bare.replace(/\blog\b.*?(?=\b(?:audit|accept|reject|drop|mark)\b|$)/, ""))?.[1];
  if (action === "reject" || action === "drop") return false;
  for (const { element, attributes } of values) {
    if (element === "port" && opensWebPort(`${attributes.port}/${attributes.protocol}`)) return true;
    if (element === "service") {
      const service = attributes.name ?? "";
      if (CLOSED_SERVICES.includes(service) || (await portsOf(service)).some(opensWebPort)) return true;
    }
  }
  return false;
}

/** True for a firewalld port entry (`80/tcp`, `1-1024/tcp`) that includes TCP or UDP 80 or 443. */
export function opensWebPort(entry: string): boolean {
  const match = /^(\d+)(?:-(\d+))?\/(tcp|udp|sctp|dccp)$/.exec(entry);
  if (match === null) return false;
  const low = Number(match[1]);
  const high = match[2] === undefined ? low : Number(match[2]);
  return FORBIDDEN_LISTEN_PORTS.some((port) => port >= low && port <= high);
}

async function k3s(ctx: Context): Promise<Action[]> {
  const { sys, options, log } = ctx;
  if (options.skipK3s) {
    log.info("k3s: skipped (--skip-k3s), left as it is");
    return [];
  }
  const installed = (await sys.stat(K3S_BINARY)) !== null;
  const current = await readText(sys, K3S_CONFIG_PATH);
  const configMatches = current === K3S_CONFIG;
  checkK3sConfig(ctx, current);
  const actions: Action[] = [];
  if (!configMatches) {
    actions.push({
      description: `write ${K3S_CONFIG_PATH} (Traefik and servicelb off)`,
      apply: async () => {
        await sys.makeDirectory("/etc/rancher/k3s", 0o755);
        if (current !== null) {
          const backup = `${K3S_CONFIG_PATH}.bak-${timestamp(sys.now())}`;
          await sys.writeFile(backup, current, 0o600);
          log.info(`k3s: previous config kept as ${backup}`);
        }
        await sys.writeFile(K3S_CONFIG_PATH, K3S_CONFIG, 0o600);
      },
    });
  }
  if (!installed) {
    actions.push({
      description: `install k3s ${K3S_VERSION} with the official installer (SHA-256 pinned)`,
      apply: () => runK3sInstaller(sys, ctx.k3sInstallerSha256),
    });
  } else if (!configMatches) {
    actions.push({
      description: "restart k3s to apply the new config",
      apply: async () => void (await mustRun(sys, ["systemctl", "restart", "k3s"], { timeoutMs: 300_000 })),
    });
  } else {
    const version = (await sys.run([K3S_BINARY, "--version"])).stdout.split("\n")[0] ?? "";
    if (!version.includes(K3S_VERSION)) {
      log.warn(`k3s: installed ${version.trim() || "(unknown version)"}, the installer pins ${K3S_VERSION}; upgrades are not automatic`);
    }
    if ((await sys.run(["systemctl", "is-active", "k3s"])).stdout.trim() !== "active") {
      actions.push({
        description: "enable and start k3s",
        apply: async () => void (await mustRun(sys, ["systemctl", "enable", "--now", "k3s"], { timeoutMs: 300_000 })),
      });
    }
  }
  if (installed && (await sys.run(["systemctl", "is-enabled", "k3s"])).stdout.trim() !== "enabled") {
    // The cluster has to come back after a reboot.
    actions.push({
      description: "enable k3s at boot",
      apply: async () => void (await mustRun(sys, ["systemctl", "enable", "k3s"])),
    });
  }
  if (actions.length > 0) {
    actions.push({ description: "wait until the k3s node is Ready", apply: () => waitForK3sNode(sys) });
  }
  if (configMatches) actions.push(...(await managedFile(sys, K3S_CONFIG_PATH, K3S_CONFIG, 0o600, "k3s config")).actions);
  await checkK3sSelinux(sys);
  return actions;
}

async function checkK3sSelinux(sys: System): Promise<void> {
  if ((await sys.stat(K3S_BINARY)) !== null && (await sys.run(["rpm", "-q", "--quiet", "k3s-selinux"])).code !== 0) {
    throw new InstallError(
      "k3s is installed but its SELinux policy (k3s-selinux) is not; reinstall k3s with the official installer, which installs the policy",
    );
  }
}

/** The signing key file holds exactly the pinned key; anything else there is someone else's. */
async function checkKeyFile(sys: System): Promise<void> {
  const current = await readText(sys, CLOUDFLARE_KEY_PATH);
  if (current !== null && current !== CLOUDFLARE_KEY) {
    throw new InstallError(
      `${CLOUDFLARE_KEY_PATH} holds a different key than the one this installer pins; it is left untouched. ` +
        "Check where it came from, remove it and run again",
    );
  }
}

async function cloudflaredPackage(ctx: Context): Promise<Action[]> {
  const { sys, log } = ctx;
  const actions: Action[] = [];
  await checkRepoFiles(sys);
  await checkKeyFile(sys);
  actions.push(...(await managedFile(sys, CLOUDFLARE_KEY_PATH, CLOUDFLARE_KEY, 0o644, "Cloudflare's package signing key")).actions);
  actions.push(
    ...(await managedFile(sys, CLOUDFLARED_REPO_PATH, CLOUDFLARED_REPO, 0o644, `cloudflared repository (${CLOUDFLARED_REPO_BASEURL})`))
      .actions,
  );
  if ((await sys.run(["rpm", "-q", "--quiet", "cloudflared"])).code !== 0) {
    actions.push({
      description: "install cloudflared from Cloudflare's repository (dnf)",
      apply: async () => {
        // Only from the pinned repository, even if another enabled repository offers it:
        // `--repo` enables just the named repository and disables every other one, in dnf5 and
        // in dnf 4 (where the man page calls it a shortcut for --disablerepo="*" --enablerepo=).
        await mustRun(sys, ["dnf", "install", "-y", `--repo=${CLOUDFLARED_REPO_ID}`, "cloudflared"], {
          inheritOutput: true,
          timeoutMs: 600_000,
        });
        await checkCloudflaredProvenance(sys);
        await checkCloudflaredSupportsTokenFile(sys);
      },
    });
  } else if (ctx.options.reinstallCloudflared && (await cloudflaredProvenanceProblem(sys)) !== null) {
    actions.push({
      description: "replace the installed cloudflared with Cloudflare's signed package from the pinned repository (--reinstall-cloudflared)",
      apply: async () => {
        const repo = `--repo=${CLOUDFLARED_REPO_ID}`;
        // reinstall needs the same version in the repository; distro-sync takes its version.
        const reinstall = await sys.run(["dnf", "reinstall", "-y", repo, "cloudflared"], { inheritOutput: true, timeoutMs: 600_000 });
        if (reinstall.code !== 0) {
          await mustRun(sys, ["dnf", "distro-sync", "-y", repo, "cloudflared"], { inheritOutput: true, timeoutMs: 600_000 });
        }
        await checkCloudflaredProvenance(sys);
        await checkCloudflaredSupportsTokenFile(sys);
      },
    });
  } else {
    await checkCloudflaredProvenance(sys);
    await checkCloudflaredSupportsTokenFile(sys);
    const version = (await sys.run([CLOUDFLARED_BINARY, "--version"])).stdout.trim();
    log.info(`cloudflared: ${version || "installed"}; updates come with \`dnf upgrade --repo=${CLOUDFLARED_REPO_ID} cloudflared\``);
  }
  return actions;
}

/**
 * The binary that receives the tunnel token must be Cloudflare's signed package from the
 * pinned repository. A cloudflared installed any other way (a downloaded RPM, another
 * repository) is refused; the installer does not replace it, because another service on the
 * host may be running it.
 */
async function checkCloudflaredProvenance(sys: System): Promise<void> {
  const problem = await cloudflaredProvenanceProblem(sys);
  if (problem !== null) {
    throw new InstallError(
      `the installed cloudflared did not come from the pinned repository (${problem}). Run again with --reinstall-cloudflared ` +
        "to replace it with Cloudflare's signed package; services already running it keep the old binary until they restart",
    );
  }
}

/** Null when the installed cloudflared is Cloudflare's signed package from the pinned repository. */
async function cloudflaredProvenanceProblem(sys: System): Promise<string | null> {
  const repoquery = await sys.run(["dnf", "repoquery", "--installed", "--queryformat", "%{from_repo}\n", "cloudflared"]);
  const fromRepo = words(repoquery.stdout).at(-1) ?? "(unknown)";
  const signature = (await sys.run(["rpm", "-q", "--queryformat", "%{RSAHEADER:pgpsig}\n", "cloudflared"])).stdout.trim();
  const signedByCloudflare = signature.toLowerCase().includes(`key id ${CLOUDFLARE_KEY_ID}`);
  if (repoquery.code === 0 && fromRepo === CLOUDFLARED_REPO_ID && signedByCloudflare) return null;
  return `installed from ${fromRepo}, signature ${signature || "(none)"}`;
}

async function tunnelCredential(ctx: Context): Promise<Action[]> {
  const { sys, options, log } = ctx;
  const path = credentialPath(options.unit);
  const actions: Action[] = [];
  const store = await sys.stat(CREDSTORE);
  if (store === null) {
    actions.push({
      description: `create ${CREDSTORE} (root only)`,
      apply: () => sys.makeDirectory(CREDSTORE, 0o700),
    });
  } else {
    if (store.type !== "directory") throw new InstallError(`${CREDSTORE} must be a directory`);
    if (store.uid !== 0 || store.gid !== 0 || store.mode !== 0o700) {
      actions.push({
        description: `restore owner root:root and mode 0700 on ${CREDSTORE}`,
        apply: () => sys.restrict(CREDSTORE, 0o700),
      });
    }
  }
  await checkTokenAvailable(ctx);
  const existing = await sys.readFile(path);
  const token = parseTunnelToken(options.token ?? existing ?? new Uint8Array());
  log.info(
    options.token === null
      ? `tunnel token: kept the one in ${path} (tunnel ${token.tunnelId})`
      : `tunnel token: tunnel ${token.tunnelId}`,
  );
  const credential = await managedFile(sys, path, token.value, 0o600, "tunnel token (root only)");
  ctx.credentialChanged = credential.contentChanged;
  actions.push(...credential.actions);
  // The unit's own config (--config), so cloudflared never reads another connector's settings.
  const config = await managedFile(sys, configPath(options.unit), renderConfig(options.unit, options.metricsPort), 0o600, "cloudflared config (root only)");
  ctx.configChanged = config.contentChanged;
  actions.push(...config.actions);
  return actions;
}

/**
 * Plans a file the installer owns: rewrite it when its content differs (the write sets root
 * ownership and the mode), otherwise restore owner and mode if someone changed them. Anything
 * but a regular file at the path, such as a symbolic link, is refused.
 */
async function managedFile(
  sys: System,
  path: string,
  content: string,
  mode: number,
  what: string,
): Promise<{ actions: Action[]; contentChanged: boolean }> {
  const info = await sys.stat(path);
  if (info !== null && info.type !== "file") {
    throw new InstallError(`${path} is a ${info.type}, not a regular file; remove it and run again`);
  }
  const current = info === null ? null : await readText(sys, path);
  if (current !== content) {
    return {
      contentChanged: true,
      actions: [
        {
          description: `${current === null ? "write" : "replace"} ${path}: ${what}`,
          apply: () => sys.writeFile(path, content, mode),
        },
      ],
    };
  }
  if (info !== null && (info.uid !== 0 || info.gid !== 0 || info.mode !== mode)) {
    return {
      contentChanged: false,
      actions: [
        {
          description: `restore owner root:root and mode 0${mode.toString(8)} on ${path}`,
          apply: () => sys.restrict(path, mode),
        },
      ],
    };
  }
  return { contentChanged: false, actions: [] };
}

async function cloudflaredUnit(ctx: Context): Promise<Action[]> {
  const { sys, options, log } = ctx;
  const unit = options.unit;
  const path = unitPath(unit);
  await checkUnitFileOwned(ctx);
  if ((await sys.stat(`${path}.d`)) !== null) {
    log.warn(`${path}.d exists: its drop-ins can weaken the sandbox; the security check below measures the result`);
  }
  const actions: Action[] = [];
  const unitFile = await managedFile(sys, path, renderUnit(unit), 0o644, "cloudflared unit (sandboxed, token through LoadCredential)");
  actions.push(...unitFile.actions);
  if (unitFile.contentChanged) {
    ctx.unitChanged = true;
    actions.push({
      description: "reload systemd's unit files",
      apply: async () => void (await mustRun(sys, ["systemctl", "daemon-reload"])),
    });
  }
  const enabled = (await sys.run(["systemctl", "is-enabled", `${unit}.service`])).stdout.trim() === "enabled";
  const active = (await sys.run(["systemctl", "is-active", `${unit}.service`])).stdout.trim() === "active";
  if (!enabled) {
    actions.push({
      description: `enable ${unit}.service at boot`,
      apply: async () => void (await mustRun(sys, ["systemctl", "enable", `${unit}.service`])),
    });
  }
  if (!active) {
    actions.push({
      description: `start ${unit}.service and wait for its first tunnel connection`,
      apply: () => startUnit(sys, ["start", `${unit}.service`], unit),
    });
  } else if (ctx.unitChanged || ctx.credentialChanged || ctx.configChanged) {
    actions.push({
      description: `restart ${unit}.service (open requests get up to 30 seconds to finish, then about 4 seconds without a connection)`,
      apply: () => startUnit(sys, ["restart", `${unit}.service`], unit),
    });
  }
  return actions;
}

// ---------------------------------------------------------------------------------------------
// Verification: the acceptance checks of Laysi/trustiot-v2#64, run on every installation.

async function verify(ctx: Context): Promise<number | null> {
  const { sys, options, log } = ctx;
  const problems: string[] = [];
  const unit = `${options.unit}.service`;
  let exposure: number | null = null;
  if ((await readText(sys, unitPath(options.unit))) !== null) {
    if (!options.dryRun && (await sys.run(["systemctl", "is-active", unit])).stdout.trim() !== "active") {
      problems.push(`${unit} is not active; see \`journalctl -u ${unit}\``);
    }
    const analysis = await sys.run(["systemd-analyze", "security", "--no-pager", unit]);
    const match = /Overall exposure level for \S+: (\d+(?:\.\d+)?)/.exec(analysis.stdout);
    if (match === null) {
      problems.push(`systemd-analyze security ${unit} gave no exposure level: ${analysis.stderr.trim()}`);
    } else {
      exposure = Number(match[1]);
      if (exposure > MAX_EXPOSURE) {
        problems.push(`${unit} has a systemd-analyze security exposure of ${exposure}, above ${MAX_EXPOSURE}`);
      } else {
        log.info(`check: systemd-analyze security ${unit} = ${exposure} (at most ${MAX_EXPOSURE})`);
      }
    }
  }
  const listeners = parseListeners((await mustRun(sys, ["ss", "-Htlnp"])).stdout);
  const udp = parseListeners((await mustRun(sys, ["ss", "-Hulnp"])).stdout);
  const forbidden = [
    ...listeners.filter((listener) => FORBIDDEN_LISTEN_PORTS.includes(listener.port)).map((l) => `TCP ${l.address}:${l.port} ${l.process}`),
    ...udp.filter((listener) => FORBIDDEN_LISTEN_PORTS.includes(listener.port)).map((l) => `UDP ${l.address}:${l.port} ${l.process}`),
  ];
  if (forbidden.length > 0) {
    problems.push(`the host listens on web ports: ${forbidden.join("; ")}`);
  } else {
    log.info(`check: nothing listens on TCP or UDP ${FORBIDDEN_LISTEN_PORTS.join(" or ")}`);
  }
  // A pod's hostPort is a NAT rule, not a listener, so ss cannot see it; ask k3s.
  if ((await sys.run(["systemctl", "is-active", "k3s"])).stdout.trim() === "active") {
    const pods = await mustRun(sys, [
      K3S_BINARY,
      "kubectl",
      "get",
      "pods",
      "--all-namespaces",
      "-o",
      'jsonpath={range .items[*]}{.metadata.namespace}/{.metadata.name}{" "}{.spec.containers[*].ports[*].hostPort}{"\\n"}{end}',
    ]);
    const hostPorts = pods.stdout
      .split("\n")
      .map((line) => words(line))
      .filter(([, ...ports]) => ports.some((port) => FORBIDDEN_LISTEN_PORTS.includes(Number(port))))
      .map(([pod]) => pod);
    if (hostPorts.length > 0) {
      problems.push(`pods publish a host port ${FORBIDDEN_LISTEN_PORTS.join(" or ")}: ${hostPorts.join(", ")}`);
    } else {
      log.info(`check: no pod publishes host port ${FORBIDDEN_LISTEN_PORTS.join(" or ")}`);
    }
  }
  const exposed = listeners.filter((listener) => !isLoopback(listener.address));
  if (exposed.length > 0) {
    log.info(
      `note: TCP listeners on non-loopback addresses (firewalld decides who reaches them): ${[
        ...new Set(exposed.map((listener) => `${listener.port} ${listener.process}`)),
      ].join(", ")}`,
    );
  }
  if (problems.length > 0) {
    if (options.dryRun) {
      for (const problem of problems) log.warn(`check: ${problem}`);
    } else {
      throw new InstallError(problems.join("\n"));
    }
  }
  return exposure;
}

// ---------------------------------------------------------------------------------------------
// Helpers

async function runK3sInstaller(sys: System, expected: string): Promise<void> {
  const script = await sys.download(K3S_INSTALLER_URL);
  const digest = createHash("sha256").update(script).digest("hex");
  if (digest !== expected) {
    throw new InstallError(
      `the k3s installer from ${K3S_INSTALLER_URL} has SHA-256 ${digest}, expected ${expected}; not running it`,
    );
  }
  const dir = await sys.makeTempDirectory();
  try {
    const path = `${dir}/k3s-install.sh`;
    await sys.writeFile(path, script, 0o700);
    await mustRun(sys, ["sh", path], {
      env: { INSTALL_K3S_VERSION: K3S_VERSION },
      inheritOutput: true,
      timeoutMs: 900_000,
    });
  } finally {
    await sys.remove(dir);
  }
}

async function waitForK3sNode(sys: System): Promise<void> {
  const deadline = sys.now().getTime() + 300_000;
  let last = "";
  while (sys.now().getTime() < deadline) {
    const result = await sys.run([K3S_BINARY, "kubectl", "get", "nodes", "--no-headers"]);
    const nodes = result.stdout.split("\n").filter((line) => line.trim() !== "");
    if (result.code === 0 && nodes.length > 0 && nodes.every((line) => line.trim().split(/\s+/)[1] === "Ready")) {
      // A new node can be Ready a few seconds before the controller manager assigns its pod
      // network, so the networks are checked once they are there.
      const networks = await readK3sNetworks(sys);
      if (networks.podNetworks.length > 0 && networks.serviceIps.length > 0) {
        checkK3sNetworkRanges(networks);
        return;
      }
      last = "the node is Ready but has no pod network assigned yet";
    } else {
      last = (result.stdout + result.stderr).trim();
    }
    await sys.sleep(5_000);
  }
  throw new InstallError(`the k3s node is not Ready after 5 minutes: ${last}`);
}

/**
 * firewalld trusts k3s's default pod network, and the checks assume its default service network. A cluster set up with
 * other networks (or IPv6) would be cut off by the firewall while unrelated addresses got
 * trusted, so its actual networks are read from the API server and must lie inside them.
 */
async function checkK3sNetworks(sys: System): Promise<void> {
  checkK3sNetworkRanges(await readK3sNetworks(sys));
}

interface K3sNetworks {
  podNetworks: string[];
  serviceIps: string[];
}

async function readK3sNetworks(sys: System): Promise<K3sNetworks> {
  const kubectl = (args: string[]) => mustRun(sys, [K3S_BINARY, "kubectl", ...args]);
  return {
    podNetworks: words((await kubectl(["get", "nodes", "-o", "jsonpath={.items[*].spec.podCIDRs[*]}"])).stdout),
    serviceIps: words((await kubectl(["-n", "default", "get", "service", "kubernetes", "-o", "jsonpath={.spec.clusterIPs[*]}"])).stdout),
  };
}

function checkK3sNetworkRanges({ podNetworks, serviceIps }: K3sNetworks): void {
  const [podRange, serviceRange] = [K3S_POD_NETWORK, K3S_SERVICE_NETWORK];
  const wrong = [
    ...podNetworks.filter((network) => !cidrWithin(network, podRange)).map((network) => `pod network ${network}`),
    ...serviceIps.filter((ip) => !cidrWithin(`${ip}/32`, serviceRange)).map((ip) => `service address ${ip}`),
  ];
  if (podNetworks.length === 0 || serviceIps.length === 0 || wrong.length > 0) {
    throw new InstallError(
      `the k3s cluster does not use the networks the firewall trusts (${podRange} for pods, ${serviceRange} for services): ` +
        `${wrong.join(", ") || "its networks could not be read"}. Only k3s's default IPv4 networks are supported`,
    );
  }
}

/** True when the IPv4 network `inner` (a.b.c.d/n) lies inside `outer`; false for anything else, IPv6 included. */
export function cidrWithin(inner: string, outer: string): boolean {
  const parse = (cidr: string): [number, number] | null => {
    const match = /^(\d{1,3})\.(\d{1,3})\.(\d{1,3})\.(\d{1,3})\/(\d{1,2})$/.exec(cidr);
    if (match === null) return null;
    const octets = match.slice(1, 5).map(Number);
    const bits = Number(match[5]);
    if (octets.some((octet) => octet > 255) || bits > 32) return null;
    return [octets.reduce((value, octet) => value * 256 + octet, 0), bits];
  };
  const a = parse(inner);
  const b = parse(outer);
  if (a === null || b === null || a[1] < b[1]) return false;
  const size = 2 ** (32 - b[1]);
  return Math.floor(a[0] / size) === Math.floor(b[0] / size);
}

async function startUnit(sys: System, verb: string[], unit: string): Promise<void> {
  // Type=notify: systemctl returns once cloudflared has registered its first connection.
  const result = await sys.run(["systemctl", ...verb], { timeoutMs: 180_000 });
  if (result.code !== 0) {
    throw new InstallError(
      `systemctl ${verb.join(" ")} failed: ${result.stderr.trim()}\nSee \`journalctl -u ${unit}.service\`; a rejected token shows up there`,
    );
  }
}

async function checkCloudflaredSupportsTokenFile(sys: System): Promise<void> {
  const help = await sys.run([CLOUDFLARED_BINARY, "tunnel", "run", "--help"]);
  if (!`${help.stdout}${help.stderr}`.includes("--token-file")) {
    throw new InstallError("the installed cloudflared has no --token-file option; update it with `dnf upgrade cloudflared`");
  }
}

/**
 * firewalld's trusted zone accepts everything. The installer uses it only for the k3s
 * networks, so it refuses a host where the trusted zone is the default zone, holds a network
 * interface, or trusts any other source: closing web ports elsewhere would not protect it.
 */
async function checkTrustedZone(sys: System, running: boolean): Promise<void> {
  const problems: string[] = [];
  // A running firewalld is asked for its runtime and its permanent configuration; a stopped
  // one is checked offline, before it starts, so a bad configuration never becomes active.
  const scopes = running
    ? [
        { cmd: ["firewall-cmd"], label: "runtime" },
        { cmd: ["firewall-cmd", "--permanent"], label: "permanent" },
      ]
    : [{ cmd: ["firewall-offline-cmd"], label: "permanent" }];
  const defaultZone = (await mustRun(sys, [running ? "firewall-cmd" : "firewall-offline-cmd", "--get-default-zone"])).stdout.trim();
  if (defaultZone === "trusted") problems.push("trusted is the default zone");
  // firewall-cmd has no --permanent form of --get-default-zone; the permanent default lives in
  // firewalld.conf and comes back after a reload or reboot.
  const config = (await readText(sys, FIREWALLD_CONFIG)) ?? "";
  if (/^\s*DefaultZone\s*=\s*trusted\s*$/m.test(config)) problems.push(`trusted is the default zone in ${FIREWALLD_CONFIG}`);
  for (const { cmd, label } of scopes) {
    const interfaces = words((await mustRun(sys, [...cmd, "--zone=trusted", "--list-interfaces"])).stdout);
    if (interfaces.length > 0) problems.push(`the trusted zone holds the interfaces ${interfaces.join(", ")} (${label})`);
    const sources = words((await mustRun(sys, [...cmd, "--zone=trusted", "--list-sources"])).stdout);
    // The service network may be there from an older setup; firewalldConfig removes it.
    const extra = sources.filter((source) => source !== K3S_POD_NETWORK && source !== K3S_SERVICE_NETWORK);
    if (extra.length > 0) problems.push(`the trusted zone trusts ${extra.join(", ")} (${label})`);
  }
  if (problems.length > 0) {
    throw new InstallError(
      `firewalld's trusted zone accepts all traffic and must hold only the k3s pod network ${K3S_POD_NETWORK}, but ${problems.join("; ")}. ` +
        "Move interfaces and other sources to a restrictive zone (for example public) and run again",
    );
  }
}

function words(text: string): string[] {
  return text.split(/\s+/).filter((word) => word !== "");
}

async function firewallQuery(sys: System, cmd: string[]): Promise<boolean> {
  const result = await sys.run(cmd);
  if (result.code === 0) return true;
  if (result.code === 1) return false;
  throw new InstallError(`${cmd.join(" ")} failed: ${result.stderr.trim() || result.stdout.trim()}`);
}

async function mustRun(sys: System, cmd: string[], options?: RunOptions): Promise<RunResult> {
  const result = await sys.run(cmd, options);
  if (result.code !== 0) {
    const detail = options?.inheritOutput ? "see the output above" : result.stderr.trim() || result.stdout.trim();
    throw new InstallError(`${cmd.join(" ")} failed (exit ${result.code}): ${detail}`);
  }
  return result;
}

async function readText(sys: System, path: string): Promise<string | null> {
  const data = await sys.readFile(path);
  return data === null ? null : new TextDecoder().decode(data);
}

export function parseOsRelease(text: string | null): Record<string, string> {
  const fields: Record<string, string> = {};
  for (const line of (text ?? "").split("\n")) {
    const match = /^([A-Z_]+)=(.*)$/.exec(line.trim());
    if (match?.[1] !== undefined && match[2] !== undefined) fields[match[1]] = match[2].replace(/^["']|["']$/g, "");
  }
  return fields;
}

export interface Listener {
  address: string;
  port: number;
  process: string;
}

/** Parses `ss -Htlnp`: State Recv-Q Send-Q Local:Port Peer:Port [Process]. */
export function parseListeners(output: string): Listener[] {
  const listeners: Listener[] = [];
  for (const line of output.split("\n")) {
    const columns = line.trim().split(/\s+/);
    const local = columns[3];
    if (local === undefined) continue;
    const separator = local.lastIndexOf(":");
    const port = Number(local.slice(separator + 1));
    if (separator < 0 || !Number.isInteger(port)) continue;
    const process = /users:\(\("([^"]+)"/.exec(line)?.[1] ?? "(unknown process)";
    listeners.push({ address: local.slice(0, separator), port, process });
  }
  return listeners;
}

function isLoopback(address: string): boolean {
  const bare = address.replace(/^\[|\]$/g, "").replace(/%.*$/, "");
  return bare.startsWith("127.") || bare === "::1";
}

function timestamp(date: Date): string {
  return date.toISOString().replace(/[-:]/g, "").replace(/\.\d+Z$/, "Z");
}
