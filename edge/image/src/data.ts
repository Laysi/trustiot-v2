// The data partition's encryption (Laysi/trustiot-v2#810): what the image check and the boot test
// expect of the files in files/data, the LUKS2 header the gateway makes on its first boot, and
// the parsers they read them back with. Pure functions only, so the unit tests cover them
// without an image (README.md, section 「資料分割區加密」).
import { LUKS } from "./pins.ts";

export interface CrypttabEntry {
  name: string;
  device: string;
  key: string;
  options: string[];
}

export function parseCrypttab(text: string): CrypttabEntry[] {
  const entries: CrypttabEntry[] = [];
  for (const raw of text.split("\n")) {
    const line = raw.trim();
    if (line === "" || line.startsWith("#")) continue;
    const [name = "", device = "", key = "", options = ""] = line.split(/\s+/);
    entries.push({ name, device, key, options: options === "" ? [] : options.split(",") });
  }
  return entries;
}

/** The crypttab options the volume is unlocked with: the TPM alone, never a password prompt. */
export const CRYPTTAB_OPTIONS = ["luks", "tpm2-device=auto", "headless=true", "nofail"] as const;

/** Why /etc/crypttab would not unlock the data volume as README.md describes, if it would not. */
export function crypttabProblems(text: string): string[] {
  const entries = parseCrypttab(text);
  const problems: string[] = [];
  if (entries.length !== 1) problems.push(`/etc/crypttab has ${entries.length} entries, expected only ${LUKS.mapping}`);
  const entry = entries.find((e) => e.name === LUKS.mapping);
  if (!entry) return [...problems, `/etc/crypttab has no ${LUKS.mapping}`];
  if (entry.device !== `UUID=${LUKS.uuid}`) problems.push(`${LUKS.mapping} is ${entry.device}, expected UUID=${LUKS.uuid}`);
  if (entry.key !== "none") problems.push(`${LUKS.mapping} has the key file ${entry.key}, expected none`);
  if (entry.options.join(",") !== CRYPTTAB_OPTIONS.join(",")) problems.push(`${LUKS.mapping} options ${entry.options.join(",")}, expected ${CRYPTTAB_OPTIONS.join(",")}`);
  return problems;
}

/**
 * Why the unit systemd-cryptsetup-generator makes from /etc/crypttab differs from what the drop-in
 * and README.md assume, if it does: it attaches the volume by its UUID with the crypttab options.
 */
export function generatedUnitProblems(unit: string): string[] {
  const problems: string[] = [];
  const execStart = [...unit.matchAll(/^ExecStart=(.*)$/gm)].map((m) => m[1] ?? "");
  const want = `systemd-cryptsetup attach '${LUKS.mapping}' '/dev/disk/by-uuid/${LUKS.uuid}' 'none' '${CRYPTTAB_OPTIONS.join(",")}'`;
  if (execStart.length !== 1 || !execStart[0]?.endsWith(want)) problems.push(`ExecStart=${execStart.join(" | ")}, expected .../${want}`);
  if (!/^DefaultDependencies=no$/m.test(unit)) problems.push("the generated unit has default dependencies");
  return problems;
}

/** The last value of `key` in `section` across `systemd-analyze cat-config` output (the one in force). */
export function effectiveSetting(catConfig: string, section: string, key: string): string | undefined {
  let current = "";
  let value: string | undefined;
  for (const raw of catConfig.split("\n")) {
    const line = raw.trim();
    if (line === "" || line.startsWith("#") || line.startsWith(";")) continue;
    if (line.startsWith("[")) {
      current = line;
      continue;
    }
    if (current !== `[${section}]`) continue;
    const eq = line.indexOf("=");
    if (eq >= 0 && line.slice(0, eq).trim() === key) value = line.slice(eq + 1).trim();
  }
  return value;
}

/** Keyslots and the token type that uses each, from `systemd-cryptenroll <device>`. */
export function parseEnrollList(text: string): { slot: number; type: string }[] {
  const lines = text.split("\n").filter((l) => l.trim() !== "");
  if (!/^\s*SLOT\s+TYPE\s*$/.test(lines[0] ?? "")) throw new Error(`systemd-cryptenroll printed no SLOT TYPE header: ${JSON.stringify(text.slice(0, 200))}`);
  return lines.slice(1).map((line) => {
    const [slot = "", type = ""] = line.trim().split(/\s+/);
    if (!/^\d+$/.test(slot) || type === "") throw new Error(`unexpected systemd-cryptenroll line ${JSON.stringify(line)}`);
    return { slot: Number(slot), type };
  });
}

interface LuksMetadata {
  keyslots?: Record<string, { type?: string }>;
  tokens?: Record<string, { type?: string; keyslots?: string[]; "tpm2-pcrs"?: unknown; "tpm2-pin"?: unknown; "tpm2-pcrlock"?: unknown }>;
  segments?: Record<string, { type?: string; encryption?: string; sector_size?: number }>;
}

/**
 * Why a LUKS2 header (`cryptsetup luksDump --dump-json-metadata`) is not the one the setup leaves,
 * if it is not: exactly one keyslot, used by exactly one token, systemd's TPM2 token, sealed
 * without PCRs, PIN or pcrlock policy, so no password or other key opens the volume; and the
 * cipher and sector size of LUKS (src/pins.ts).
 */
export function luksHeaderProblems(json: string): string[] {
  const meta = JSON.parse(json) as LuksMetadata;
  const problems: string[] = [];
  const keyslots = Object.keys(meta.keyslots ?? {});
  const tokens = Object.entries(meta.tokens ?? {});
  if (keyslots.length !== 1) problems.push(`${keyslots.length} keyslots (${keyslots.join(", ")}), expected exactly one`);
  if (tokens.length !== 1) problems.push(`${tokens.length} tokens (${tokens.map(([id, t]) => `${id}: ${t.type}`).join(", ")}), expected exactly one`);
  const [, token] = tokens[0] ?? [];
  if (token) {
    if (token.type !== "systemd-tpm2") problems.push(`the token is ${token.type}, expected systemd-tpm2`);
    if (JSON.stringify(token.keyslots) !== JSON.stringify(keyslots)) problems.push(`the token uses keyslots ${JSON.stringify(token.keyslots)}, the header has ${JSON.stringify(keyslots)}`);
    if (!Array.isArray(token["tpm2-pcrs"]) || token["tpm2-pcrs"].length !== 0) problems.push(`the token is bound to PCRs ${JSON.stringify(token["tpm2-pcrs"])}`);
    if (token["tpm2-pin"] === true) problems.push("the token needs a PIN");
    if (token["tpm2-pcrlock"] === true) problems.push("the token uses a pcrlock policy");
  }
  const segments = Object.values(meta.segments ?? {});
  if (segments.length !== 1 || segments[0]?.encryption !== LUKS.cipher || segments[0]?.sector_size !== LUKS.sectorBytes) {
    problems.push(`data segments ${JSON.stringify(segments)}, expected one ${LUKS.cipher} segment with ${LUKS.sectorBytes}-byte sectors`);
  }
  return problems;
}

/**
 * `NAME=value` lines as rpi-systemd-config prints them (the effective rpi-swap settings, read
 * with systemd's own drop-in rules); a value may be in single quotes.
 */
export function parseShellAssignments(text: string): Map<string, string> {
  const values = new Map<string, string>();
  for (const line of text.split("\n")) {
    if (line.trim() === "") continue;
    const match = /^([A-Z_][A-Z0-9_]*)=(?:'([^']*)'|([^\s'"`$\\;]*))$/.exec(line.trim());
    if (!match) throw new Error(`unexpected line from rpi-systemd-config: ${JSON.stringify(line)}`);
    values.set(match[1] ?? "", match[2] ?? match[3] ?? "");
  }
  return values;
}

/** The `readonly NAME=value` lines of files/data/trustiot-data-setup. */
export function scriptConstants(script: string): Map<string, string> {
  return new Map([...script.matchAll(/^readonly ([A-Z_]+)=(\S+)$/gm)].map((m) => [m[1] ?? "", m[2] ?? ""]));
}

/**
 * Installed packages (`dpkg-query -W -f '${Package} ${db:Status-Abbrev} ${Provides}\n'`) that
 * would keep a second copy of the log on disk: a syslog daemon (README.md, section 「日誌與置換空間」).
 */
export function syslogDaemons(dpkgQuery: string): string[] {
  return dpkgQuery
    .split("\n")
    .map((line) => line.trim().split(/\s+/))
    .filter(([, status = ""]) => status.startsWith("ii"))
    .filter(([name = "", , ...provides]) => /^(rsyslog|syslog-ng.*|busybox-syslogd|inetutils-syslogd|sysklogd)$/.test(name) || provides.join(" ").split(/,\s*|\s+/).includes("system-log-daemon"))
    .map(([name = ""]) => name);
}
