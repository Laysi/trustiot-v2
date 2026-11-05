// What files/data/trustiot-data-setup decides when the TPM does not open the data volume
// (Laysi/trustiot-v2#867; README.md, section 「TPM 永久解不開時」): its functions, sourced into
// bash with the TPM and cryptsetup replaced by stand-ins, so the decision is tested without a TPM.
// That the stand-ins answer the way the real programs do is the boot test's part.
import { afterAll, describe, expect, test } from "bun:test";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { REPO } from "../src/host.ts";
import { DATA_CHECK, LUKS } from "../src/pins.ts";

const SCRIPT = join(REPO, "edge/image/files/data/trustiot-data-setup");
const dir = mkdtempSync(join(tmpdir(), "trustiot-data-setup-test-"));
afterAll(() => rmSync(dir, { recursive: true, force: true }));

const A = `000b${"a".repeat(64)}`;
const B = `000b${"b".repeat(64)}`;
const C = `000b${"c".repeat(64)}`;

/** Runs `body` in bash after sourcing the setup script (which must not run main() then). */
async function sourced(body: string): Promise<{ stdout: string; stderr: string; code: number }> {
  const child = Bun.spawn(["bash", "-c", `source "$1"\n${body}`, "data-setup-test", SCRIPT], { stdout: "pipe", stderr: "pipe", cwd: dir });
  const [stdout, stderr, code] = await Promise.all([new Response(child.stdout).text(), new Response(child.stderr).text(), child.exited]);
  return { stdout, stderr, code };
}

const quote = (s: string) => `'${s.replaceAll("'", `'\\''`)}'`;

/**
 * check_tpm with observe() answering `answers` in turn (the last one from then on), each look
 * taking `lookS` seconds of a clock that pause() moves on.
 */
async function check(answers: string[], lookS = 0): Promise<{ verdict: string; looks: number; pauses: number[]; at: number[]; log: string }> {
  const r = await sourced(`
answers=(${answers.map(quote).join(" ")})
i=0; clock=0; pauses=(); at=()
observe() { at+=("$clock"); answer=\${answers[i]:-\${answers[-1]}}; i=$((i + 1)); clock=$((clock + ${lookS})); }
uptime_s() { printf '%s\\n' "$clock"; }
pause() { pauses+=("$1"); clock=$((clock + $1)); }
check_tpm /dev/test-data
printf 'verdict=%s\\nlooks=%s\\npauses=%s\\nat=%s\\n' "$verdict" "$i" "\${pauses[*]}" "\${at[*]}"
`);
  if (r.code !== 0) throw new Error(`bash exited with ${r.code}: ${r.stderr}`);
  const v = Object.fromEntries(r.stdout.trim().split("\n").map((l) => [l.slice(0, l.indexOf("=")), l.slice(l.indexOf("=") + 1)]));
  const numbers = (s = "") => (s === "" ? [] : s.split(" ").map(Number));
  return { verdict: v.verdict ?? "", looks: Number(v.looks), pauses: numbers(v.pauses), at: numbers(v.at), log: r.stderr };
}

describe("trustiot-data-setup: sourcing it", () => {
  test("defines the functions and runs nothing", async () => {
    const r = await sourced("declare -F check_tpm observe format main >/dev/null && echo defined");
    expect(r).toEqual({ stdout: "defined\n", stderr: "", code: 0 });
  });

  test("its check constants are the pinned ones", async () => {
    const r = await sourced('printf "%s %s %s %s %s %s %s %s\\n" "$CHECK_INTERVAL_S" "$CHECK_WINDOW_S" "$CHECK_SAME_ANSWERS" "$OWNER_TOKEN_ID" "$OWNER_TOKEN_TYPE" "$MARK_TOKEN_ID" "$MARK_TOKEN_TYPE" "$FACTORY_FLAG"');
    expect(r.stdout.trim().split(" ")).toEqual([
      String(DATA_CHECK.intervalS),
      String(DATA_CHECK.windowS),
      String(DATA_CHECK.sameAnswers),
      LUKS.ownerToken.id,
      LUKS.ownerToken.type,
      LUKS.markToken.id,
      LUKS.markToken.type,
      DATA_CHECK.factoryFlag,
    ]);
  });
});

describe("trustiot-data-setup: how long it looks at the TPM and what it decides", () => {
  const { intervalS, windowS, sameAnswers } = DATA_CHECK;

  test("a TPM that opens the volume at once: no wait", async () => {
    expect(await check(["opens"])).toMatchObject({ verdict: "opens", looks: 1, pauses: [] });
  });

  test("a TPM that shows up late opens the volume within the window: nothing is wiped", async () => {
    const r = await check([...Array(6).fill("absent"), "silent", "opens"]);
    expect(r).toMatchObject({ verdict: "opens", looks: 8 });
    expect(r.pauses).toEqual(Array(7).fill(intervalS));
  });

  test("the same other owner hierarchy, CHECK_SAME_ANSWERS times in a row, is permanent", async () => {
    const r = await check(Array(sameAnswers).fill(`other-owner ${B}`));
    expect(r).toMatchObject({ verdict: `permanent ${B}`, looks: sameAnswers });
    expect(r.at).toEqual(Array.from({ length: sameAnswers }, (_, k) => k * intervalS));
  });

  test("one fewer is not", async () => {
    const r = await check([...Array(sameAnswers - 1).fill(`other-owner ${B}`), "opens"]);
    expect(r.verdict).toBe("opens");
  });

  test("any other answer in between starts the count again", async () => {
    for (const between of ["absent", "silent", "unrecorded", "unknown-owner", "same-owner", `other-owner ${C}`]) {
      const answers = [`other-owner ${B}`, `other-owner ${B}`, between, ...Array(sameAnswers).fill(`other-owner ${B}`)];
      const r = await check(answers);
      expect({ between, ...r }).toMatchObject({ between, verdict: `permanent ${B}`, looks: answers.length });
    }
  });

  test("another owner hierarchy that changes between looks is not counted as one", async () => {
    const r = await check(Array.from({ length: 2 * (windowS / intervalS + 1) }, (_, k) => `other-owner ${k % 2 === 0 ? B : C}`));
    expect(r.verdict).toBe(`unsure other-owner ${B}`);
    expect(r.at.at(-1)).toBe(windowS);
  });

  for (const answer of ["absent", "silent", "unrecorded", "unknown-owner", "same-owner"]) {
    test(`"${answer}" for the whole window: never wiped, the last look at most CHECK_WINDOW_S after the first`, async () => {
      const r = await check([answer]);
      expect(r.verdict).toBe(`unsure ${answer}`);
      expect(r.at[0]).toBe(0);
      expect(r.at.at(-1)).toBe(windowS);
      expect(r.looks).toBe(windowS / intervalS + 1);
      expect(r.pauses).toEqual(Array(r.looks - 1).fill(intervalS));
    });
  }

  test("looks that take time: the window still bounds the last one", async () => {
    const r = await check(["silent"], 7);
    expect(r.verdict).toBe("unsure silent");
    expect(r.at.at(-1)).toBeLessThanOrEqual(windowS);
    expect(r.at.at(-1)).toBeGreaterThan(windowS - intervalS - 7);
  });

  test("the permanent answers must all come within the window", async () => {
    // Absent until the last two looks of the window: two answers are not enough.
    const answers = [...Array(windowS / intervalS - 1).fill("absent"), `other-owner ${B}`, `other-owner ${B}`, `other-owner ${B}`];
    const r = await check(answers);
    expect(r.verdict).toBe(`unsure other-owner ${B}`);
    expect(r.at.at(-1)).toBe(windowS);
  });

  test("every look is in the journal, with the seconds since the first", async () => {
    const r = await check(["absent", "absent", "opens"], 2);
    expect(r.log.split("\n").filter((l) => l.startsWith("the TPM "))).toEqual(["the TPM (look 1, 0 s): absent", `the TPM (look 2, ${intervalS + 2} s): absent`, `the TPM (look 3, ${2 * intervalS + 4} s): opens`]);
  });
});

/**
 * observe() with cryptsetup, tpm2_getcap and the fingerprints replaced: what it answers for each
 * thing the TPM and the header can say.
 */
async function observe(stub: { present?: boolean; opens?: boolean; answers?: boolean; token?: string | null; owner?: string | null; srk?: string | null }): Promise<string> {
  const token = stub.token === undefined ? `{"type":"${LUKS.ownerToken.type}","keyslots":[],"owner":"${A}"}` : stub.token;
  const r = await sourced(`
scratch=${quote(dir)}
tpm_present() { ${stub.present === false ? "false" : "true"}; }
timeout() { shift; "$@"; }
cryptsetup() {
	case "$1 $2" in
	"open --test-passphrase") ${stub.opens ? "return 0" : "return 1"} ;;
	"token export") [[ $4 == ${LUKS.ownerToken.id} ]] && ${token === null ? "return 1" : `printf '%s\\n' ${quote(token)}`} ;;
	*) echo "unexpected cryptsetup $*" >&2; return 99 ;;
	esac
}
tpm2_getcap() { ${stub.answers === false ? "return 1" : "return 0"}; }
owner_fingerprint() { ${stub.owner === null ? "return 1" : `echo ${stub.owner ?? A}`}; }
srk_name() { ${stub.srk ? `echo ${stub.srk}` : "return 1"}; }
observe /dev/test-data
printf '%s\\n' "$answer"
`);
  if (r.code !== 0 || r.stderr !== "") throw new Error(`bash exited with ${r.code}: ${r.stderr}`);
  return r.stdout.trim();
}

describe("trustiot-data-setup: one look at the TPM", () => {
  test("no /dev/tpmrm0", async () => {
    expect(await observe({ present: false })).toBe("absent");
  });

  test("the TPM opens the volume", async () => {
    expect(await observe({ opens: true })).toBe("opens");
  });

  test("the TPM does not answer", async () => {
    expect(await observe({ answers: false })).toBe("silent");
  });

  test("no fingerprint recorded, or not the setup's own token: never a wipe", async () => {
    expect(await observe({ token: null, owner: B })).toBe("unrecorded");
    expect(await observe({ token: `{"type":"systemd-tpm2","keyslots":["1"],"owner":"${A}"}`, owner: B })).toBe("unrecorded");
    expect(await observe({ token: `{"type":"${LUKS.ownerToken.type}","keyslots":[],"owner":"000b${"a".repeat(63)}"}`, owner: B })).toBe("unrecorded");
    expect(await observe({ token: `{"type":"${LUKS.ownerToken.type}","keyslots":[],"owner":"${A}x"}`, owner: B })).toBe("unrecorded");
  });

  test("the fingerprint cannot be made (an owner password, for example)", async () => {
    expect(await observe({ owner: null })).toBe("unknown-owner");
  });

  test("the same owner hierarchy: not a TPM change", async () => {
    expect(await observe({ owner: A })).toBe("same-owner");
  });

  test("the storage key the volume was sealed under is still in the TPM: the same owner hierarchy", async () => {
    expect(await observe({ owner: B, srk: A })).toBe("same-owner");
  });

  test("another owner hierarchy: another TPM, or this one cleared", async () => {
    expect(await observe({ owner: B })).toBe(`other-owner ${B}`);
    expect(await observe({ owner: B, srk: C })).toBe(`other-owner ${B}`);
  });

  test("cryptsetup's JSON with spaces is read the same", async () => {
    expect(await observe({ token: `{ "type": "${LUKS.ownerToken.type}", "keyslots": [ ], "owner": "${A}" }`, owner: B })).toBe(`other-owner ${B}`);
  });
});

describe("trustiot-data-setup: the mark", () => {
  test("is made and read back field by field; a field that is not what the setup writes reads as unknown", async () => {
    const r = await sourced(`
for json in "$(make_mark tpm-owner-changed ${A} ${B})" '{"type":"${LUKS.markToken.type}","keyslots":[],"reason":"Bad;reason","sealed_owner":"${A}\\\\n","found_owner":"x"}' '{"type":"${LUKS.markToken.type}","keyslots":[]}'; do
	mark_fields "$json"
	printf '%s %s %s\\n' "$mark_reason" "$mark_sealed" "$mark_found"
done
`);
    expect(r.stdout.trim().split("\n")).toEqual([`tpm-owner-changed ${A} ${B}`, "unknown unknown unknown", "unknown unknown unknown"]);
  });

  test("only a token of the mark's type is a mark", async () => {
    const r = await sourced(`
cryptsetup() { [[ "$1 $2 $3 $4" == "token export --token-id ${LUKS.markToken.id}" ]] && printf '%s\\n' "$TOKEN"; }
TOKEN='{"type":"other","keyslots":[],"reason":"tpm-owner-changed"}' read_mark /dev/test-data; echo "[other]"
TOKEN="$(make_mark tpm-owner-changed ${A} ${B})" read_mark /dev/test-data
`);
    expect(r.stdout.trim().split("\n")).toEqual(["[other]", `{"type":"${LUKS.markToken.type}","keyslots":[],"reason":"tpm-owner-changed","sealed_owner":"${A}","found_owner":"${B}"}`]);
  });

  test("a name file is read as hex, and only a SHA-256 name is accepted", async () => {
    writeFileSync(join(dir, "good.name"), Buffer.from(A, "hex"));
    writeFileSync(join(dir, "short.name"), Buffer.from(A.slice(0, 66), "hex"));
    writeFileSync(join(dir, "sha1.name"), Buffer.from(`0004${"a".repeat(40)}`, "hex"));
    const r = await sourced(`
for f in good short sha1; do name_hex "${dir}/$f.name" || echo "$f refused"; done
`);
    expect(r.stdout.trim().split("\n")).toEqual([A, "short refused", "sha1 refused"]);
  });
});
