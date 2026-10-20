// Command line of trustiot-host-install. The tunnel token is accepted only from stdin or a
// file: an argument would show up in the process list and the shell history.
import { DEFAULT_METRICS_PORTS, DEFAULT_UNIT } from "./desired.ts";
import type { InstallOptions } from "./install.ts";

export const USAGE = `Usage: trustiot-host-install [options]

Installs and configures a TrustIoT SaaS host: SELinux check, firewalld, k3s (Traefik and
servicelb off) and cloudflared as a sandboxed systemd service. Safe to run again: it only
changes what differs. Run as root. See deploy/host/README.md.

Options:
  --token-stdin        read the Cloudflare Tunnel token from standard input
  --token-file PATH    read the Cloudflare Tunnel token from a file
                       (without either, the token already on the host is kept)
  --unit NAME          systemd unit name for cloudflared (default: ${DEFAULT_UNIT})
  --metrics-port PORT  serve cloudflared's metrics on 127.0.0.1:PORT (default: the first free
                       one of ${DEFAULT_METRICS_PORTS[0]}-${DEFAULT_METRICS_PORTS.at(-1)}); set it when another connector runs on the host
  --skip-k3s           leave k3s exactly as it is
  --reconfigure-k3s    replace a different /etc/rancher/k3s/config.yaml (kept as a backup)
                       and restart k3s
  --reinstall-cloudflared
                       replace a cloudflared that was not installed from Cloudflare's
                       signed repository (running services keep the old binary until
                       they restart)
  --dry-run            print the changes without making them
  -h, --help           show this help
`;

export type TokenSource = { kind: "none" } | { kind: "stdin" } | { kind: "file"; path: string };

export interface ParsedArgs {
  help: boolean;
  tokenSource: TokenSource;
  options: Omit<InstallOptions, "token">;
}

export class UsageError extends Error {}

export function parseArgs(argv: string[]): ParsedArgs {
  const parsed: ParsedArgs = {
    help: false,
    tokenSource: { kind: "none" },
    options: { unit: DEFAULT_UNIT, metricsPort: null, skipK3s: false, reconfigureK3s: false, reinstallCloudflared: false, dryRun: false },
  };
  const setToken = (source: TokenSource) => {
    if (parsed.tokenSource.kind !== "none") throw new UsageError("give the token only once (--token-stdin or --token-file)");
    parsed.tokenSource = source;
  };
  for (let index = 0; index < argv.length; index++) {
    const argument = argv[index] ?? "";
    const [flag, inlineValue] = argument.startsWith("--") && argument.includes("=")
      ? [argument.slice(0, argument.indexOf("=")), argument.slice(argument.indexOf("=") + 1)]
      : [argument, undefined];
    const value = (): string => {
      const next = inlineValue ?? argv[++index];
      if (next === undefined || next === "") throw new UsageError(`${flag} needs a value`);
      return next;
    };
    switch (flag) {
      case "-h":
      case "--help":
        parsed.help = true;
        break;
      case "--token-stdin":
        setToken({ kind: "stdin" });
        break;
      case "--token-file":
        setToken({ kind: "file", path: value() });
        break;
      case "--unit":
        parsed.options.unit = value().replace(/\.service$/, "");
        break;
      case "--metrics-port": {
        const text = value();
        const port = /^\d+$/.test(text) ? Number(text) : Number.NaN;
        if (!Number.isInteger(port) || port < 1024 || port > 65535) throw new UsageError(`--metrics-port must be a port from 1024 to 65535`);
        parsed.options.metricsPort = port;
        break;
      }
      case "--skip-k3s":
        parsed.options.skipK3s = true;
        break;
      case "--reconfigure-k3s":
        parsed.options.reconfigureK3s = true;
        break;
      case "--reinstall-cloudflared":
        parsed.options.reinstallCloudflared = true;
        break;
      case "--dry-run":
        parsed.options.dryRun = true;
        break;
      case "--token":
        throw new UsageError("--token is not accepted: pass the token with --token-stdin or --token-file");
      default:
        throw new UsageError(`unknown argument ${JSON.stringify(argument)}`);
    }
  }
  return parsed;
}
