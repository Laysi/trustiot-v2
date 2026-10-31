// What the image assumes about the agent's systemd unit and account (edge/agent/systemd), checked
// without building the image: the data partition only protects the agent's state if the unit
// keeps that state where the partition is mounted.
import { describe, expect, test } from "bun:test";
import { readdirSync, readFileSync } from "node:fs";
import { join } from "node:path";
import { REPO } from "../src/host.ts";
import { AGENT_SYSTEMD_FILES, AGENT_SYSTEMD_NOT_INSTALLED, DATA, IMAGE_PACKAGES } from "../src/pins.ts";

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
    const installed = new Set<string>(AGENT_SYSTEMD_FILES.map((file) => file.image));
    const named = AGENT_SYSTEMD_FILES.flatMap(({ repo }) =>
      [...readFileSync(join(SYSTEMD, repo), "utf8").matchAll(/\/usr\/(share\/trustiot-agent|lib\/systemd\/system\/[a-z0-9.@-]+\.d)\/[a-z0-9.-]*[a-z0-9]/g)].map(
        (m) => `${repo}: ${m[0]}`,
      ),
    );
    expect(named.length).toBeGreaterThan(0);
    for (const entry of named) expect(`${entry} ${installed.has(entry.split(": ")[1] ?? "")}`).toBe(`${entry} true`);
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
