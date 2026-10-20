// The remote commands of deploy/flux/README.md 「展示叢集的 age 金鑰」, run as written with bash
// against a stand-in for `sudo k3s kubectl` and age-keygen: every failure stops before the next
// step, and none leaves an empty or broken sops-age, or the age-keygen binary, behind. Run with
// `bun test deploy/flux`; needs bash, sha256sum and base64.
import { afterAll, describe, expect, test } from "bun:test";
import { chmodSync, existsSync, mkdirSync, mkdtempSync, readdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

const README = readFileSync(join(import.meta.dir, "..", "README.md"), "utf8");
/** Bounds one bash run; it takes well under a second, but the machine may be loaded. */
const SCENARIO_TIMEOUT_MS = 60_000;
const ISSUE = "464";

/** The two remote blocks: the upload of age-keygen and the key generation. */
function remoteBlocks(): { upload: string; keygen: string } {
  const section = README.slice(README.indexOf("### 展示叢集的 age 金鑰"), README.indexOf("### 日常變更"));
  const blocks = [...section.matchAll(/ssh <展示機> '(set -eu\n[\s\S]*?)'(?: < <age-keygen 的路徑>)?\n/g)].map((m) => m[1] ?? "");
  if (blocks.length !== 2) throw new Error(`expected the upload and the key generation blocks, found ${blocks.length}`);
  return { upload: blocks[0] ?? "", keygen: blocks[1] ?? "" };
}

/** `sudo k3s kubectl` for the commands the blocks run; the state is a directory per namespace. */
const FAKE_SUDO = `#!/bin/bash
[ "$1" = k3s ] && [ "$2" = kubectl ] || { echo "unexpected: sudo $*" >&2; exit 99; }
shift 2
S="$FAKE_STATE"
echo "$*" >> "$S/calls"
case "$*" in
  "create namespace flux-system")
    if [ -d "$S/ns" ]; then echo 'Error from server (AlreadyExists): namespaces "flux-system" already exists' >&2; exit 1; fi
    mkdir "$S/ns"; echo "namespace/flux-system created";;
  "-n flux-system create secret generic sops-age --from-file=age.agekey=/dev/stdin")
    [ -d "$S/ns" ] || { echo 'Error from server (NotFound): namespaces "flux-system" not found' >&2; exit 1; }
    [ -e "$S/ns/sops-age" ] && { echo 'Error from server (AlreadyExists): secrets "sops-age" already exists' >&2; exit 1; }
    cat > "$S/ns/sops-age"; echo "secret/sops-age created";;
  "-n flux-system get secret sops-age -o jsonpath={.data.age\\.agekey}")
    [ -e "$S/ns/sops-age" ] || { echo 'Error from server (NotFound): secrets "sops-age" not found' >&2; exit 1; }
    base64 -w0 < "$S/ns/sops-age";;
  "delete namespace flux-system --wait")
    rm -rf "$S/ns"; echo 'namespace "flux-system" deleted';;
  *) echo "unexpected: kubectl $*" >&2; exit 98;;
esac
`;

/** age-keygen: a fixed identity, its public key on stderr, and -y that derives it back. */
const FAKE_AGE_KEYGEN = `#!/bin/bash
if [ "$1" = -y ]; then grep -q '^AGE-SECRET-KEY-1FAKE$' && { echo age1fake; exit 0; }; echo "no identities found" >&2; exit 1; fi
echo "# public key: age1fake"
echo "AGE-SECRET-KEY-1FAKE"
echo "Public key: age1fake" >&2
`;
const FAILING_AGE_KEYGEN = "#!/bin/bash\nexit 1\n";
/** Prints an identity whose public key is not the one it announces. */
const LYING_AGE_KEYGEN = `#!/bin/bash
if [ "$1" = -y ]; then cat > /dev/null; echo age1other; exit 0; fi
echo "AGE-SECRET-KEY-1FAKE"
echo "Public key: age1fake" >&2
`;

const work = mkdtempSync(join(tmpdir(), "trustiot-readme-commands-"));
afterAll(() => rmSync(work, { recursive: true, force: true }));

interface Scenario {
  keygen?: string;
  owner?: string;
  wrongHash?: boolean;
  noRuntimeDir?: boolean;
  /** An existing flux-system with someone else's sops-age. */
  existing?: boolean;
}

function run(name: string, scenario: Scenario) {
  const root = join(work, name);
  const dirs = { home: join(root, "home"), run: join(root, "run"), state: join(root, "state"), bin: join(root, "bin") };
  for (const dir of Object.values(dirs)) mkdirSync(dir, { recursive: true });
  writeFileSync(join(dirs.bin, "sudo"), FAKE_SUDO, { mode: 0o755 });
  writeFileSync(join(dirs.state, "calls"), "");
  mkdirSync(join(dirs.home, "e2e.lock"));
  writeFileSync(join(dirs.home, "e2e.lock", "owner"), scenario.owner ?? `Laysi/trustiot-v2#${ISSUE} 2026-10-20T08:00:00Z\n`);
  if (scenario.existing) {
    mkdirSync(join(dirs.state, "ns"));
    writeFileSync(join(dirs.state, "ns", "sops-age"), "AGE-SECRET-KEY-1SOMEONEELSE\n");
  }
  const local = join(root, "age-keygen");
  writeFileSync(local, scenario.keygen ?? FAKE_AGE_KEYGEN);
  chmodSync(local, 0o755);
  const hash = scenario.wrongHash ? "0".repeat(64) : new Bun.CryptoHasher("sha256").update(readFileSync(local)).digest("hex");
  const env: Record<string, string> = { PATH: `${dirs.bin}:/usr/bin:/bin`, HOME: dirs.home, FAKE_STATE: dirs.state };
  if (!scenario.noRuntimeDir) env.XDG_RUNTIME_DIR = dirs.run;
  const { upload, keygen } = remoteBlocks();
  const fill = (script: string) => script.replaceAll("<issue>", ISSUE).replaceAll("<雜湊>", hash);
  const uploaded = Bun.spawnSync(["bash", "-c", fill(upload)], { env, cwd: dirs.home, stdin: Bun.file(local), stdout: "pipe", stderr: "pipe" });
  const generated =
    uploaded.exitCode === 0 ? Bun.spawnSync(["bash", "-c", fill(keygen)], { env, cwd: dirs.home, stdout: "pipe", stderr: "pipe" }) : undefined;
  const secret = join(dirs.state, "ns", "sops-age");
  return {
    upload: uploaded.exitCode,
    keygen: generated?.exitCode,
    output: `${uploaded.stderr.toString()}${generated?.stdout.toString() ?? ""}${generated?.stderr.toString() ?? ""}`,
    namespace: existsSync(join(dirs.state, "ns")),
    secret: existsSync(secret) ? readFileSync(secret, "utf8") : undefined,
    leftInRuntimeDir: existsSync(dirs.run) ? readdirSync(dirs.run) : [],
    calls: readFileSync(join(dirs.state, "calls"), "utf8").trim().split("\n").filter(Boolean),
  };
}

describe("the demo cluster's age key commands in the README", () => {
  test(
    "write the key into sops-age, print its public key as derived from the Secret, and leave nothing in the runtime directory",
    () => {
      const result = run("success", {});
      expect(result.upload).toBe(0);
      expect(result.keygen).toBe(0);
      expect(result.secret).toContain("AGE-SECRET-KEY-1FAKE");
      expect(result.output).toContain("Public key: age1fake");
      expect(result.leftInRuntimeDir).toEqual([]);
    },
    SCENARIO_TIMEOUT_MS,
  );

  test(
    "stop before anything when the lock is someone else's, even #4640 for #464, or the runtime directory is not set",
    () => {
      for (const [name, scenario] of [
        ["other-lock", { owner: "Laysi/trustiot-v2#4640 2026-10-20T08:00:00Z\n" }],
        ["no-runtime-dir", { noRuntimeDir: true }],
      ] as const) {
        const result = run(name, scenario);
        expect(result.upload).not.toBe(0);
        expect(result.keygen).toBeUndefined();
        expect(result.calls).toEqual([]);
        expect(result.namespace).toBe(false);
      }
    },
    SCENARIO_TIMEOUT_MS,
  );

  test(
    "stop before touching the cluster when the uploaded file's hash differs",
    () => {
      const result = run("wrong-hash", { wrongHash: true });
      expect(result.keygen).not.toBe(0);
      expect(result.calls).toEqual([]);
      expect(result.leftInRuntimeDir).toEqual([]);
    },
    SCENARIO_TIMEOUT_MS,
  );

  test(
    "leave an existing flux-system and its sops-age alone",
    () => {
      const result = run("existing", { existing: true });
      expect(result.keygen).not.toBe(0);
      expect(result.calls).toEqual(["create namespace flux-system"]);
      expect(result.secret).toBe("AGE-SECRET-KEY-1SOMEONEELSE\n");
    },
    SCENARIO_TIMEOUT_MS,
  );

  test(
    "remove the namespace they created when age-keygen fails or the Secret does not give back the printed key",
    () => {
      for (const [name, keygen] of [
        ["keygen-fails", FAILING_AGE_KEYGEN],
        ["keygen-lies", LYING_AGE_KEYGEN],
      ] as const) {
        const result = run(name, { keygen });
        expect(result.keygen).not.toBe(0);
        expect(result.namespace).toBe(false);
        expect(result.secret).toBeUndefined();
        expect(result.calls.at(-1)).toBe("delete namespace flux-system --wait");
        expect(result.output).not.toContain("Public key: age1other");
        expect(result.leftInRuntimeDir).toEqual([]);
      }
    },
    SCENARIO_TIMEOUT_MS,
  );
});
