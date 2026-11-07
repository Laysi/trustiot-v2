// What the image assumes about the agent's systemd unit and account (edge/agent/systemd), checked
// without building the image: the data partition only protects the agent's state if the unit
// keeps that state where the partition is mounted.
import { describe, expect, test } from "bun:test";
import { readdirSync, readFileSync } from "node:fs";
import { join } from "node:path";
import { REPO } from "../src/host.ts";
import { AGENT_EXECUTABLES_NOT_INSTALLED, AGENT_SYSTEMD_FILES, AGENT_SYSTEMD_NOT_INSTALLED, DATA, IMAGE_PACKAGES } from "../src/pins.ts";

const SYSTEMD = join(REPO, "edge/agent/systemd");
const unit = readFileSync(join(SYSTEMD, "trustiot-agent.service"), "utf8");
const sysusers = readFileSync(join(SYSTEMD, "trustiot-agent.sysusers"), "utf8");

function values(key: string): string[] {
  return [...unit.matchAll(new RegExp(`^${key}=(.*)$`, "gm"))].map((m) => (m[1] ?? "").trim());
}

describe("agent unit", () => {
  test("keeps its state on the data partition", () => {
    // StateDirectory=<name> is /var/lib/<name> and implies RequiresMountsFor= on it, so the agent
    // does not start on the root file system when the data partition is not mounted.
    expect(values("StateDirectory")).toEqual([DATA.mountPoint.replace(/^\/var\/lib\//, "")]);
    expect(values("StateDirectoryMode")).toEqual(["0700"]);
  });

  test("runs the binary the image installs, with the configuration the factory writes", () => {
    expect(values("ExecStart")).toEqual(["/usr/bin/trustiot-agent --config /etc/trustiot/agent.toml"]);
  });

  test("starts at boot once enabled", () => {
    expect(values("WantedBy")).toEqual(["multi-user.target"]);
  });

  test("runs as the sysusers account, whose home is the data partition", () => {
    expect(values("User")).toEqual(["trustiot-agent"]);
    const lines = sysusers.split("\n").filter((l) => l.trim() !== "" && !l.startsWith("#"));
    expect(lines).toHaveLength(1);
    const [type, name, , , home, shell] = (lines[0] ?? "").match(/"[^"]*"|\S+/g) ?? [];
    expect({ type, name, home, shell }).toEqual({ type: "u", name: "trustiot-agent", home: DATA.mountPoint, shell: "/usr/sbin/nologin" });
  });

  test("the image installs every file of edge/agent/systemd", () => {
    // A new unit or rule file there is something the agent may require: list it in
    // AGENT_SYSTEMD_FILES (src/pins.ts), with what else it needs in the image, or, while the
    // build cannot install what it needs, in AGENT_SYSTEMD_NOT_INSTALLED with the issue that will.
    expect(readdirSync(SYSTEMD).toSorted()).toEqual(
      [...AGENT_SYSTEMD_FILES.map((file) => file.repo), ...AGENT_SYSTEMD_NOT_INSTALLED].toSorted(),
    );
    const installed = new Set<string>(AGENT_SYSTEMD_FILES.map((file) => file.repo));
    for (const name of AGENT_SYSTEMD_NOT_INSTALLED) expect(`${name} installed: ${installed.has(name)}`).toBe(`${name} installed: false`);
    // Vendor locations only, never /etc: units and their drop-ins, sysusers, the agent's own data.
    for (const file of AGENT_SYSTEMD_FILES) {
      expect(file.image).toMatch(/^\/usr\/(lib\/(systemd\/system(\/[a-z0-9.@-]+\.d)?|sysusers\.d)|share\/trustiot-agent)\/[a-z0-9.-]+$/);
    }
  });

  test("every file an installed unit names under /usr is installed at that path", () => {
    // The egress unit and the nftables.service drop-in name the rules file and the drop-in by
    // absolute path: installed anywhere else, the egress unit fails and the agent does not start.
    // One exception: a file of AGENT_SYSTEMD_NOT_INSTALLED that the unit only tests for
    // (`[ -e <path> ]`), as the egress check does for the default deny until
    // Laysi/trustiot-v2#866 installs it; once it is installed, the rule above applies to it again.
    const installed = new Set<string>(AGENT_SYSTEMD_FILES.map((file) => file.image));
    const notYet = new Set<string>(AGENT_SYSTEMD_NOT_INSTALLED);
    const named = AGENT_SYSTEMD_FILES.flatMap(({ repo }) => {
      const text = readFileSync(join(SYSTEMD, repo), "utf8");
      return [...text.matchAll(/\/usr\/(share\/trustiot-agent|lib\/systemd\/system\/[a-z0-9.@-]+\.d)\/[a-z0-9.-]*[a-z0-9]/g)]
        .filter((m) => {
          const tested = text.slice(Math.max(0, m.index - 5), m.index) === "[ -e " && text.startsWith(" ]", m.index + m[0].length);
          return !(tested && notYet.has(m[0].split("/").pop() ?? ""));
        })
        .map((m) => `${repo}: ${m[0]}`);
    });
    expect(named.length).toBeGreaterThan(0);
    for (const entry of named) expect(`${entry} ${installed.has(entry.split(": ")[1] ?? "")}`).toBe(`${entry} true`);
  });

  test("every executable a unit names under /usr/libexec is listed, and an installed unit runs one only behind a file not installed yet", () => {
    // The egress check runs /usr/libexec/trustiot-agent/nameservers after it loads flushed rules
    // (Laysi/trustiot-v2#878): at any other path the agent account's DNS stays refused after a
    // flush, with only an error in the journal. The image does not install the executables yet;
    // Laysi/trustiot-v2#866 will, at the paths of AGENT_EXECUTABLES_NOT_INSTALLED, together with
    // the default deny's rules file. Until then every such path a file here names is one of those,
    // and an installed unit names one only in a comment or runs it right after the check's guard
    // on the same line: `[ -e <the default deny's rules file> ] && `. Once Laysi/trustiot-v2#866
    // installs them, this test checks where the image puts them instead.
    const defaultDeny = "trustiot-agent-default-deny.nft";
    expect(AGENT_SYSTEMD_NOT_INSTALLED).toContain(defaultDeny);
    const guard = `[ -e /usr/share/trustiot-agent/${defaultDeny} ] && `;
    const listed = new Set<string>(AGENT_EXECUTABLES_NOT_INSTALLED);
    const named: string[] = [];
    const runs: string[] = [];
    const unguarded: string[] = [];
    for (const repo of readdirSync(SYSTEMD).toSorted()) {
      const text = readFileSync(join(SYSTEMD, repo), "utf8");
      const installedUnit = AGENT_SYSTEMD_FILES.some((file) => file.repo === repo);
      for (const m of text.matchAll(/\/usr\/libexec\/trustiot-agent\/[a-z0-9.-]*[a-z0-9]/g)) {
        named.push(`${repo}: ${m[0]} listed: ${listed.has(m[0])}`);
        const before = text.slice(text.lastIndexOf("\n", m.index) + 1, m.index);
        if (!installedUnit || /^\s*[#;]/.test(before)) continue;
        runs.push(`${repo}: ${m[0]}`);
        if (!before.endsWith(guard) && !before.endsWith(`${guard}! `)) unguarded.push(`${repo}: ${m[0]}`);
      }
    }
    expect(named.filter((entry) => !entry.endsWith(" listed: true"))).toEqual([]);
    // The check is what this test is for: it runs the executable, not just names it in a comment.
    expect(runs).toContain("trustiot-agent-egress-check.service: /usr/libexec/trustiot-agent/nameservers");
    expect(unguarded).toEqual([]);
  });

  test("every TrustIoT unit it depends on is installed too", () => {
    const installed = new Set(AGENT_SYSTEMD_FILES.map((file) => file.image.split("/").pop()));
    const named = ["Requires", "Requisite", "BindsTo", "Wants", "Upholds", "After"]
      .flatMap((key) => values(key))
      .flatMap((v) => v.split(/\s+/))
      .filter((u) => u.startsWith("trustiot"));
    for (const unitName of named) expect(`${unitName}: ${installed.has(unitName)}`).toBe(`${unitName}: true`);
  });

  test("its supplementary groups come from packages the image installs", () => {
    // tss is created by tpm-udev, which also gives it /dev/tpmrm0.
    for (const group of values("SupplementaryGroups").flatMap((v) => v.split(/\s+/))) {
      expect(group === "tss" && "tpm-udev" in IMAGE_PACKAGES).toBe(true);
    }
  });
});
