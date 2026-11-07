// What the image assumes about the agent's systemd unit and account (edge/agent/systemd), checked
// without building the image: the data partition only protects the agent's state if the unit
// keeps that state where the partition is mounted.
import { describe, expect, test } from "bun:test";
import { readdirSync, readFileSync } from "node:fs";
import { join } from "node:path";
import { REPO } from "../src/host.ts";
import { AGENT_EXECUTABLES, AGENT_SYSTEMD_FILES, AGENT_SYSTEMD_NOT_INSTALLED, DATA, IMAGE_PACKAGES } from "../src/pins.ts";

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
    // build cannot install what it needs, in AGENT_SYSTEMD_NOT_INSTALLED with the issue that will
    // (empty since Laysi/trustiot-v2#866).
    expect(readdirSync(SYSTEMD).toSorted()).toEqual(
      [...AGENT_SYSTEMD_FILES.map((file) => file.repo), ...AGENT_SYSTEMD_NOT_INSTALLED].toSorted(),
    );
    const installed = new Set<string>(AGENT_SYSTEMD_FILES.map((file) => file.repo));
    for (const name of AGENT_SYSTEMD_NOT_INSTALLED) expect(`${name} installed: ${installed.has(name)}`).toBe(`${name} installed: false`);
    // Vendor locations only, never /etc: units and their drop-ins, sysusers, the agent's own data.
    for (const file of AGENT_SYSTEMD_FILES) {
      expect(file.image).toMatch(/^\/usr\/(lib\/(systemd\/system(\/[a-z0-9.@-]+\.d)?|sysusers\.d)|share\/trustiot-agent)\/[a-z0-9.@-]+$/);
    }
  });

  test("every file an installed unit names under /usr is installed at that path", () => {
    // The egress unit and the nftables.service drop-in name the rules file and the drop-in by
    // absolute path, the rules include the default deny by its path, and the units run the
    // agent's other executables by theirs: installed anywhere else, a unit fails or the agent's
    // firewall comes apart. One exception: a file of AGENT_SYSTEMD_NOT_INSTALLED that the unit
    // only tests for (`[ -e <path> ]`), as the egress check did for the default deny before
    // Laysi/trustiot-v2#866 installed it; once it is installed, the rule applies to it again.
    const installed = new Set<string>([...AGENT_SYSTEMD_FILES.map((file) => file.image), ...AGENT_EXECUTABLES.map((e) => e.image)]);
    const notYet = new Set<string>(AGENT_SYSTEMD_NOT_INSTALLED);
    const named = AGENT_SYSTEMD_FILES.flatMap(({ repo }) => {
      const text = readFileSync(join(SYSTEMD, repo), "utf8");
      return [...text.matchAll(/\/usr\/(share\/trustiot-agent|libexec\/trustiot-agent|lib\/systemd\/system\/[a-z0-9.@-]+\.d)\/[a-z0-9.-]*[a-z0-9]/g)]
        .filter((m) => {
          const tested = text.slice(Math.max(0, m.index - 5), m.index) === "[ -e " && text.startsWith(" ]", m.index + m[0].length);
          return !(tested && notYet.has(m[0].split("/").pop() ?? ""));
        })
        .map((m) => `${repo}: ${m[0]}`);
    });
    expect(named.length).toBeGreaterThan(0);
    for (const entry of named) expect(`${entry} ${installed.has(entry.split(": ")[1] ?? "")}`).toBe(`${entry} true`);
    // The ones Laysi/trustiot-v2#866 wires up are among them.
    for (const path of ["/usr/share/trustiot-agent/trustiot-agent-default-deny.nft", ...AGENT_EXECUTABLES.slice(1).map((e) => e.image)]) {
      expect(named.some((entry) => entry.endsWith(`: ${path}`))).toBe(true);
    }
  });

  test("the image installs every executable of the agent's package", () => {
    // A new [[bin]] in edge/agent/Cargo.toml is something a unit may run: list it in
    // AGENT_EXECUTABLES (src/pins.ts), so the build takes it from the cargo build and installs it.
    const cargo = readFileSync(join(REPO, "edge/agent/Cargo.toml"), "utf8");
    const bins = [...cargo.matchAll(/^\[\[bin\]\]\s*\nname = "([^"]+)"/gm)].map((m) => m[1]);
    expect(bins.toSorted()).toEqual(AGENT_EXECUTABLES.map((e) => e.build).toSorted());
    for (const { image } of AGENT_EXECUTABLES) expect(image).toMatch(/^\/usr\/(bin\/trustiot-agent|libexec\/trustiot-agent\/[a-z0-9-]+)$/);
  });

  test("the units run the executables under /usr/libexec, not just name them", () => {
    // The egress check runs /usr/libexec/trustiot-agent/nameservers after it loads flushed rules
    // (Laysi/trustiot-v2#878), and so do the reload of nftables.service and the agent's start
    // (Laysi/trustiot-v2#866), which also runs the check of IPAddressDeny=. Named only in a
    // comment, or behind a typo, the agent account's DNS stays refused after a flush or a reload,
    // or the check never runs, with no failing test (Laysi/trustiot-v2#1188). The test above holds
    // every path a unit names to where the image installs it.
    const runs = AGENT_SYSTEMD_FILES.flatMap(({ repo }) => {
      const text = readFileSync(join(SYSTEMD, repo), "utf8");
      return [...text.matchAll(/\/usr\/libexec\/trustiot-agent\/[a-z0-9.-]*[a-z0-9]/g)]
        .filter((m) => !/^\s*[#;]/.test(text.slice(text.lastIndexOf("\n", m.index) + 1, m.index)))
        .map((m) => `${repo}: ${m[0]}`);
    });
    for (const entry of [
      "nftables-reload.conf: /usr/libexec/trustiot-agent/nameservers",
      "trustiot-agent-allowlist@.service: /usr/libexec/trustiot-agent/allowlist",
      "trustiot-agent-egress-check.service: /usr/libexec/trustiot-agent/nameservers",
      "trustiot-agent-nameservers.service: /usr/libexec/trustiot-agent/nameservers",
      "trustiot-agent.service: /usr/libexec/trustiot-agent/ip-deny-check",
      "trustiot-agent.service: /usr/libexec/trustiot-agent/nameservers",
    ]) {
      expect(runs).toContain(entry);
    }
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
