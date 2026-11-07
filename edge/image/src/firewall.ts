// The agent's firewall as the image wires it (Laysi/trustiot-v2#866; edge/agent/README.md,
// 「給 Laysi/trustiot-v2#866 的接法」), read from the image's own files without booting it: the
// image check (src/inside/check.ts) runs these on `systemd-analyze cat-config` output, so a
// drop-in in the image that resets a line counts as well. Pure functions, so the unit tests cover
// them without an image.

/** Where the image installs the egress rules and the default deny they include (AGENT_SYSTEMD_FILES). */
export const RULES = "/usr/share/trustiot-agent/trustiot-agent-egress.nft";
export const DEFAULT_DENY = "/usr/share/trustiot-agent/trustiot-agent-default-deny.nft";
export const NAMESERVERS = "/usr/libexec/trustiot-agent/nameservers";
export const IP_DENY_CHECK = "/usr/libexec/trustiot-agent/ip-deny-check";

/** The units the agent's unit pulls in with Wants= and orders itself after (neither has [Install]). */
export const PULLED_IN = ["trustiot-agent-nameservers.path", "trustiot-agent-allowlist.socket"] as const;

/**
 * The agent's ExecStartPre= lines, in this order: load the rules (both tables, one transaction),
 * fill the default deny's DNS allowance, then confirm IPAddressDeny= (`-`: a failure is logged and
 * the agent starts anyway; README.md, section 「`IPAddressDeny=` 沒有掛上時」).
 */
export const AGENT_EXEC_START_PRE = [`+/usr/sbin/nft -f ${RULES}`, `+${NAMESERVERS}`, `-+${IP_DENY_CHECK}`] as const;

/** nftables.service's ExecReload= with the agent's drop-in: one transaction for both files, then the DNS allowance. */
export const NFTABLES_EXEC_RELOAD = [`/usr/sbin/nft 'include "/etc/nftables.conf"; include "${RULES}"'`, NAMESERVERS] as const;

/** The egress unit's ExecStart=, which loads the rules before the agent's first start. */
export const EGRESS_EXEC_START = [`/usr/sbin/nft -f ${RULES}`] as const;

/** The agent unit's IPAddressDeny= as edge/agent/systemd/trustiot-agent.service ships it. */
export const AGENT_IP_ADDRESS_DENY = ["localhost", "0.0.0.0/8", "multicast", "255.255.255.255/32", "169.254.169.254/32", "fd00:ec2::254/128"] as const;

/**
 * The agent unit's settings, besides IPAddressDeny=, that put BPF programs on its cgroup's ingress
 * and egress hooks, or (IPAddressAllow=) let through what IPAddressDeny= refuses: none of them may
 * be set. ip-deny-check counts the programs on those two hooks, so only IPAddressDeny= may put
 * them there for its count to confirm IPAddressDeny=.
 */
export const AGENT_IP_SETTINGS_UNSET = ["IPAddressAllow", "IPIngressFilterPath", "IPEgressFilterPath", "RestrictNetworkInterfaces"] as const;

/** systemd's boolean parsing (parse_boolean): what counts as true. */
function isTrue(value: string): boolean {
  return /^(1|yes|y|true|t|on)$/i.test(value);
}

/**
 * The values a list setting (ExecStartPre=, Wants=, After=, ...) of one section ends up with, from
 * `systemd-analyze cat-config` output: every assignment adds to the list, an empty one empties
 * it. Dependency settings hold several unit names per line; pass `split` to get them one by one.
 */
export function effectiveList(catConfig: string, section: string, key: string, split = false): string[] {
  let current = "";
  let values: string[] = [];
  for (const raw of catConfig.split("\n")) {
    const line = raw.trim();
    if (line === "" || line.startsWith("#") || line.startsWith(";")) continue;
    if (line.startsWith("[")) {
      current = line;
      continue;
    }
    if (current !== `[${section}]`) continue;
    const eq = line.indexOf("=");
    if (eq < 0 || line.slice(0, eq).trim() !== key) continue;
    const value = line.slice(eq + 1).trim();
    if (value === "") values = [];
    else values.push(...(split ? value.split(/\s+/) : [value]));
  }
  return values;
}

/** The value a single-value setting of one section ends up with (the last assignment), "" when unset. */
export function effectiveValue(catConfig: string, section: string, key: string): string {
  return effectiveList(catConfig, section, key).at(-1) ?? "";
}

/**
 * Why the agent's cgroup could carry ingress or egress programs other than its IPAddressDeny='s,
 * or IPAddressDeny= not be the shipped list, if it could: `agent` is the agent unit's
 * `systemd-analyze cat-config` output, `systemConf` the one of systemd/system.conf.
 */
export function ipAddressDenyProblems(agent: string, systemConf: string): string[] {
  const problems: string[] = [];
  const deny = effectiveList(agent, "Service", "IPAddressDeny", true);
  if (JSON.stringify(deny) !== JSON.stringify(AGENT_IP_ADDRESS_DENY)) problems.push(`trustiot-agent.service's IPAddressDeny= is ${JSON.stringify(deny)}, expected ${JSON.stringify(AGENT_IP_ADDRESS_DENY)}`);
  for (const key of AGENT_IP_SETTINGS_UNSET) {
    const values = effectiveList(agent, "Service", key, true);
    if (values.length > 0) problems.push(`trustiot-agent.service sets ${key}=${values.join(" ")}, which IPAddressDeny='s check cannot tell from IPAddressDeny=`);
  }
  const accounting = effectiveValue(agent, "Service", "IPAccounting");
  if (isTrue(accounting)) problems.push(`trustiot-agent.service sets IPAccounting=${accounting}, which IPAddressDeny='s check cannot tell from IPAddressDeny=`);
  const fallback = effectiveValue(systemConf, "Manager", "DefaultIPAccounting");
  if (accounting === "" && isTrue(fallback)) problems.push(`system.conf sets DefaultIPAccounting=${fallback}, which IPAddressDeny='s check cannot tell from IPAddressDeny=`);
  return problems;
}

/** The nftables statements of a rules file: comments and blank lines left out. */
function statements(rules: string): string[] {
  return rules
    .split("\n")
    .map((l) => l.replace(/#.*$/, "").trim())
    .filter((l) => l !== "");
}

export interface FirewallFiles {
  /** `systemd-analyze cat-config systemd/system/trustiot-agent.service` in the image. */
  agent: string;
  /** `systemd-analyze cat-config systemd/system.conf` in the image. */
  systemConf: string;
  /** The same for trustiot-agent-egress.service. */
  egress: string;
  /** The same for nftables.service, with the agent's drop-in. */
  nftables: string;
  /** The installed rules file (RULES). */
  rules: string;
}

/**
 * Why the image's units would not load the default deny with the egress rules on each of the
 * three paths (the egress unit's start, the agent's start, the reload of nftables.service), fill
 * the DNS allowance after each load, pull in the allowlist component's socket and the nameservers'
 * path unit, or confirm IPAddressDeny= (as shipped, and the only setting that puts programs where
 * ip-deny-check counts them), if they would not.
 */
export function firewallWiringProblems(files: FirewallFiles): string[] {
  const problems: string[] = [];
  const same = (what: string, have: readonly string[], want: readonly string[]) => {
    if (JSON.stringify(have) !== JSON.stringify(want)) problems.push(`${what} is ${JSON.stringify(have)}, expected ${JSON.stringify(want)}`);
  };
  same("trustiot-agent.service's ExecStartPre=", effectiveList(files.agent, "Service", "ExecStartPre"), AGENT_EXEC_START_PRE);
  same("trustiot-agent-egress.service's ExecStart=", effectiveList(files.egress, "Service", "ExecStart"), EGRESS_EXEC_START);
  same("nftables.service's ExecReload=", effectiveList(files.nftables, "Service", "ExecReload"), NFTABLES_EXEC_RELOAD);
  for (const key of ["Wants", "After"]) {
    const have = effectiveList(files.agent, "Unit", key, true);
    for (const unit of PULLED_IN) if (!have.includes(unit)) problems.push(`trustiot-agent.service has no ${key}=${unit}`);
  }
  // Wants=, not Requires=: a failed socket or path unit must not keep the agent, and with it the
  // cloud connection, from starting (edge/agent/README.md, 「給 Laysi/trustiot-v2#866 的接法」).
  for (const key of ["Requires", "Requisite", "BindsTo", "PartOf", "Upholds"]) {
    for (const unit of PULLED_IN) if (effectiveList(files.agent, "Unit", key, true).includes(unit)) problems.push(`trustiot-agent.service has ${key}=${unit}, expected only Wants=`);
  }
  problems.push(...ipAddressDenyProblems(files.agent, files.systemConf));
  const include = `include "${DEFAULT_DENY}"`;
  const lines = statements(files.rules);
  if (lines.at(-1) !== include) problems.push(`${RULES} does not end with ${include}`);
  if (lines.filter((l) => l.startsWith("include ")).length !== 1) problems.push(`${RULES} has ${lines.filter((l) => l.startsWith("include ")).length} include statements, expected only ${include}`);
  return problems;
}
