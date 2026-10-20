// The state the installer converges a SaaS host to. Everything a reviewer needs to judge the
// host's security posture is in this file: versions, pins, the k3s config and the systemd
// sandbox of cloudflared. deploy/host/README.md explains the reasons.
import cloudflareKey from "../assets/cloudflare-packaging.asc" with { type: "text" };

/** First line of every file the installer owns; files without it are never overwritten. */
export const MARKER = "# Managed by trustiot-host-install (deploy/host in Laysi/trustiot-v2).";

// k3s: same version as the lab and the chart's kubeVersion tests. The official installer is
// pinned to the tag of that version and verified by SHA-256 before it runs; it then downloads
// the k3s binary and verifies it against the release's sha256sum file, and installs the
// k3s-selinux policy from rpm.rancher.io.
export const K3S_VERSION = "v1.36.5+k3s1";
export const K3S_INSTALLER_URL = "https://raw.githubusercontent.com/k3s-io/k3s/v1.36.5%2Bk3s1/install.sh";
export const K3S_INSTALLER_SHA256 = "46177d4c99440b4c0311b67233823a8e8a2fc09693f6c89af1a7161e152fbfad";
export const K3S_BINARY = "/usr/local/bin/k3s";
export const K3S_CONFIG_PATH = "/etc/rancher/k3s/config.yaml";
export const K3S_CONFIG = `${MARKER}
# Product traffic reaches the Istio ingress gateway's ClusterIP through cloudflared on this
# host, so nothing in k3s may listen on the host's ports: Traefik is replaced by the Istio
# ingress (decision 27) and servicelb, which would bind host ports for every LoadBalancer
# Service, is off.
disable:
  - traefik
  - servicelb
`;

// Pod and Service networks of k3s (its defaults). firewalld trusts the pod network as a
// source, otherwise it drops pod traffic to the host (API server, kubelet) and forwarded pod
// traffic. The Service network is never a packet's source (ClusterIPs are only destinations,
// rewritten by kube-proxy), so it is not trusted: trusting it would only accept spoofed
// packets. The installer removes it from the trusted zone where an older setup added it.
export const K3S_POD_NETWORK = "10.42.0.0/16";
export const K3S_SERVICE_NETWORK = "10.43.0.0/16";
// The trusted zone accepts everything from the pods, which k3s needs (the API server, the
// kubelet, forwarding). sshd is the exception: ops SSH reaches the host only through the
// tunnel and Cloudflare Access, so pods are refused. The rule is written the way firewalld
// prints it, so --query-rich-rule finds it.
export const TRUSTED_ZONE_RICH_RULES = [
  `rule family="ipv4" source address="${K3S_POD_NETWORK}" port port="22" protocol="tcp" reject`,
];

/** Web ports the host must not open in any firewalld zone. */
export const CLOSED_SERVICES = ["http", "https", "http3"];
export const CLOSED_PORTS = ["80/tcp", "443/tcp", "443/udp"];
export const FORBIDDEN_LISTEN_PORTS = [80, 443];
export const FIREWALLD_CONFIG = "/etc/firewalld/firewalld.conf";

// cloudflared comes from Cloudflare's official RPM repository, so `dnf upgrade` updates it.
// Every package is signature-checked against Cloudflare's packaging key, which is pinned here
// by fingerprint instead of being fetched at install time. The repository metadata is not:
// Cloudflare signs repomd.xml with an older key (254B391D8CACCBF8, checked 2026-10-11) that it
// no longer publishes, so repo_gpgcheck=1 makes dnf refuse the repository. The installed
// package's own signature is verified again afterwards (checkCloudflaredProvenance).
export const CLOUDFLARE_KEY = cloudflareKey;
// Written as `gpg --fingerprint` prints it, so it can be compared by eye with gpg's output.
const CLOUDFLARE_KEY_FINGERPRINT_GROUPED = "CC94 B39C 77AE 7342 A68B  8962 8A68 2D30 8D4E 5E73";
/** The fingerprint as 40 hex digits without spaces, the form gpg's machine-readable output uses. */
export const CLOUDFLARE_KEY_FINGERPRINT = CLOUDFLARE_KEY_FINGERPRINT_GROUPED.replace(/\s+/g, "");
/** The key ID rpm prints for packages signed with that key: the fingerprint's last 16 digits. */
export const CLOUDFLARE_KEY_ID = CLOUDFLARE_KEY_FINGERPRINT.slice(-16).toLowerCase();
export const CLOUDFLARE_KEY_PATH = "/etc/pki/rpm-gpg/RPM-GPG-KEY-cloudflare-packaging";
export const REPO_DIRECTORY = "/etc/yum.repos.d";
export const CLOUDFLARED_REPO_PATH = `${REPO_DIRECTORY}/cloudflared.repo`;
export const CLOUDFLARED_REPO_ID = "cloudflared-stable";
export const CLOUDFLARED_REPO_BASEURL = "https://pkg.cloudflare.com/cloudflared/rpm";
/** Recognizes other definitions of Cloudflare's repository, which the installer refuses. */
export const CLOUDFLARE_REPO_PATTERN = /pkg\.cloudflare\.com|^\s*\[cloudflared[^\]]*\]/im;
export const CLOUDFLARED_REPO = `${MARKER}
[${CLOUDFLARED_REPO_ID}]
name=cloudflared-stable
baseurl=${CLOUDFLARED_REPO_BASEURL}
enabled=1
type=rpm
gpgcheck=1
gpgkey=file://${CLOUDFLARE_KEY_PATH}
`;
export const CLOUDFLARED_BINARY = "/usr/bin/cloudflared";

/** systemd's directory for service credentials; LoadCredential= reads from it. */
export const CREDSTORE = "/etc/credstore";
export const DEFAULT_UNIT = "cloudflared";
const UNIT_NAME = /^[a-z][a-z0-9-]{0,62}$/;

/** Highest `systemd-analyze security` exposure the cloudflared unit may have. */
export const MAX_EXPOSURE = 2.0;

export function checkUnitName(name: string): void {
  if (!UNIT_NAME.test(name)) {
    throw new Error(`invalid unit name "${name}": use lowercase letters, digits and dashes, starting with a letter`);
  }
}

export function unitPath(unit: string): string {
  return `/etc/systemd/system/${unit}.service`;
}

export function credentialPath(unit: string): string {
  return `${CREDSTORE}/${unit}.tunnel-token`;
}

/** The unit's own cloudflared config, handed to it like the token (LoadCredential). */
export function configPath(unit: string): string {
  return `${CREDSTORE}/${unit}.cloudflared.yml`;
}

/** Ports cloudflared picks for its metrics server by itself when none is set. */
export const DEFAULT_METRICS_PORTS = [20241, 20242, 20243, 20244, 20245];

export function checkMetricsPort(port: number): void {
  if (!Number.isInteger(port) || port < 1024 || port > 65535) {
    throw new Error(`invalid metrics port ${port}: use 1024 to 65535`);
  }
}

/**
 * The unit's cloudflared config. With --config, cloudflared reads only this file and never looks
 * for one in /etc/cloudflared or /usr/local/etc/cloudflared, where another connector on the same
 * host may keep its settings. The tunnel's routes stay in Cloudflare. Without a metrics port,
 * cloudflared picks the first free one of 20241 to 20245 on loopback, as before.
 */
export function renderConfig(unit: string, metricsPort: number | null): string {
  checkUnitName(unit);
  if (metricsPort !== null) checkMetricsPort(metricsPort);
  return `${MARKER}
# cloudflared settings of ${unit}.service; routes are set in Cloudflare (remotely managed tunnel).
no-autoupdate: true
${metricsPort === null ? "" : `metrics: 127.0.0.1:${metricsPort}\n`}`;
}

/**
 * The cloudflared unit. The tunnel is remotely managed: its routes (the ingress gateway's
 * ClusterIP for product hostnames, ssh://localhost:22 for ops SSH behind Cloudflare Access) are
 * set in Cloudflare, and the token is the only local secret. systemd reads the root-only token
 * and config files and hands them to the service in a private credentials directory (%d); the
 * service itself runs as a throwaway user that can read nothing else.
 */
export function renderUnit(unit: string): string {
  checkUnitName(unit);
  return `${MARKER}
# Changes are overwritten on the next run of the installer; see deploy/host/README.md.
[Unit]
Description=Cloudflare Tunnel connector for TrustIoT (${unit})
Documentation=https://github.com/Laysi/trustiot-v2/blob/main/deploy/host/README.md
After=network-online.target
Wants=network-online.target
# Keep retrying through long network outages instead of giving up after five failed starts.
StartLimitIntervalSec=0

[Service]
# cloudflared reports readiness once its first connection to Cloudflare is registered.
Type=notify
ExecStart=${CLOUDFLARED_BINARY} --no-autoupdate tunnel --config %d/cloudflared-config run --token-file %d/tunnel-token
LoadCredential=tunnel-token:${credentialPath(unit)}
LoadCredential=cloudflared-config:${configPath(unit)}
Restart=always
RestartSec=5s
# cloudflared waits 30 seconds for open requests before it exits on SIGTERM.
TimeoutStopSec=60s

# Sandbox: a transient unprivileged user without capabilities, a read-only OS, no access to
# home directories, devices, other processes or kernel settings, and only the system calls and
# socket families a network daemon needs. The ICMP proxy needs raw sockets and stays off.
DynamicUser=yes
CapabilityBoundingSet=
AmbientCapabilities=
NoNewPrivileges=yes
ProtectSystem=strict
ProtectHome=yes
PrivateTmp=yes
PrivateDevices=yes
PrivateUsers=yes
PrivateIPC=yes
ProtectKernelTunables=yes
ProtectKernelModules=yes
ProtectKernelLogs=yes
ProtectControlGroups=yes
ProtectClock=yes
ProtectHostname=yes
ProtectProc=invisible
ProcSubset=pid
RestrictAddressFamilies=AF_INET AF_INET6 AF_UNIX AF_NETLINK
RestrictNamespaces=yes
RestrictRealtime=yes
RestrictSUIDSGID=yes
LockPersonality=yes
MemoryDenyWriteExecute=yes
SystemCallArchitectures=native
SystemCallFilter=@system-service
SystemCallFilter=~@privileged @resources
SystemCallErrorNumber=EPERM
UMask=0077
DevicePolicy=closed
KeyringMode=private
RemoveIPC=yes

[Install]
WantedBy=multi-user.target
`;
}
