// The boot test's look at the states a power cut inside the setup can leave (Laysi/trustiot-v2#918):
// boot-test/classify.sh runs the image's own setup on a state and reports its decision
// (src/boot-results.ts parseSetupRun), and the decisions become the boots that must confirm them
// (outcomeKey, outcomeBoots). Here classify.sh runs the repository's setup with stand-ins for
// blkid, cmp, cryptsetup and systemd-cryptenroll, which can also write to the partition file as the
// real ones do when they repair a metadata copy; that the real tools answer the same way on the
// rebuilt partitions is the boot test's part.
import { afterAll, describe, expect, test } from "bun:test";
import { chmodSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { type Owners, type SetupRun, type SetupRunOn, WIPED, bootRuns, outcomeBoots, outcomeKey, parseSetupRun, refusalLogs, wipeMark } from "../src/boot-results.ts";
import { REPO } from "../src/host.ts";
import { DATA_CHECK, LUKS } from "../src/pins.ts";

const A = `000b${"a1".repeat(32)}`;
const B = `000b${"b2".repeat(32)}`;
const C = `000b${"c3".repeat(32)}`;

const dir = mkdtempSync(join(tmpdir(), "trustiot-classify-test-"));
afterAll(() => rmSync(dir, { recursive: true, force: true }));

/** The setup as the boot test hands it to classify.sh: its FACTORY_FLAG is a file of the worker's own. */
const FLAG = join(dir, "flag");
const SETUP = join(dir, "setup");
{
  const text = readFileSync(join(REPO, "edge/image/files/data/trustiot-data-setup"), "utf8");
  const line = `readonly FACTORY_FLAG=${DATA_CHECK.factoryFlag}`;
  if (text.split("\n").filter((l) => l === line).length !== 1) throw new Error(`the setup does not have the line ${line} exactly once`);
  // The test runs unprivileged: the setup's scratch directories go to the test's directory, not /run.
  writeFileSync(SETUP, text.replace(line, `readonly FACTORY_FLAG=${FLAG}`).replaceAll("/run/trustiot-data-setup.", `${dir}/scratch.`));
}

/**
 * Stand-ins for the programs the setup reads the partition with; each answers from its environment
 * variable. STAND_IN_ENROLL_WRITES=1 has systemd-cryptenroll, and STAND_IN_ISLUKS_WRITES=1 has
 * cryptsetup isLuks (classify.sh's stand-in for udisks2), write to the partition file, as libcryptsetup
 * does when it repairs one metadata copy from the other.
 */
const BIN = join(dir, "bin");
mkdirSync(BIN);
const standIns: Record<string, string> = {
  blkid: 'if [[ -z ${STAND_IN_BLKID-} ]]; then exit 2; fi\nprintf "%s\\n" "$STAND_IN_BLKID"',
  cmp: 'exit "${STAND_IN_CMP:-0}"',
  cryptsetup: [
    'case "$1" in',
    'luksDump) [[ -n ${STAND_IN_DUMP-} ]] || exit 1; printf "%s\\n" "$STAND_IN_DUMP" ;;',
    'token) v="STAND_IN_TOKEN_$4"; [[ -n ${!v-} ]] || exit 1; printf "%s\\n" "${!v}" ;;',
    'isLuks) [[ ${STAND_IN_ISLUKS_WRITES-} != 1 ]] || printf "isLuks\\n" >>"$2"; [[ -n ${STAND_IN_BLKID-} ]] ;;',
    "*) exit 1 ;;",
    "esac",
  ].join("\n"),
  "systemd-cryptenroll": '[[ ${STAND_IN_ENROLL_WRITES-} != 1 ]] || printf "enroll\\n" >>"$1"\n[[ -n ${STAND_IN_ENROLL-} ]] || exit 1\nprintf "%s\\n" "$STAND_IN_ENROLL"',
};
for (const [name, body] of Object.entries(standIns)) {
  writeFileSync(join(BIN, name), `#!/bin/bash\n${body}\n`);
  chmodSync(join(BIN, name), 0o755);
}

const PARTITION = join(dir, "partition");
writeFileSync(PARTITION, "");

/** Runs classify.sh as the boot test does, on an empty partition file, with the stand-ins answering `env`. */
async function classify(env: Record<string, string>): Promise<SetupRun> {
  writeFileSync(PARTITION, "");
  const child = Bun.spawn(["bash", join(REPO, "edge/image/boot-test/classify.sh"), SETUP, PARTITION, FLAG], {
    env: { PATH: `${BIN}:/usr/bin:/bin`, ...env },
    stdout: "pipe",
    stderr: "pipe",
  });
  const [stdout, stderr, code] = await Promise.all([new Response(child.stdout).text(), new Response(child.stderr).text(), child.exited]);
  expect(code).toBe(0);
  return parseSetupRun(stdout, stderr, PARTITION);
}

const blkid = (label: string, uuid: string = LUKS.uuid) => `DEVNAME=x\nVERSION=2\nUUID=${uuid}\nLABEL=${label}\nTYPE=crypto_LUKS\nUSAGE=crypto`;
const dump = (label: string, tokens: string[]) => `LUKS header information\nVersion:       \t2\nLabel:         \t${label}\n\nTokens:\n${tokens.map((t) => `  ${t}`).join("\n")}\nDigests:\n  0: pbkdf2`;
const ownerJson = (owner: string) => JSON.stringify({ type: LUKS.ownerToken.type, keyslots: [], owner });
const markJson = (sealed: string, found: string) => JSON.stringify({ type: LUKS.markToken.type, keyslots: [], reason: "tpm-owner-changed", sealed_owner: sealed, found_owner: found });
const OWNER = `${LUKS.ownerToken.id}: ${LUKS.ownerToken.type}`;
const MARK = `${LUKS.markToken.id}: ${LUKS.markToken.type}`;
const TPM_ONLY = "SLOT TYPE\n   1 tpm2";

describe("classify.sh: the image's setup decides about one state", () => {
  test("an all-zero partition: set up as on a first boot", async () => {
    expect(await classify({ STAND_IN_CMP: "0" })).toEqual({ decision: "first-boot", mark: "", owner: "", refusal: "", flag: "", changedBy: "nothing" });
  });

  test("a new header: started again, with the mark the header carries", async () => {
    expect(await classify({ STAND_IN_BLKID: blkid(LUKS.labelNew), STAND_IN_DUMP: dump(LUKS.labelNew, ["0: systemd-tpm2", OWNER]) })).toMatchObject({ decision: "start-again", mark: "" });
    const withMark = await classify({
      STAND_IN_BLKID: blkid(LUKS.labelNew),
      STAND_IN_DUMP: dump(LUKS.labelNew, [OWNER, MARK]),
      [`STAND_IN_TOKEN_${LUKS.markToken.id}`]: markJson(A, B),
    });
    expect(withMark).toMatchObject({ decision: "start-again", mark: wipeMark({ before: A, recorded: B }) });
  });

  test("a finished volume: left to the TPM, with its recorded owner and the flag of its mark", async () => {
    const env = { STAND_IN_BLKID: blkid(LUKS.label), STAND_IN_ENROLL: TPM_ONLY, [`STAND_IN_TOKEN_${LUKS.ownerToken.id}`]: ownerJson(A) };
    expect(await classify({ ...env, STAND_IN_DUMP: dump(LUKS.label, ["0: systemd-tpm2", OWNER]) })).toEqual({ decision: "check", mark: "", owner: A, refusal: "", flag: "", changedBy: "nothing" });
    const marked = await classify({ ...env, STAND_IN_DUMP: dump(LUKS.label, [OWNER, MARK]), [`STAND_IN_TOKEN_${LUKS.markToken.id}`]: markJson(A, B) });
    expect(marked).toMatchObject({ decision: "check", owner: A, flag: `reason=tpm-owner-changed;sealed_owner=${A};found_owner=${B};` });
  });

  test("refusals, with the partition's path left out, and the flag when the mark cannot be read", async () => {
    // blkid reads the new label, cryptsetup cannot read the header (a torn copy, the other gone).
    const torn = await classify({ STAND_IN_BLKID: blkid(LUKS.labelNew) });
    expect(torn).toMatchObject({ decision: "refused", flag: "reason=unknown;sealed_owner=unknown;found_owner=unknown;" });
    expect(torn.refusal).toBe(`blkid reads <partition>'s label as ${LUKS.labelNew}, but cryptsetup does not: the header's two copies differ; not formatting it`);
    // blkid reads what a torn label leaves.
    const label = await classify({ STAND_IN_BLKID: blkid("trustiot-data-n") });
    expect(label).toMatchObject({ decision: "refused", flag: "" });
    expect(label.refusal).toBe(`<partition> holds crypto_LUKS (UUID ${LUKS.uuid}, label trustiot-data-n), not this image's data volume; not formatting it`);
    expect(await classify({ STAND_IN_CMP: "1" })).toMatchObject({ decision: "refused", refusal: "<partition> has no known format but is not empty; not formatting it" });
    // A finished volume with a password keyslot.
    expect(await classify({ STAND_IN_BLKID: blkid(LUKS.label), STAND_IN_ENROLL: "SLOT TYPE\n   0 password\n   1 tpm2" })).toMatchObject({ decision: "refused", refusal: "<partition> has a keyslot other than the TPM's" });
  });

  test("a finished volume whose mark cannot be read is refused after the TPM, with every field of the flag unknown", async () => {
    const r = await classify({ STAND_IN_BLKID: blkid(LUKS.label), STAND_IN_ENROLL: TPM_ONLY, [`STAND_IN_TOKEN_${LUKS.ownerToken.id}`]: ownerJson(A) });
    expect(r).toMatchObject({ decision: "refused", owner: A, flag: "reason=unknown;sealed_owner=unknown;found_owner=unknown;" });
  });

  test("what wrote the header area: the setup's own tools, or udisks2 later in the boot", async () => {
    const finished = { STAND_IN_BLKID: blkid(LUKS.label), STAND_IN_ENROLL: TPM_ONLY, STAND_IN_DUMP: dump(LUKS.label, ["0: systemd-tpm2", OWNER]), [`STAND_IN_TOKEN_${LUKS.ownerToken.id}`]: ownerJson(A) };
    // tpm_only's systemd-cryptenroll repairs a copy before the TPM opens the volume.
    expect(await classify({ ...finished, STAND_IN_ENROLL_WRITES: "1" })).toMatchObject({ decision: "check", owner: A, changedBy: "setup" });
    // udisks2 reads every LUKS partition after the setup, also one the setup refused.
    const torn = { STAND_IN_BLKID: blkid("trustiot-data-n"), STAND_IN_ISLUKS_WRITES: "1" };
    expect(await classify(torn)).toMatchObject({ decision: "refused", changedBy: "udisks2" });
    expect(await classify({ ...finished, STAND_IN_ENROLL_WRITES: "1", STAND_IN_ISLUKS_WRITES: "1" })).toMatchObject({ decision: "check", changedBy: "both" });
    // A boot that formats writes a new header: what udisks2 would do to the old one does not matter.
    const again = await classify({ STAND_IN_BLKID: blkid(LUKS.labelNew), STAND_IN_DUMP: dump(LUKS.labelNew, ["0: systemd-tpm2", OWNER]), STAND_IN_ISLUKS_WRITES: "1" });
    expect(again).toMatchObject({ decision: "start-again", changedBy: "nothing" });
    // udisks2 reads only what blkid finds LUKS on.
    expect(await classify({ STAND_IN_CMP: "1", STAND_IN_ISLUKS_WRITES: "1" })).toMatchObject({ decision: "refused", changedBy: "nothing" });
  });

  test("output it does not know is an error, not a decision", () => {
    expect(() => parseSetupRun("code=1\nflag=\nchanged_by=nothing\n", "set -e stopped it\n", PARTITION)).toThrow("classify.sh printed");
    expect(() => parseSetupRun("decision=format\nmark=\ncode=0\nflag=\nchanged_by=nothing\n", "neither of the known lines\n", PARTITION)).toThrow("classify.sh printed");
    expect(() => parseSetupRun("decision=check\nowner=\ncode=0\nchanged_by=nothing\n", "", PARTITION)).toThrow("classify.sh printed");
    // Without what wrote the header area, or with a writer it does not know.
    expect(() => parseSetupRun("decision=check\nowner=\ncode=0\nflag=\n", "", PARTITION)).toThrow("classify.sh printed");
    expect(() => parseSetupRun("decision=check\nowner=\ncode=0\nflag=\nchanged_by=someone\n", "", PARTITION)).toThrow("classify.sh printed");
    expect(parseSetupRun("decision=check\nowner=\ncode=0\nflag=\nchanged_by=udisks2\n", "", PARTITION)).toMatchObject({ decision: "check", changedBy: "udisks2" });
  });
});

describe("what a refused boot's journal must show", () => {
  test("the refusal without the partition's path", () => {
    expect(refusalLogs("blkid reads <partition>'s label as trustiot-data-new, but cryptsetup does not: the header's two copies differ; not formatting it")).toBe(
      "'s label as trustiot-data-new, but cryptsetup does not: the header's two copies differ; not formatting it",
    );
  });

  test("and without what blkid read of a torn header, whatever bytes it holds", () => {
    for (const label of ["trustiot-data-n", "trust), (UUID x", "^@M-x"]) {
      expect(refusalLogs(`<partition> holds crypto_LUKS (UUID ${LUKS.uuid}, label ${label}), not this image's data volume; not formatting it`)).toBe(
        ", not this image's data volume; not formatting it",
      );
    }
  });
});

const run = (r: Partial<SetupRunOn> & Pick<SetupRunOn, "decision">): SetupRunOn => ({ mark: "", owner: "", refusal: "", flag: "", changed: false, changedBy: r.changed ? "setup" : "nothing", ...r });
const FIRST: Owners = { recorded: A, before: null };
const WIPE: Owners = { recorded: B, before: A };
const UNKNOWN = "reason=unknown;sealed_owner=unknown;found_owner=unknown;";
const wipeFlag = `reason=tpm-owner-changed;sealed_owner=${A};found_owner=${B};`;
const TORN = "blkid reads <partition>'s label as trustiot-data-new, but cryptsetup does not: the header's two copies differ; not formatting it";

describe("the boots that confirm a state's decision", () => {
  test("set up again, or opened by the TPM: one boot that unlocks the volume", () => {
    for (const [runs, logs] of [
      [[run({ decision: "first-boot" })], "is empty: first boot"],
      [[run({ decision: "start-again" })], "starting again"],
      [[run({ decision: "check", owner: A })], "is encrypted and opens only with the TPM"],
    ] as const) {
      const o = outcomeBoots(runs, "first-boot", FIRST);
      expect(o.problems).toEqual([]);
      expect(o.stuck).toBe(false);
      expect(o.boots).toHaveLength(1);
      expect(o.boots[0]?.expect).toMatchObject({ volume: "unlocked", setup: "success" });
      expect(o.boots[0]?.expect.mark ?? false).toBe(false);
      expect(o.boots[0]?.expect.setupLogs).toContain(logs);
    }
  });

  test("a refusal that leaves the partition as it was: one locked boot, and fail closed for good", () => {
    const o = outcomeBoots([run({ decision: "refused", refusal: TORN, flag: UNKNOWN })], "first-boot", FIRST);
    expect(o).toEqual({
      boots: [{ what: "refuses the partition and leaves it as it was", expect: { volume: "locked", setup: "failed", setupLogs: refusalLogs(TORN), markUnknown: true } }],
      problems: [],
      stuck: true,
    });
  });

  test("a refusal in which one copy was written over the other: the next boot decides again", () => {
    const o = outcomeBoots([run({ decision: "refused", refusal: TORN, flag: UNKNOWN, changed: true }), run({ decision: "start-again" })], "first-boot", FIRST);
    expect(o.stuck).toBe(false);
    expect(o.problems).toEqual([]);
    expect(o.boots.map((b) => b.expect.volume)).toEqual(["locked", "unlocked"]);
    // One that keeps changing the header is a problem.
    const changing = Array.from({ length: 3 }, () => run({ decision: "refused", refusal: TORN, changed: true }));
    expect(outcomeBoots(changing, "first-boot", FIRST).problems).toEqual(["still changing the header after 3 runs"]);
  });

  const M = "0123456789abcdef0123456789abcdef";

  test("the TPM opens a volume whose header changes in the boot: the agent ran on it, and the next boot must keep what it wrote", () => {
    // Laysi/trustiot-v2#918: blkid read the finished label from the copy whose checksum fails, the
    // setup's systemd-cryptenroll wrote the other copy (still the new label) over it, and the next
    // boot starts the setup again.
    const lost = [run({ decision: "check", owner: A, changed: true }), run({ decision: "start-again" })];
    const o = outcomeBoots(lost, "first-boot", FIRST, M);
    expect(o.boots.map((b) => b.what)).toEqual(["the TPM opens the volume, and the setup writes one metadata copy over the other", "starts the setup again"]);
    expect(o.boots[0]?.expect).toMatchObject({ volume: "unlocked", setupLogs: "is encrypted and opens only with the TPM", writeMarker: M });
    expect(o.boots[1]?.expect).toMatchObject({ volume: "unlocked", markerGone: true });
    expect(o.problems).toEqual([
      "the TPM opened the volume and the agent ran on it, but the setup wrote one metadata copy over the other in that boot, and the next boot takes the header for a setup that did not finish and formats the volume: whatever the agent wrote is lost",
    ]);
    expect(o.stuck).toBe(false);
    expect(outcomeKey(lost, FIRST)).toBe("a finished volume of the recording's TPM; the setup wrote one metadata copy over the other -> started again");
    // The problems do not depend on the marker; without one, no boot writes or looks for it.
    const without = outcomeBoots(lost, "first-boot", FIRST);
    expect(without.problems).toEqual(o.problems);
    expect(without.boots[0]?.expect.writeMarker).toBeUndefined();
    // A refusal on the next boot locks what the agent wrote away.
    const refused = outcomeBoots([run({ decision: "check", owner: A, changed: true, changedBy: "udisks2" }), run({ decision: "refused", refusal: TORN, flag: UNKNOWN })], "first-boot", FIRST, M);
    expect(refused.stuck).toBe(true);
    expect(refused.problems).toEqual([
      "the TPM opened the volume and the agent ran on it, but udisks2 wrote one metadata copy over the other in that boot, and the next boot refuses the partition: what the agent wrote stays locked away until the factory sets the partition up",
    ]);
  });

  test("a repair that keeps the volume finished: the next boot opens it and reads back what the agent wrote", () => {
    const kept = [run({ decision: "check", owner: A, changed: true, changedBy: "udisks2" }), run({ decision: "check", owner: A })];
    const o = outcomeBoots(kept, "start-again", FIRST, M);
    expect(o.problems).toEqual([]);
    expect(o.boots.map((b) => b.expect)).toEqual([
      { volume: "unlocked", setup: "success", setupLogs: "is encrypted and opens only with the TPM", mark: false, writeMarker: M },
      { volume: "unlocked", setup: "success", setupLogs: "is encrypted and opens only with the TPM", mark: false, readMarker: M },
    ]);
    expect(outcomeKey(kept, FIRST)).toBe("a finished volume of the recording's TPM; udisks2 wrote one metadata copy over the other -> a finished volume of the recording's TPM");
    // One that keeps changing the header is a problem.
    const changing = Array.from({ length: 3 }, () => run({ decision: "check", owner: A, changed: true }));
    expect(outcomeBoots(changing, "first-boot", FIRST, M).problems).toEqual(["still changing the header after 3 runs"]);
  });

  test("after a wipe the agent stays off: a changed header loses nothing it wrote", () => {
    const o = outcomeBoots([run({ decision: "check", owner: B, flag: wipeFlag, changed: true }), run({ decision: "start-again", mark: wipeMark(WIPE) })], "wipe", WIPE, M);
    expect(o.problems).toEqual([]);
    expect(o.boots[0]?.expect).toMatchObject({ volume: "unlocked", mark: true });
    expect(o.boots[0]?.expect.writeMarker).toBeUndefined();
    expect(o.boots[1]?.expect.markerGone).toBeUndefined();
  });

  test("the boots end with the run that ends what the cut started", () => {
    const later = run({ decision: "refused", refusal: TORN });
    expect(bootRuns([run({ decision: "check", owner: A, changed: true }), run({ decision: "start-again", changed: true }), later], FIRST)).toHaveLength(2);
    expect(bootRuns([run({ decision: "check", owner: A }), later], FIRST)).toHaveLength(1);
    expect(bootRuns([run({ decision: "refused", refusal: TORN, changed: true }), run({ decision: "check", owner: A }), later], FIRST)).toHaveLength(2);
    // A volume of another TPM ends it: wiped again after a wipe, a problem otherwise.
    expect(bootRuns([run({ decision: "check", owner: A, changed: true }), later], WIPE)).toHaveLength(1);
  });

  test("a wipe: the mark is never lost, and the old volume is wiped again", () => {
    expect(outcomeBoots([run({ decision: "start-again", mark: wipeMark(WIPE) })], "wipe", WIPE)).toMatchObject({ problems: [] });
    expect(outcomeBoots([run({ decision: "start-again" })], "wipe", WIPE).problems).toEqual(["started again without the mark after a wipe: the mark is lost"]);
    expect(outcomeBoots([run({ decision: "first-boot" })], "wipe", WIPE).problems).toEqual(["set up as on a first boot after a wipe: the mark is lost"]);
    expect(outcomeBoots([run({ decision: "check", owner: B })], "wipe", WIPE).problems).toEqual(["the volume the wipe made carries no mark: the mark is lost"]);
    const opened = outcomeBoots([run({ decision: "check", owner: B, flag: wipeFlag })], "wipe", WIPE);
    expect(opened.problems).toEqual([]);
    expect(opened.boots[0]?.expect).toMatchObject({ volume: "unlocked", mark: true });
    const old = outcomeBoots([run({ decision: "check", owner: A })], "wipe", WIPE);
    expect(old.problems).toEqual([]);
    expect(old.boots[0]?.expect).toMatchObject({ volume: "unlocked", setupLogs: WIPED, mark: true, tpmLooks: { outcome: "permanent", answers: [] } });
  });

  test("outside a wipe no mark may appear, and only the recording's TPM may hold a finished volume", () => {
    expect(outcomeBoots([run({ decision: "start-again", mark: wipeMark(WIPE) })], "start-again", FIRST).problems).toEqual(["started again with a mark no wipe made"]);
    expect(outcomeBoots([run({ decision: "check", owner: A, flag: wipeFlag })], "first-boot", FIRST).problems).toEqual([`the setup left ${DATA_CHECK.factoryFlag} without a wipe`]);
    expect(outcomeBoots([run({ decision: "check", owner: C })], "first-boot", FIRST).problems).toEqual(["a finished volume of another TPM: the recording knows no such volume"]);
    expect(outcomeBoots([run({ decision: "refused", refusal: TORN, flag: wipeFlag })], "first-boot", FIRST).problems).toHaveLength(1);
  });

  test("states are one class when the setup decides the same about them, whatever blkid read", () => {
    const holds = (label: string) => run({ decision: "refused", refusal: `<partition> holds crypto_LUKS (UUID ${LUKS.uuid}, label ${label}), not this image's data volume; not formatting it` });
    expect(outcomeKey([holds("trustiot-data-n")], FIRST)).toBe(outcomeKey([holds("trustiot-dat")], FIRST));
    expect(outcomeKey([holds("x")], FIRST)).toBe("refused: <partition> holds <what blkid reads>, not this image's data volume; not formatting it");
    expect(outcomeKey([run({ decision: "refused", refusal: TORN, flag: UNKNOWN, changed: true }), run({ decision: "check", owner: A })], WIPE)).toBe(
      `refused: ${TORN} (flag: mark unknown); the setup wrote one metadata copy over the other -> a finished volume of the TPM before the wipe`,
    );
    expect(outcomeKey([run({ decision: "start-again", mark: wipeMark(WIPE) })], WIPE)).toBe("started again with the wipe's mark");
    expect(outcomeKey([run({ decision: "check", owner: B, flag: wipeFlag })], WIPE)).toBe("a finished volume of the recording's TPM (flag: the wipe's mark)");
  });
});
