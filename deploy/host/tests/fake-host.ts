// A simulated Fedora host for the installer's tests. It models only what the installer
// touches: files, RPM packages, systemd units, firewalld zones, SELinux mode, k3s and the TCP
// listeners. Every command and every log line is recorded, so tests can check that a secret
// never shows up in either.
import { createHash } from "node:crypto";
import { CLOUDFLARED_BINARY, K3S_BINARY, MARKER } from "../src/desired.ts";
import type { Logger } from "../src/install.ts";
import type { FileInfo, RunOptions, RunResult, System } from "../src/system.ts";

interface Zone {
  services: Set<string>;
  ports: Set<string>;
  sources: Set<string>;
  interfaces: Set<string>;
  richRules: Set<string>;
  forwardPorts: Set<string>;
}

interface Unit {
  enabled: boolean;
  active: boolean;
}

export const FAKE_INSTALLER = new TextEncoder().encode("#!/bin/sh\necho fake k3s installer\n");
export const FAKE_INSTALLER_SHA256 = createHash("sha256").update(FAKE_INSTALLER).digest("hex");

export class FakeHost implements System {
  files = new Map<string, { data: Uint8Array; mode: number; uid: number }>();
  directories = new Map<string, { mode: number; uid: number }>();
  /** Paths that are symbolic links (their target's content is in `files`). */
  symlinks = new Set<string>();
  packages = new Set<string>(["systemd", "iproute"]);
  /** Where an installed package came from and who signed it, as dnf and rpm report them. */
  provenance = new Map<string, { fromRepo: string; signature: string }>();
  /** Whether Cloudflare's repository still offers the installed cloudflared version (for dnf reinstall). */
  repositoryHasInstalledVersion = true;
  /** What the k3s API server reports: the node's pod networks and the kubernetes Service's addresses. */
  podCidrs = "10.42.0.0/24";
  serviceIps = "10.43.0.1";
  /** Number of pod network queries that come back empty, as for a node that just became Ready. */
  podCidrsPending = 0;
  units = new Map<string, Unit>();
  selinux = "Enforcing";
  defaultZone = "FedoraServer";
  /** Zone and policy targets other than the defaults (trusted ACCEPT, other zones default, policies CONTINUE). */
  targets = new Map<string, string>();
  /** Output of `firewall-cmd --direct --get-all-rules`. */
  directRules = "";
  /** "unknown" makes --info-zone/--info-policy print only the target, a layout the installer must refuse. */
  infoFormat: "firewalld-2.4" | "unknown" = "firewalld-2.4";
  /** False for a firewalld older than 0.9, which has no policies. */
  policiesSupported = true;
  /** False for a cloudflared older than its --token-file option. */
  cloudflaredHasTokenFile = true;
  /** firewalld policies, the same at runtime and permanently. */
  policies = new Map<string, Zone>([["allow-host-ipv6", emptyZone()]]);
  servicePorts: Record<string, string[]> = {
    http: ["80/tcp"],
    https: ["443/tcp"],
    http3: ["443/udp"],
    ssh: ["22/tcp"],
    cockpit: ["9090/tcp"],
    "dhcpv6-client": ["546/udp"],
    "web-alt": ["8080/tcp", "443/tcp"],
  };
  runtime = new Map<string, Zone>();
  permanent = new Map<string, Zone>();
  listeners: string[] = [
    'LISTEN 0 128 0.0.0.0:22 0.0.0.0:* users:(("sshd",pid=900,fd=6))',
    'LISTEN 0 4096 127.0.0.53%lo:53 0.0.0.0:* users:(("systemd-resolve",pid=600,fd=13))',
  ];
  udpListeners: string[] = ['UNCONN 0 0 127.0.0.53%lo:53 0.0.0.0:* users:(("systemd-resolve",pid=600,fd=12))'];
  /** `namespace/name hostPort...` per pod, as the installer's kubectl query prints it. */
  podHostPorts = "kube-system/coredns-1 \nkube-system/metrics-server-1 \n";
  exposure = 1.1;
  installerBytes: Uint8Array = FAKE_INSTALLER;
  commands: string[][] = [];
  /** Number of changes made to the host; a dry run must leave it at 0. */
  mutations = 0;
  output: string[] = [];
  osRelease = 'NAME="Fedora Linux"\nID=fedora\nVERSION_ID=44\nPRETTY_NAME="Fedora Linux 44 (Cloud Edition)"\n';
  private clock = Date.parse("2026-10-10T00:00:00Z");

  constructor() {
    this.files.set("/etc/os-release", { data: encode(this.osRelease), mode: 0o644, uid: 0 });
    this.files.set("/usr/bin/dnf", { data: encode(""), mode: 0o755, uid: 0 });
    this.files.set("/etc/yum.repos.d/fedora.repo", {
      data: encode("[fedora]\nmetalink=https://mirrors.fedoraproject.org/metalink?repo=fedora-$releasever&arch=$basearch\n"),
      mode: 0o644,
      uid: 0,
    });
    for (const zone of ["FedoraServer", "public", "trusted", "work"]) {
      this.runtime.set(zone, emptyZone());
      this.permanent.set(zone, emptyZone());
    }
    for (const zones of [this.runtime, this.permanent]) {
      zones.get("FedoraServer")?.services.add("ssh").add("cockpit");
      zones.get("work")?.ports.add("1025-65535/tcp");
    }
    this.files.set("/etc/firewalld/firewalld.conf", { data: encode("DefaultZone=FedoraServer\n"), mode: 0o644, uid: 0 });
  }

  readonly log: Logger = {
    info: (message) => this.output.push(`  ${message}`),
    warn: (message) => this.output.push(`! ${message}`),
    change: (message) => this.output.push(`+ ${message}`),
  };

  text(path: string): string | undefined {
    const file = this.files.get(path);
    return file === undefined ? undefined : new TextDecoder().decode(file.data);
  }

  /** Puts the host in the state a finished installation leaves, without the installer. */
  markActive(unit: string): void {
    this.units.set(unit, { enabled: true, active: true });
  }

  async run(cmd: string[], _options?: RunOptions): Promise<RunResult> {
    this.commands.push([...cmd]);
    const [program, ...args] = cmd;
    switch (program) {
      case "systemctl":
        return this.systemctl(args);
      case "getenforce":
        return ok(`${this.selinux}\n`);
      case "rpm": {
        const name = args.at(-1) ?? "";
        if (!this.packages.has(name)) return fail(1, `package ${name} is not installed`);
        if (args.includes("--queryformat")) return ok(`${this.provenance.get(name)?.signature ?? "(none)"}\n`);
        return ok("");
      }
      case "dnf":
        return this.dnf(args);
      case "firewall-cmd":
        return this.firewall(args);
      case "firewall-offline-cmd":
        // Reads and changes the permanent configuration, with or without a running firewalld.
        if (!this.packages.has("firewalld")) return fail(127, "firewall-offline-cmd: command not found");
        return this.firewall(["--permanent", ...args], true);
      case "sh":
        return this.k3sInstaller();
      case K3S_BINARY:
        if (args[0] === "--version") return ok("k3s version v1.36.5+k3s1 (abcdef12)\ngo version go1.25\n");
        if (!this.units.get("k3s")?.active) return fail(1, "connection refused");
        if (args.includes("jsonpath={.items[*].spec.podCIDRs[*]}")) {
          if (this.podCidrsPending > 0) {
            this.podCidrsPending--;
            return ok("");
          }
          return ok(this.podCidrs);
        }
        if (args.includes("jsonpath={.spec.clusterIPs[*]}")) return ok(this.serviceIps);
        if (args.includes("pods")) return ok(this.podHostPorts);
        return ok("host Ready control-plane 1m v1.36.5+k3s1\n");
      case CLOUDFLARED_BINARY:
        if (!this.files.has(CLOUDFLARED_BINARY)) return fail(127, "not found");
        if (args[0] === "--version") return ok("cloudflared version 2026.10.0\n");
        if (!this.cloudflaredHasTokenFile) return ok("OPTIONS:\n   --token value   The Tunnel token.\n");
        return ok("OPTIONS:\n   --token-file value   Filepath at which to read the tunnel token.\n");
      case "systemd-analyze": {
        const unit = args.at(-1) ?? "";
        return ok(`→ Overall exposure level for ${unit}: ${this.exposure} OK\n`);
      }
      case "ss":
        return ok(`${(args[0] === "-Hulnp" ? this.udpListeners : this.listeners).join("\n")}\n`);
      default:
        return fail(127, `${program}: command not found`);
    }
  }

  private systemctl(args: string[]): RunResult {
    const [verb, ...rest] = args;
    const name = (rest.at(-1) ?? "").replace(/\.service$/, "");
    const unit = this.units.get(name);
    switch (verb) {
      case "--version":
        return ok("systemd 259 (259.9-1.fc44)\n+PAM +AUDIT\n");
      case "is-enabled":
        return unit?.enabled ? ok("enabled\n") : fail(1, unit ? "disabled\n" : "not-found\n");
      case "is-active":
        return unit?.active ? ok("active\n") : fail(3, "inactive\n");
      case "daemon-reload":
        this.mutations++;
        return ok("");
      case "enable": {
        const known = this.knownUnit(name);
        if (!known) return fail(1, `Unit ${name}.service not found.`);
        known.enabled = true;
        if (rest[0] === "--now") this.activate(name, known);
        this.mutations++;
        return ok("");
      }
      case "start":
      case "restart": {
        const known = this.knownUnit(name);
        if (!known) return fail(5, `Unit ${name}.service not found.`);
        this.activate(name, known);
        this.mutations++;
        return ok("");
      }
      default:
        return fail(1, `unsupported systemctl ${verb}`);
    }
  }

  /** Starting firewalld loads its permanent configuration into the runtime one, as the real one does. */
  private activate(name: string, unit: Unit): void {
    if (name === "firewalld" && !unit.active) {
      this.runtime = new Map(
        [...this.permanent].map(([zone, config]) => [
          zone,
          {
            services: new Set(config.services),
            ports: new Set(config.ports),
            sources: new Set(config.sources),
            interfaces: new Set(config.interfaces),
            richRules: new Set(config.richRules),
            forwardPorts: new Set(config.forwardPorts),
          },
        ]),
      );
    }
    unit.active = true;
  }

  private knownUnit(name: string): Unit | undefined {
    const existing = this.units.get(name);
    if (existing) return existing;
    const packaged = (name === "firewalld" && this.packages.has("firewalld")) || name === "k3s";
    const written = this.text(`/etc/systemd/system/${name}.service`)?.startsWith(MARKER);
    if (!packaged && !written) return undefined;
    const unit = { enabled: false, active: false };
    this.units.set(name, unit);
    return unit;
  }

  private dnf(args: string[]): RunResult {
    const name = args.at(-1) ?? "";
    if (args[0] === "repoquery") {
      return ok(this.packages.has(name) ? `${this.provenance.get(name)?.fromRepo ?? "fedora"}\n` : "");
    }
    if (args[0] === "reinstall" && !this.repositoryHasInstalledVersion) return fail(1, "Installed package cloudflared not available.");
    if (!["install", "reinstall", "distro-sync"].includes(args[0] ?? "") || !args.includes("-y")) return fail(1, "unsupported dnf call");
    if (name === "cloudflared") {
      this.provenance.set(name, {
        fromRepo: "cloudflared-stable",
        signature: "RSA/SHA512, Thu Oct  8 01:38:14 2026, Key ID 8a682d308d4e5e73",
      });
    }
    if (name === "cloudflared" && !this.text("/etc/yum.repos.d/cloudflared.repo")) {
      return fail(1, "No match for argument: cloudflared");
    }
    this.packages.add(name);
    if (name === "cloudflared") this.files.set(CLOUDFLARED_BINARY, { data: encode(""), mode: 0o755, uid: 0 });
    this.mutations++;
    return ok("");
  }

  private k3sInstaller(): RunResult {
    this.mutations++;
    this.files.set(K3S_BINARY, { data: encode(""), mode: 0o755, uid: 0 });
    this.packages.add("k3s-selinux");
    this.units.set("k3s", { enabled: true, active: true });
    return ok("");
  }

  private firewall(args: string[], offline = false): RunResult {
    const running = this.units.get("firewalld")?.active ?? false;
    if (!running && !offline) return fail(252, "FirewallD is not running");
    if (args[0] === "--state") return ok("running\n");
    if (args.includes("--get-default-zone")) {
      const configured = /^DefaultZone=(.*)$/m.exec(this.text("/etc/firewalld/firewalld.conf") ?? "")?.[1];
      return ok(`${offline ? configured : this.defaultZone}\n`);
    }
    const permanent = args.includes("--permanent");
    if (args.includes("--get-zones")) return ok(`${[...(permanent ? this.permanent : this.runtime).keys()].sort().join(" ")}\n`);
    if (args.includes("--get-policies")) {
      return this.policiesSupported ? ok(`${[...this.policies.keys()].join(" ")}\n`) : fail(2, "unrecognized arguments: --get-policies");
    }
    const service = args.find((arg) => arg.startsWith("--info-service="))?.slice(15);
    if (service !== undefined) {
      const ports = this.servicePorts[service];
      if (ports === undefined) return fail(101, "INVALID_SERVICE");
      return ok(`${service}\n  ports: ${ports.join(" ")}\n  protocols: \n  source-ports: \n  modules: \n  destination: \n`);
    }
    if (args.includes("--direct")) return ok(args.includes("--get-all-rules") ? this.directRules : "");
    const info = args.find((arg) => /^--info-(zone|policy)=/.test(arg));
    if (info !== undefined) {
      const [, kind, name] = /^--info-(zone|policy)=(.*)$/.exec(info) ?? [];
      const entry = kind === "policy" ? this.policies.get(name ?? "") : (permanent ? this.permanent : this.runtime).get(name ?? "");
      if (!entry) return fail(112, "INVALID_ZONE");
      const target = this.targets.get(name ?? "") ?? (kind === "policy" ? "CONTINUE" : name === "trusted" ? "ACCEPT" : "default");
      if (this.infoFormat === "unknown") return ok(`${name}\n  target: ${target}\n`);
      // The layout of firewalld 2.4: one `  key: value` line per field, and the entries of
      // forward-ports and rich rules on their own lines, indented with a tab.
      const list = (items: Set<string>) => [...items].map((item) => `\t${item}\n`).join("");
      const head =
        kind === "policy"
          ? `${name}\n  disable: no\n  priority: -1\n  target: ${target}\n  ingress-zones: ANY\n  egress-zones: HOST\n`
          : `${name}\n  target: ${target}\n  ingress-priority: 0\n  egress-priority: 0\n  icmp-block-inversion: no\n` +
            `  interfaces: ${[...entry.interfaces].join(" ")}\n  sources: ${[...entry.sources].join(" ")}\n`;
      return ok(
        `${head}  services: ${[...entry.services].join(" ")}\n  ports: ${[...entry.ports].join(" ")}\n  protocols: \n` +
          `  forward: yes\n  masquerade: no\n  forward-ports: \n${list(entry.forwardPorts)}  source-ports: \n  icmp-blocks: \n` +
          `  rich rules: \n${list(entry.richRules)}`,
      );
    }
    const policyName = args.find((arg) => arg.startsWith("--policy="))?.slice(9);
    const zoneName = args.find((arg) => arg.startsWith("--zone="))?.slice(7) ?? this.defaultZone;
    const zone = policyName !== undefined ? this.policies.get(policyName) : (permanent ? this.permanent : this.runtime).get(zoneName);
    if (!zone) return fail(112, "INVALID_ZONE");
    if (args.includes("--list-interfaces")) return ok(`${[...zone.interfaces].join(" ")}\n`);
    if (args.includes("--list-sources")) return ok(`${[...zone.sources].join(" ")}\n`);
    if (args.includes("--list-services")) return ok(`${[...zone.services].join(" ")}\n`);
    if (args.includes("--list-ports")) return ok(`${[...zone.ports].join(" ")}\n`);
    if (args.includes("--list-forward-ports")) return ok(`${[...zone.forwardPorts].join("\n")}\n`);
    if (args.includes("--list-rich-rules")) return ok(`${[...zone.richRules].join("\n")}\n`);
    const pattern = /^--(query|add|remove)-(service|port|source|rich-rule)=(.*)$/;
    const operation = args.find((arg) => pattern.test(arg));
    if (!operation) return fail(2, "unsupported firewall-cmd call");
    const [, verb, kind, value] = pattern.exec(operation) ?? [];
    const sets = { service: zone.services, port: zone.ports, source: zone.sources, "rich-rule": zone.richRules };
    const set = sets[kind as keyof typeof sets];
    if (verb === "query") return set.has(value ?? "") ? ok("yes\n") : fail(1, "no\n");
    this.mutations++;
    if (verb === "add") set.add(value ?? "");
    else set.delete(value ?? "");
    return ok("success\n");
  }

  async readFile(path: string): Promise<Uint8Array | null> {
    return this.files.get(path)?.data ?? null;
  }

  async listDirectory(path: string): Promise<string[]> {
    const prefix = `${path}/`;
    return [...this.files.keys()]
      .filter((name) => name.startsWith(prefix) && !name.slice(prefix.length).includes("/"))
      .map((name) => name.slice(prefix.length))
      .sort();
  }

  async writeFile(path: string, data: Uint8Array | string, mode: number): Promise<void> {
    this.mutations++;
    this.files.set(path, { data: typeof data === "string" ? encode(data) : data, mode, uid: 0 });
  }

  async makeDirectory(path: string, mode: number): Promise<void> {
    this.mutations++;
    this.directories.set(path, { mode, uid: 0 });
  }

  async restrict(path: string, mode: number): Promise<void> {
    this.mutations++;
    const entry = this.files.get(path) ?? this.directories.get(path);
    if (entry) {
      entry.mode = mode;
      entry.uid = 0;
    }
  }

  async remove(path: string): Promise<void> {
    for (const name of [...this.files.keys()]) if (name === path || name.startsWith(`${path}/`)) this.files.delete(name);
    this.directories.delete(path);
  }

  async stat(path: string): Promise<FileInfo | null> {
    if (this.symlinks.has(path)) return { mode: 0o777, uid: 0, gid: 0, type: "symlink" };
    const file = this.files.get(path);
    if (file) return { mode: file.mode, uid: file.uid, gid: 0, type: "file" };
    const directory = this.directories.get(path);
    if (directory) return { mode: directory.mode, uid: directory.uid, gid: 0, type: "directory" };
    return null;
  }

  async download(_url: string): Promise<Uint8Array> {
    return this.installerBytes;
  }

  async makeTempDirectory(): Promise<string> {
    this.directories.set("/var/tmp/trustiot-host-install.test", { mode: 0o700, uid: 0 });
    return "/var/tmp/trustiot-host-install.test";
  }

  uid(): number {
    return 0;
  }

  arch(): string {
    return "arm64";
  }

  async sleep(ms: number): Promise<void> {
    this.clock += ms;
  }

  now(): Date {
    return new Date(this.clock);
  }
}

function emptyZone(): Zone {
  return {
    services: new Set(),
    ports: new Set(),
    sources: new Set(),
    interfaces: new Set(),
    richRules: new Set(),
    forwardPorts: new Set(),
  };
}

function encode(text: string): Uint8Array {
  return new TextEncoder().encode(text);
}

function ok(stdout: string): RunResult {
  return { code: 0, stdout, stderr: "" };
}

function fail(code: number, stderr: string): RunResult {
  return { code, stdout: code === 1 || code === 3 ? stderr : "", stderr };
}
