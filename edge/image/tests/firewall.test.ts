// The agent's firewall as the image wires it (src/firewall.ts; Laysi/trustiot-v2#866): the shipped
// units and rules pass, and each way the wiring could come apart is caught.
import { describe, expect, test } from "bun:test";
import { readFileSync } from "node:fs";
import { join } from "node:path";
import { AGENT_EXEC_START_PRE, DEFAULT_DENY, IP_DENY_CHECK, NAMESERVERS, RULES, effectiveList, firewallWiringProblems } from "../src/firewall.ts";
import { REPO } from "../src/host.ts";
import { AGENT_EXECUTABLES, AGENT_SYSTEMD_FILES, DATA_FILES } from "../src/pins.ts";

const read = (path: string) => readFileSync(join(REPO, path), "utf8");
const systemd = (name: string) => read(`edge/agent/systemd/${name}`);
const installedAt = (repo: string) => AGENT_SYSTEMD_FILES.find((f) => f.repo === repo)?.image;

/** What `systemd-analyze cat-config` prints: each file after a comment naming it. */
const catConfig = (...files: [string, string][]) => files.map(([path, text]) => `# ${path}\n${text}`).join("\n");

const dataDropIn = DATA_FILES.find((f) => f.image.startsWith("/usr/lib/systemd/system/trustiot-agent.service.d/"));
const shipped = {
  agent: catConfig(
    ["/usr/lib/systemd/system/trustiot-agent.service", systemd("trustiot-agent.service")],
    [dataDropIn?.image ?? "", read(`edge/image/files/data/${dataDropIn?.repo}`)],
  ),
  egress: catConfig(["/usr/lib/systemd/system/trustiot-agent-egress.service", systemd("trustiot-agent-egress.service")]),
  nftables: catConfig(
    ["/usr/lib/systemd/system/nftables.service", "[Service]\nType=oneshot\nExecStart=/usr/sbin/nft -f /etc/nftables.conf\nExecReload=/usr/sbin/nft -f /etc/nftables.conf\n"],
    [installedAt("nftables-reload.conf") ?? "", systemd("nftables-reload.conf")],
  ),
  rules: systemd("trustiot-agent-egress.nft"),
};

describe("the agent's firewall wiring", () => {
  test("the paths it names are where the image installs the files and executables", () => {
    expect(installedAt("trustiot-agent-egress.nft")).toBe(RULES);
    expect(installedAt("trustiot-agent-default-deny.nft")).toBe(DEFAULT_DENY);
    const executables = AGENT_EXECUTABLES.map((e) => e.image as string);
    for (const path of [NAMESERVERS, IP_DENY_CHECK]) expect(executables).toContain(path);
  });

  test("the shipped units and rules, with the image's drop-ins, pass", () => {
    expect(firewallWiringProblems(shipped)).toEqual([]);
  });

  test("a list setting adds up across the unit and its drop-ins, and an empty assignment empties it", () => {
    const text = "[Unit]\nWants=a b\n[Service]\nExecStartPre=x\n# /etc/x.d/y.conf\n[Unit]\nWants=c\n[Service]\nExecStartPre=\nExecStartPre=y\n";
    expect(effectiveList(text, "Unit", "Wants", true)).toEqual(["a", "b", "c"]);
    expect(effectiveList(text, "Service", "ExecStartPre")).toEqual(["y"]);
    expect(effectiveList(text, "Service", "Wants")).toEqual([]);
  });

  test("fails when a drop-in resets or reorders the agent's ExecStartPre= lines", () => {
    const reset = { ...shipped, agent: `${shipped.agent}\n# /etc/systemd/system/trustiot-agent.service.d/x.conf\n[Service]\nExecStartPre=\nExecStartPre=+/usr/sbin/nft -f ${RULES}\n` };
    expect(firewallWiringProblems(reset).join()).toContain("trustiot-agent.service's ExecStartPre=");
    const strict = { ...shipped, agent: shipped.agent.replace(`ExecStartPre=-+${IP_DENY_CHECK}`, `ExecStartPre=+${IP_DENY_CHECK}`) };
    expect(firewallWiringProblems(strict).join()).toContain(JSON.stringify(AGENT_EXEC_START_PRE));
  });

  test("fails when the agent does not pull in the socket or the path unit, or requires them", () => {
    const without = { ...shipped, agent: shipped.agent.replace("Wants=trustiot-agent-allowlist.socket\n", "") };
    expect(firewallWiringProblems(without)).toEqual(["trustiot-agent.service has no Wants=trustiot-agent-allowlist.socket"]);
    const unordered = { ...shipped, agent: shipped.agent.replace("After=trustiot-agent-nameservers.path\n", "") };
    expect(firewallWiringProblems(unordered)).toEqual(["trustiot-agent.service has no After=trustiot-agent-nameservers.path"]);
    const required = { ...shipped, agent: `${shipped.agent}\n[Unit]\nRequires=trustiot-agent-allowlist.socket\n` };
    expect(firewallWiringProblems(required)).toEqual(["trustiot-agent.service has Requires=trustiot-agent-allowlist.socket, expected only Wants="]);
  });

  test("fails when the reload of nftables.service loads the files apart, or does not refill the DNS allowance", () => {
    const noRefill = { ...shipped, nftables: shipped.nftables.replace(`ExecReload=${NAMESERVERS}\n`, "") };
    expect(firewallWiringProblems(noRefill).join()).toContain("nftables.service's ExecReload=");
    const overridden = { ...shipped, nftables: `${shipped.nftables}\n# /etc/systemd/system/nftables.service.d/zz.conf\n[Service]\nExecReload=\nExecReload=/usr/sbin/nft -f /etc/nftables.conf\n` };
    expect(firewallWiringProblems(overridden).join()).toContain("nftables.service's ExecReload=");
  });

  test("fails when the rules do not include the default deny last, or include something else", () => {
    const include = `include "${DEFAULT_DENY}"`;
    expect(shipped.rules).toContain(include);
    const without = { ...shipped, rules: shipped.rules.replace(include, `# ${include}`) };
    expect(firewallWiringProblems(without).join()).toContain(`does not end with ${include}`);
    const relative = { ...shipped, rules: shipped.rules.replace(include, 'include "trustiot-agent-default-deny.nft"') };
    expect(firewallWiringProblems(relative).join()).toContain(`does not end with ${include}`);
    const another = { ...shipped, rules: `include "/etc/other.nft"\n${shipped.rules}` };
    expect(firewallWiringProblems(another).join()).toContain("2 include statements");
  });

  test("fails when the egress unit loads another file", () => {
    const other = { ...shipped, egress: shipped.egress.replace(`ExecStart=/usr/sbin/nft -f ${RULES}`, "ExecStart=/usr/sbin/nft -f /etc/other.nft") };
    expect(firewallWiringProblems(other).join()).toContain("trustiot-agent-egress.service's ExecStart=");
  });
});
